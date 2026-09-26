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
    let expected = s.call(&args, result, None, None);
    s.constrain(s.closed(callee), expected, Provenance::default());
    let outcome = s.solve().remove(0);
    (s, result, outcome)
}

/// Default every variable whose lower bounds are solved, repeatedly, solving
/// between rounds, as a flow driver would
fn default_all(s: &mut Solver<'_>) -> Vec<Outcome> {
    loop {
        let unsolved: Vec<_> = s.unresolved().collect();
        let progress = unsolved
            .into_iter()
            .filter(|&id| s.default(id).is_ok())
            .count();
        let outcomes = s.solve();
        if progress == 0 {
            return outcomes;
        }
    }
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
        let expected = s.call(
            &[CallArgument::Positional(s.closed(arg))],
            result,
            None,
            None,
        );
        s.constrain(s.closed(id), expected, Provenance::default());
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
                Term::View(_) => None,
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
    db.set_intrinsic(Intrinsic::Iter, iter);
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
    let expected = s.call(&[], result, Some(s.closed(iter_int)), None);
    s.constrain(s.closed(f), expected, Provenance::default());
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
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let one = literal(&db, 1);
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

    // A generic call's binder defaults first; literals are not widened here
    let (mut s, result, _) = solve_call(&db, id, &[one], None);
    assert_eq!(s.default(variable_id(result)), Err(Residual::Inference));
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    assert_eq!(s.reify(result), Ok(one));

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
        Err(Residual::Unsupported)
    );
    assert_eq!(s.solution(variable_id(conflicted)), None);
    assert!(!s.defaulted(variable_id(conflicted)));
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
    let expected = s.call(
        &[
            CallArgument::Keyword(b, str_term),
            CallArgument::Keyword(a_name, int_term),
        ],
        s.closed(db.top()),
        None,
        None,
    );
    s.constrain(s.closed(options), expected, Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    assert_eq!(lower_schema(&s), vec![Ok(named_rest)]);
}
