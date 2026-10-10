use super::*;

fn schema_reference(db: &Database, slot: usize) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(0, slot),
        kind: Kind::Schema,
    })
}

/// Constrain a call of `callee` and solve, returning the solver
fn solve_call<'db>(
    db: &'db Database,
    callee: TypeId,
    args: &[TypeId],
    result: Option<TypeId>,
) -> (Solver<'db>, Term, Outcome) {
    let mut s = Solver::new(db);
    let result = match result {
        Some(ty) => s.closed(ty),
        None => s.infer(),
    };
    let args: Vec<_> = args
        .iter()
        .map(|&ty| CallArgument::Positional(s.closed(ty)))
        .collect();
    let callee = s.closed(callee);
    constrain_call(&mut s, callee, &args, result, None);
    let outcome = s.solve().remove(0);
    (s, result, outcome)
}

#[test]
fn generic_callees_are_instantiated_once_per_use() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // id[T] x@T -> T
    let id = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t),
    );
    db.seal();
    let mut s = Solver::new(&db);
    let mut results = Vec::new();
    for arg in [int, str] {
        let result = s.infer();
        let (args, id) = ([CallArgument::Positional(s.closed(arg))], s.closed(id));
        constrain_call(&mut s, id, &args, result, None);
        results.push(result);
    }
    assert!(s.solve().iter().all(|o| o.status == Status::Unresolved));
    // Two results and one binder variable for each call
    assert_eq!(s.bounds.len(), 4);
    assert_eq!(s.instantiations.borrow().len(), 2);
    let mut binders = HashSet::new();
    for (result, arg) in results.into_iter().zip([int, str]) {
        let lower: Vec<_> = s.bounds(variable_id(result)).lower().collect();
        assert!(lower.iter().any(|&term| s.reify(term) == Ok(arg)));
        let [binder] = lower
            .iter()
            .filter_map(|term| match term {
                Term::Infer(id) => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>()[..]
        else {
            panic!("the binder's variable");
        };
        assert!(binders.insert(binder));
    }
}

#[test]
fn invariant_arguments_force_instantiated_binders() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let t = reference(&db, 0, 0);
    // first[T] items@Array[T] -> T
    let first = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[apply(&db, array, &[t])], t),
    );
    let ints = apply(&db, array, &[int]);
    db.seal();
    let (_, _, outcome) = solve_call(&db, first, &[ints], Some(int));
    assert_eq!(outcome.status, Status::Proven);
    let (_, _, outcome) = solve_call(&db, first, &[ints], Some(str));
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));
    let (_, _, outcome) = solve_call(&db, first, &[int], Some(int));
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));

    // Reprocessing the call reuses its variables
    let (mut s, result, outcome) = solve_call(&db, first, &[ints], None);
    assert_eq!(outcome.status, Status::Unresolved);
    let variables = s.bounds.len();
    s.constrain(result, s.closed(int), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.bounds.len(), variables);
    assert_eq!(s.instantiations.borrow().len(), 1);
    assert_eq!(s.reify(result), Ok(int));
}

#[test]
fn instantiated_binders_are_below_their_bounds() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let [t, u] = [0, 1].map(|slot| reference(&db, 0, slot));
    let bounded_by = |bound| Binder {
        bound: Some(bound),
        ..binder(Variance::Invariant)
    };
    // numeric[T @ Num] x@T -> T
    let numeric = quantified(&db, vec![bounded_by(num)], function(&db, &[t], t));
    // sibling[T @ Num, U @ T] x@U -> U
    let sibling = quantified(
        &db,
        vec![bounded_by(num), bounded_by(t)],
        function(&db, &[u], u),
    );
    // bounded[T @ Cmp[T]] x@T -> T
    let cmp = nominal(&mut db, "Cmp", vec![binder(Variance::Invariant)], vec![]);
    let (ord_id, ord, ord_source) = reserve(&mut db, DeclKind::Class, "Ord");
    let cmp_ord = apply(&db, cmp, &[ord]);
    populate(&mut db, ord_id, ord_source, ord, vec![cmp_ord]);
    let f_bounded = quantified(
        &db,
        vec![bounded_by(apply(&db, cmp, &[t]))],
        function(&db, &[t], t),
    );
    db.seal();
    // Nothing forces a variable with only its bound above it
    for (callee, arg, status) in [
        (numeric, int, Status::Unresolved),
        (numeric, str, Status::Contradicted),
        (sibling, int, Status::Unresolved),
        (sibling, str, Status::Contradicted),
        (f_bounded, ord, Status::Proven),
        (f_bounded, int, Status::Contradicted),
    ] {
        let (_, _, outcome) = solve_call(&db, callee, &[arg], Some(db.top()));
        assert_eq!(outcome.status, status, "{callee:?} {arg:?}");
    }
}

