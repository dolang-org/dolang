use super::*;

/// Whether any obligation's current premises include `step`
fn uses(s: &Solver<'_>, step: &Step) -> bool {
    (s.obligations.iter()).any(|o| o.active.borrow().iter().any(|(_, found)| found == step))
}

#[test]
fn skolems_are_bounded_by_their_binders() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let unknown = db.unknown();
    let bottom = db.bottom();
    let t = reference(&db, 0, 0);
    // [T @ Num] (T) -> Int
    let bounded_t = quantified(
        &db,
        vec![bounded(Kind::Type, Binding::Positional, Some(num))],
        function(&db, &[t], int),
    );
    // [T] (T) -> T
    let identity = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t),
    );
    let top = db.top();
    db.seal();
    for (actual, expected) in [
        (function(&db, &[num], int), bounded_t),
        (function(&db, &[top], bottom), identity),
        (function(&db, &[unknown], unknown), identity),
        (identity, identity),
    ] {
        let outcome = check(&db, actual, expected);
        assert_eq!(outcome.status, Status::Proven, "{actual:?} <: {expected:?}");
    }
    // The bound is all that is known of `T`
    let outcome = check(&db, function(&db, &[int], int), bounded_t);
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));
    // Nothing but itself, bottom and `Unknown` is below `T`
    let outcome = check(&db, function(&db, &[top], int), identity);
    assert!(contradiction(&outcome, Contradiction::Rigid));
    // An unbounded `T` is below nothing but itself, top and `Unknown`
    let outcome = check(&db, function(&db, &[int], bottom), identity);
    assert!(contradiction(&outcome, Contradiction::Rigid));
}

#[test]
fn skolem_bounds_are_read_in_their_environment() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let t = reference(&db, 0, 0);
    let box_t = apply(&db, boxed, &[t]);
    // [T @ Box[T]] (T) -> Int
    let expected = quantified(
        &db,
        vec![bounded(Kind::Type, Binding::Positional, Some(box_t))],
        function(&db, &[t], int),
    );
    let box_top = apply(&db, boxed, &[db.top()]);
    let box_int = apply(&db, boxed, &[int]);
    db.seal();
    let outcome = check(&db, function(&db, &[box_top], int), expected);
    assert_eq!(outcome.status, Status::Proven);
    // `T <: Box[T] <: Box[Int]` needs `T <: Int`, but `T` is only a `Box`
    let outcome = check(&db, function(&db, &[box_int], int), expected);
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));
}

#[test]
fn unbounded_skolems_are_below_unions_admitting_anything() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let expected = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], int),
    );
    let union = |db: &Database, members: &[TypeId]| {
        db.intern(Type::Union(
            members.iter().copied().map(UnionMember::Type).collect(),
        ))
    };
    let with_top = union(&db, &[int, db.top()]);
    let with_unknown = union(&db, &[str, db.unknown()]);
    let neither = union(&db, &[int, str]);
    db.seal();
    for param in [with_top, with_unknown] {
        let outcome = check(&db, function(&db, &[param], int), expected);
        assert_eq!(outcome.status, Status::Proven);
    }
    let outcome = check(&db, function(&db, &[neither], int), expected);
    assert!(contradiction(&outcome, Contradiction::Rigid));
}

#[test]
fn skolem_bound_reductions_are_labeled() {
    let mut db = Database::new();
    let iter = nominal(&mut db, "Iter", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let body = function(&db, &[t], int);
    db.seal();
    for (binding, step) in [
        (Binding::Positional, Step::SkolemBound),
        (Binding::Implicit, Step::ImplicitBound),
    ] {
        let expected = quantified(&db, vec![bounded(Kind::Type, binding, Some(iter))], body);
        let mut s = Solver::new(&db);
        let actual = function(&db, &[iter], int);
        s.constrain(s.closed(actual), s.closed(expected), Provenance::default());
        assert_eq!(s.solve()[0].status, Status::Proven);
        assert!(uses(&s, &Step::Skolemization));
        assert!(uses(&s, &step));
    }
}

#[test]
fn rest_skolems_are_bounded_by_their_shapes() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let top = db.top();
    let rest = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    });
    let params = |db: &Database, rest| items(db, vec![include(Multiplicity::Required, rest)]);
    let function_of = |db: &Database, params| {
        db.intern(Type::Function(Function {
            params,
            result: int,
            input: None,
            output: None,
        }))
    };
    // [*R] (...R) -> Int
    let expected = quantified(
        &db,
        vec![bounded(Kind::Schema, Binding::Rest(Rest::Positional), None)],
        function_of(&db, params(&db, rest)),
    );
    let any = function_of(
        &db,
        items(&db, vec![positional(Multiplicity::Repeated, top)]),
    );
    let one = function_of(
        &db,
        items(&db, vec![positional(Multiplicity::Required, top)]),
    );
    db.seal();
    assert_eq!(check(&db, any, expected).status, Status::Proven);
    // `R` may be empty, so it can't supply a required item
    assert_eq!(check(&db, one, expected).status, Status::Contradicted);
}