#[test]
fn ambient_binders_are_bounded_by_the_callers_channels() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let iter_unknown = apply(&db, iter, &[db.unknown()]);
    let iter_int = apply(&db, iter, &[int]);
    let input = reference(&db, 0, 0);
    let body = db.intern(Type::Function(Function {
        params: schema(&db, &[]),
        result: input,
        input: Some(input),
        output: None,
    }));
    // f[<I @ Iter[Unknown]]() -> I
    let f = quantified(
        &db,
        vec![bounded(Kind::Type, Binding::Implicit, Some(iter_unknown))],
        body,
    );
    db.seal();
    let mut s = Solver::new(&db);
    let result = s.infer();
    let (f, input) = (s.closed(f), s.closed(iter_int));
    constrain_call(&mut s, f, &[], result, Some(input));
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    // The channel's variable defaults to the caller's channel, and the result to it
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    assert_eq!(s.reify(result), Ok(iter_int));
}

#[test]
fn defaults_are_the_join_of_solved_lower_bounds() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let one = fresh(&db, 1);
    let t = reference(&db, 0, 0);
    let f = function(&db, &[int], str);
    let id = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t),
    );
    db.seal();

    // A monomorphic call's result
    let (mut s, result, outcome) = solve_call(&db, f, &[int], None);
    assert_eq!(outcome.status, Status::Unresolved);
    let result = variable_id(result);
    assert_eq!(s.default(result), Ok(str));
    assert!(s.defaulted(result));
    assert_eq!(s.solve()[0].status, Status::Proven);

    // A generic call's binder defaults first, decaying its literal
    let (mut s, result, _) = solve_call(&db, id, &[one], None);
    assert_eq!(s.default(variable_id(result)), Err(Residual::Inference));
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    assert_eq!(s.reify(result), Ok(int));

    // Several lower bounds join, and `Unknown` makes the join dynamic
    let mut s = Solver::new(&db);
    let [joined, dynamic, empty, conflicted] = [(); 4].map(|()| s.infer());
    for (variable, lower) in [
        (joined, int),
        (joined, str),
        (dynamic, int),
        (dynamic, db.unknown()),
        (conflicted, str),
    ] {
        s.constrain(s.closed(lower), variable, Provenance::default());
    }
    s.constrain(conflicted, s.closed(int), Provenance::default());
    s.solve();
    let union = db.intern(Type::Union(
        vec![UnionMember::Type(int), UnionMember::Type(str)].into(),
    ));
    assert_eq!(s.default(variable_id(joined)), Ok(union));
    assert_eq!(s.default(variable_id(dynamic)), Ok(db.unknown()));
    assert_eq!(s.default(variable_id(empty)), Err(Residual::Inference));
    assert_eq!(
        s.default(variable_id(conflicted)),
        Err(Residual::Unsupported(
            "a default above a variable's upper bounds"
        ))
    );
    assert_eq!(s.solution(variable_id(conflicted)), None);
    assert!(!s.defaulted(variable_id(conflicted)));
}

#[test]
fn defaults_decay_literals() {
    let mut db = Database::new();
    let sym = db.intern_symbol("a");
    let mut kinds = vec![];
    for (intrinsic, literal) in [
        (Intrinsic::Nil, Literal::Nil),
        (Intrinsic::Bool, Literal::Bool(true)),
        (Intrinsic::Int, Literal::Int(1)),
        (Intrinsic::Str, Literal::Str("a".into())),
        (Intrinsic::Sym, Literal::Sym(sym)),
    ] {
        let class = nominal(&mut db, &format!("{intrinsic:?}"), vec![], vec![]);
        db.set_intrinsic(intrinsic, class);
        kinds.push((db.intern(Type::Fresh(literal)), class));
    }
    let int = db.intrinsic(Intrinsic::Int).unwrap();
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let t = reference(&db, 0, 0);
    let pair = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t, t], apply(&db, array, &[t])),
    );
    let one = fresh(&db, 1);
    let two = fresh(&db, 2);
    let exact = literal(&db, 1);
    // What was written in a type, as an existing value's is
    let one_two = db.intern(Type::Union(
        vec![UnionMember::Type(exact), UnionMember::Type(literal(&db, 2))].into(),
    ));
    let array_one_two = apply(&db, array, &[one_two]);
    db.seal();

    // Each kind of fresh literal decays to its class
    for (literal, class) in kinds {
        let mut s = Solver::new(&db);
        let variable = s.infer();
        s.constrain(s.closed(literal), variable, Provenance::default());
        s.solve();
        assert_eq!(s.default(variable_id(variable)), Ok(class));
    }

    // Literals decay where the default fixes an invariant argument
    let (mut s, result, _) = solve_call(&db, pair, &[one, two], None);
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    assert_eq!(s.reify(result), Ok(apply(&db, array, &[int])));

    let mut s = Solver::new(&db);
    let [required, existing, mixed, written] = [(); 4].map(|()| s.infer());
    for (variable, lower) in [
        (required, one),
        (existing, array_one_two),
        (mixed, one),
        (mixed, int),
        (written, exact),
    ] {
        s.constrain(s.closed(lower), variable, Provenance::default());
    }
    s.constrain(required, s.closed(one_two), Provenance::default());
    s.solve();
    // An upper bound can require the literal
    assert_eq!(s.default(variable_id(required)), Ok(one));
    // An existing value's invariant argument can't widen
    assert_eq!(s.default(variable_id(existing)), Ok(array_one_two));
    assert_eq!(s.default(variable_id(mixed)), Ok(int));
    // A literal written in a type never decays
    assert_eq!(s.default(variable_id(written)), Ok(exact));

    // A default that wouldn't lock the literal in keeps it
    let mut s = Solver::new(&db);
    let variable = s.infer();
    s.constrain(s.closed(one), variable, Provenance::default());
    s.solve();
    assert_eq!(s.default_with(variable_id(variable), false), Ok(one));
}