#[test]
fn quantified_actuals_are_instantiated_inside_the_skolems_scope() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let t = reference(&db, 0, 0);
    // [A] (A) -> Num
    let actual = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], num),
    );
    // [T] (T) -> Int
    let expected = quantified(
        &db,
        vec![binder(Variance::Contravariant)],
        function(&db, &[t], int),
    );
    db.seal();
    let mut s = Solver::new(&db);
    s.constrain(s.closed(actual), s.closed(expected), Provenance::default());
    let outcome = s.solve().remove(0);
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));
    assert_eq!((s.skolems.len(), s.inference.len()), (1, 1));
    let scope = s.skolems[0].scope;
    assert_ne!(scope, ScopeId(0));
    assert_eq!(s.inference[0].scope, scope);
    // `A` takes `T` from below
    let lower: Vec<_> = (s.bounds(InferVarId(0)).lower())
        .map(|term| s.resolve(term).unwrap())
        .collect();
    assert_eq!(lower, [Term::Skolem(SkolemId(0))]);
}

#[test]
fn skolemizing_is_repeatable() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // [T, U] (T) -> Int
    let expected = quantified(
        &db,
        vec![binder(Variance::Invariant), binder(Variance::Invariant)],
        function(&db, &[t], int),
    );
    let top = db.top();
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    // (Value) -> ?v, reprocessed once `?v` is solved
    let actual = function_term(&s, &[s.closed(top)], v);
    s.constrain(actual, s.closed(expected), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    s.constrain(v, s.closed(int), Provenance::default());
    s.constrain(s.closed(int), v, Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.skolems.len(), 2);
    assert_eq!(s.skolemizations.borrow().len(), 1);
}

#[test]
fn skolems_never_leave_their_judgment() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let expected = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], int),
    );
    db.seal();
    let mut s = Solver::new(&db);
    s.constrain(
        s.closed(function(&db, &[db.top()], int)),
        s.closed(expected),
        Provenance::default(),
    );
    assert_eq!(s.solve()[0].status, Status::Proven);
    assert_eq!(s.reify(Term::Skolem(SkolemId(0))), Err(Residual::Escape));
}

#[test]
fn nominals_below_quantified_types_are_residual() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let expected = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], int),
    );
    db.seal();
    let outcome = check(&db, int, expected);
    assert_eq!(outcome.status, Status::Unresolved);
}

/// `[A] (A) -> A`, with `A`'s variance given
fn identity(db: &Database, variance: Variance) -> TypeId {
    let t = reference(db, 0, 0);
    quantified(db, vec![binder(variance)], function(db, &[t], t))
}

#[test]
fn quantified_types_subsume_through_instantiation() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let nil = db.intern(Type::Literal(Literal::Nil));
    let t = reference(&db, 0, 0);
    let t_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(t), UnionMember::Type(nil)].into(),
    ));
    let one = |db: &Database, result| {
        quantified(
            db,
            vec![binder(Variance::Invariant)],
            function(db, &[t], result),
        )
    };
    // [T] (T) -> T | nil
    let widened = one(&db, t_or_nil);
    // [T] (T) -> Int
    let to_int = one(&db, int);
    // [T] () -> T
    let produce = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[], t),
    );
    db.seal();
    let actual = identity(&db, Variance::Invariant);
    for expected in [widened, identity(&db, Variance::Covariant), actual] {
        let mut s = Solver::new(&db);
        s.constrain(s.closed(actual), s.closed(expected), Provenance::default());
        assert_eq!(s.solve()[0].status, Status::Proven, "{expected:?}");
        // `A` took `T`, which has no canonical form
        if let Some(variable) = s.inference.iter().next() {
            assert!(variable.assignment.get().is_some());
            assert_eq!(s.solution(InferVarId(0)), None);
            assert_eq!(s.reify(Term::Infer(InferVarId(0))), Err(Residual::Escape));
        }
    }
    // `A` must be `T`, which isn't `Int`
    let outcome = check(&db, actual, to_int);
    assert!(contradiction(&outcome, Contradiction::Rigid));
    // Nothing is below `A`, so it takes bottom
    assert_eq!(check(&db, produce, produce).status, Status::Proven);
    let produce_a = quantified(
        &db,
        vec![binder(Variance::Covariant)],
        function(&db, &[], t),
    );
    assert_eq!(check(&db, produce_a, produce).status, Status::Proven);
}

#[test]
fn implicit_channels_are_settled_by_their_skolems() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let iter = nominal(&mut db, "Iter", vec![], vec![]);
    let input = reference(&db, 0, 0);
    let method = |db: &Database, param, result| {
        let body = db.intern(Type::Function(Function {
            params: schema(db, &[param]),
            result,
            input: Some(input),
            output: None,
        }));
        quantified(
            db,
            vec![bounded(Kind::Type, Binding::Implicit, Some(iter))],
            body,
        )
    };
    // An override may take more and give less
    let base = method(&db, int, num);
    let derived = method(&db, num, int);
    db.seal();
    assert_eq!(check(&db, derived, base).status, Status::Proven);
    assert_eq!(check(&db, base, derived).status, Status::Contradicted);
}