#[test]
fn literals_without_a_class_are_kept() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    s.constrain(s.closed(one), variable, Provenance::default());
    s.solve();
    assert_eq!(s.default(variable_id(variable)), Ok(one));
}

#[test]
fn pack_binders_take_the_remaining_arguments() {
    use Multiplicity::Required as Req;
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let ps = schema_reference(&db, 0);
    let a = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("a"))));
    let params = items(&db, vec![positional(Req, int), include(Req, ps)]);
    // pack[*Ps] first@Int *rest@...Ps -> nil
    let pack = quantified(
        &db,
        vec![bounded(Kind::Schema, Binding::Rest(Rest::Positional), None)],
        db.intern(Type::Function(Function {
            params,
            result: db.top(),
            input: None,
            output: None,
        })),
    );
    let keyed_params = items(&db, vec![keyed(Req, a, int), include(Req, ps)]);
    // options[**Ks] :a@Int **rest@...Ks -> nil
    let options = quantified(
        &db,
        vec![bounded(Kind::Schema, Binding::Rest(Rest::Keyed), None)],
        db.intern(Type::Function(Function {
            params: keyed_params,
            result: db.top(),
            input: None,
            output: None,
        })),
    );
    let rest = schema(&db, &[str, int]);
    let b = db.intern_symbol("b");
    let b_key = db.intern(Type::Literal(Literal::Sym(b)));
    let named_rest = items(&db, vec![keyed(Req, b_key, str)]);
    db.seal();
    let lower_schema = |s: &Solver<'_>| {
        let variable = (0..s.bounds.len())
            .map(InferVarId)
            .find(|&id| s.inference[id.0].kind == Kind::Schema)
            .unwrap();
        let lower: Vec<_> = s.bounds(variable).lower().collect();
        lower
            .into_iter()
            .map(|term| s.reify(term))
            .collect::<Vec<_>>()
    };
    let (mut s, _, outcome) = solve_call(&db, pack, &[int, str, int], Some(db.top()));
    assert_eq!(outcome.status, Status::Unresolved);
    assert_eq!(lower_schema(&s), vec![Ok(rest)]);
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    let (_, _, outcome) = solve_call(&db, pack, &[], Some(db.top()));
    assert!(contradiction(&outcome, Contradiction::Missing(0)));
    let (_, _, outcome) = solve_call(&db, pack, &[str], Some(db.top()));
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));

    let mut s = Solver::new(&db);
    let [int_term, str_term] = [int, str].map(|ty| s.closed(ty));
    let a_name = db.intern_symbol("a");
    let args = [
        CallArgument::Keyword(b, str_term),
        CallArgument::Keyword(a_name, int_term),
    ];
    let (options, top) = (s.closed(options), s.closed(db.top()));
    constrain_call(&mut s, options, &args, top, None);
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    assert_eq!(lower_schema(&s), vec![Ok(named_rest)]);
}

#[test]
fn closed_solvers_settle_their_own_variables() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // id[T] x@T -> T
    let id = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t),
    );
    db.seal();
    let judge = |expected: TypeId, close: bool| {
        let mut s = Solver::new(&db);
        if close {
            s.close();
        }
        s.constrain(s.closed(id), s.closed(expected), Provenance::default());
        let status = s.solve().remove(0).status;
        (status, s.unresolved().count())
    };
    // An instantiation's variables are the caller's to default
    let loose = function(&db, &[int], db.top());
    assert_eq!(judge(loose, false), (Status::Unresolved, 1));
    // A closed solver defaults them itself
    assert_eq!(judge(loose, true), (Status::Proven, 0));
    let crossed = function(&db, &[int], str);
    assert_eq!(judge(crossed, true).0, Status::Contradicted);
}

#[test]
fn instantiation_establishes_implied_bounds() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let s = items(&db, vec![keyed(Multiplicity::Repeated, str, int)]);
    let k = reference(&db, 0, 0);
    // get[K] key@K -> IndexItem[{*(Str): Int}, K]
    let get = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[k], selecting(&db, false, s, k)),
    );
    db.seal();
    // Even where nothing evaluates the projection, a key outside `Keys[S]`
    // is contradicted
    for (key, status) in [(str, Status::Proven), (int, Status::Contradicted)] {
        let (_, _, outcome) = solve_call(&db, get, &[key], Some(db.top()));
        assert_eq!(outcome.status, status, "{key:?}: {outcome:?}");
    }
}