#[test]
fn probes_prove_quantified_union_members() {
    let mut db = Database::new();
    let nil = db.intern(Type::Literal(Literal::Nil));
    let t = reference(&db, 0, 0);
    let t_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(t), UnionMember::Type(nil)].into(),
    ));
    let widened = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t_or_nil),
    );
    let member = db.intern(Type::Union(
        vec![UnionMember::Type(widened), UnionMember::Type(nil)].into(),
    ));
    db.seal();
    let outcome = check(&db, identity(&db, Variance::Invariant), member);
    assert_eq!(outcome.status, Status::Proven);
}

#[test]
fn nested_quantifiers_nest_their_scopes() {
    let mut db = Database::new();
    let outer = reference(&db, 1, 0);
    let inner = reference(&db, 0, 0);
    // [A] (A) -> [B] (B) -> A, with the given variances
    let curried = |db: &Database, variance| {
        let body = quantified(db, vec![binder(variance)], function(db, &[inner], outer));
        quantified(
            db,
            vec![binder(variance)],
            function(db, &[reference(db, 0, 0)], body),
        )
    };
    let actual = curried(&db, Variance::Invariant);
    let expected = curried(&db, Variance::Covariant);
    // [A] (A) -> [B] (B) -> B
    let wrong = quantified(
        &db,
        vec![binder(Variance::Covariant)],
        function(
            &db,
            &[reference(&db, 0, 0)],
            quantified(
                &db,
                vec![binder(Variance::Covariant)],
                function(&db, &[inner], inner),
            ),
        ),
    );
    db.seal();
    let mut s = Solver::new(&db);
    s.constrain(s.closed(actual), s.closed(expected), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
    let depths: Vec<_> = (0..s.skolems.len())
        .map(|i| s.scopes[s.skolems[i].scope.0].depth)
        .collect();
    assert_eq!(depths, [1, 2]);
    let outcome = check(&db, actual, wrong);
    assert!(contradiction(&outcome, Contradiction::Rigid), "{outcome:?}");
}

#[test]
fn quantifiers_under_parameters_are_checked() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let nil = db.intern(Type::Literal(Literal::Nil));
    let t = reference(&db, 0, 0);
    let t_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(t), UnionMember::Type(nil)].into(),
    ));
    let widened = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[t], t_or_nil),
    );
    let exact = identity(&db, Variance::Invariant);
    // A function taking a generic callback
    let taking = |db: &Database, callback| function(db, &[callback], int);
    db.seal();
    // A callback that must be at least as general as `[V] (V) -> V | nil` can be
    // given the identity
    let outcome = check(&db, taking(&db, widened), taking(&db, exact));
    assert_eq!(outcome.status, Status::Proven);
    let outcome = check(&db, taking(&db, exact), taking(&db, widened));
    assert!(contradiction(&outcome, Contradiction::Rigid));
}

#[test]
fn escaping_skolems_are_promoted_to_their_bounds() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // [T @ Int] (T) -> Int
    let expected = quantified(
        &db,
        vec![bounded(Kind::Type, Binding::Positional, Some(int))],
        function(&db, &[t], int),
    );
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    // (?v) -> Int: `?v` is outside `T`'s scope, so it takes `T`'s bound
    let actual = function_term(&s, &[v], s.closed(int));
    s.constrain(actual, s.closed(expected), Provenance::default());
    s.solve();
    assert!(uses(&s, &Step::Promotion));
    assert!(
        default_all(&mut s)
            .iter()
            .all(|o| o.status == Status::Proven)
    );
    assert_eq!(s.solution(variable_id(v)), Some(int));
}

#[test]
fn skolems_never_escape_through_variables() {
    let mut db = Database::new();
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    // (?v) -> ?v <: [T] (T) -> T: `?v` would have to be `T`
    let actual = function_term(&s, &[v], v);
    let expected = identity(&db, Variance::Invariant);
    s.constrain(actual, s.closed(expected), Provenance::default());
    let outcome = s.solve().remove(0);
    assert_eq!(outcome.status, Status::Unresolved);
    assert!(has(&outcome, Residual::Escape.into()));
    assert!(s.bounds(variable_id(v)).upper().next().is_none());
    // Its bound from below is `T`'s, `Value`, which isn't below `T`
    let outcome = default_all(&mut s).remove(0);
    assert!(
        contradiction(&outcome, Contradiction::Rigid),
        "{outcome:?} {:?}",
        s.solution(variable_id(v))
    );
    assert_eq!(s.solution(variable_id(v)), Some(db.top()));
}

#[test]
fn variables_below_quantified_types_are_residual() {
    let mut db = Database::new();
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    let expected = identity(&db, Variance::Invariant);
    s.constrain(v, s.closed(expected), Provenance::default());
    let outcome = s.solve().remove(0);
    assert!(has(
        &outcome,
        Residual::Unsupported("a variable below a quantified type").into()
    ));
    assert!(s.bounds(variable_id(v)).upper().next().is_none());
    assert!(s.skolems.is_empty());
}
