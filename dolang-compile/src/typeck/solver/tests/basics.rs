use super::*;

#[test]
fn solver_requires_sealing() {
    assert!(
        std::panic::catch_unwind(|| {
            Solver::new(&Database::new());
        })
        .is_err()
    );
}

#[test]
fn identity_top_bottom_and_literal_difference() {
    let mut db = Database::new();
    let a = literal(&db, 1);
    let b = literal(&db, 2);
    db.seal();
    assert_eq!(check(&db, a, a).status, Status::Proven);
    assert_eq!(check(&db, a, db.top()).status, Status::Proven);
    assert_eq!(check(&db, db.bottom(), a).status, Status::Proven);
    assert_eq!(check(&db, a, b).status, Status::Contradicted);
    // The canonical database can still grow while borrowed by a solver.
    let solver = Solver::new(&db);
    let c = literal(&db, 3);
    assert_eq!(solver.kind(solver.closed(c)), Kind::Type);
}

#[test]
fn open_identity_uses_environments() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let f = function(&db, &[r], r);
    db.seal();
    let mut s = Solver::new(&db);
    let e1 = s.environment(s.empty_environment(), vec![s.closed(one)]);
    let e2 = s.environment(s.empty_environment(), vec![s.closed(two)]);
    assert_eq!(
        e1,
        s.environment(s.empty_environment(), vec![s.closed(one)])
    );
    s.constrain(s.view(f, e1), s.view(f, e2), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Contradicted);
}

#[test]
fn replacements_keep_their_own_context_under_quantifiers() {
    let mut db = Database::new();
    let local = reference(&db, 0, 0);
    let outer = reference(&db, 1, 0);
    let one = literal(&db, 1);
    let open = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], outer),
    );
    let closed = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], one),
    );
    db.seal();
    let mut s = Solver::new(&db);
    let captured = s.environment(s.empty_environment(), vec![s.closed(one)]);
    let replacement = s.view(local, captured);
    let caller = s.environment(s.empty_environment(), vec![replacement]);
    assert!(s.same(s.view(open, caller), s.closed(closed)).unwrap());
    s.constrain(
        s.view(open, caller),
        s.closed(closed),
        Provenance::default(),
    );
    assert_eq!(s.solve()[0].status, Status::Proven);
}

#[test]
fn closed_quantifiers_ignore_unused_environments() {
    let mut db = Database::new();
    let local = reference(&db, 0, 0);
    let poly = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], local),
    );
    db.seal();
    let mut s = Solver::new(&db);
    let e = s.environment(s.empty_environment(), vec![s.closed(db.top())]);
    assert!(s.same(s.view(poly, e), s.closed(poly)).unwrap());
}

#[test]
fn all_literal_backing_types_and_their_supertypes() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sym = db.intern_symbol("value");
    let mut cases = vec![];
    for (intrinsic, value) in [
        (Intrinsic::Nil, Literal::Nil),
        (Intrinsic::Bool, Literal::Bool(true)),
        (Intrinsic::Int, Literal::Int(5)),
        (Intrinsic::Sym, Literal::Sym(sym)),
        (Intrinsic::Str, Literal::Str("value".into())),
    ] {
        let backing = nominal(&mut db, "Backing", vec![], vec![base]);
        db.set_intrinsic(intrinsic, backing);
        cases.push((db.intern(Type::Literal(value)), backing));
    }
    db.seal();
    for (value, backing) in cases {
        assert_eq!(check(&db, value, backing).status, Status::Proven);
        assert_eq!(check(&db, value, base).status, Status::Proven);
    }
}

#[test]
fn missing_literal_backing_is_not_a_contradiction() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let value = literal(&db, 0);
    db.seal();
    let result = check(&db, value, base);
    assert!(has(
        &result,
        Residual::MissingIntrinsic(Intrinsic::Int).into()
    ));
    assert_eq!(result.status, Status::Unresolved);
}

#[test]
fn unsupported_nested_forms_stay_residual() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let union = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(two)].into(),
    ));
    let a = function(&db, &[], one);
    let b = function(&db, &[], union);
    let r = reference(&db, 0, 0);
    let poly = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[r], r),
    );
    let mono = function(&db, &[one], one);
    let optional = db.intern(Type::Schema(
        vec![SchemaItem {
            multiplicity: Multiplicity::Optional,
            element: Element::Positional(one),
        }]
        .into(),
    ));
    let variadic = db.intern(Type::Function(Function {
        params: optional,
        result: one,
        input: None,
        output: None,
    }));
    db.seal();
    // Instantiation with a fresh variable forces it through the invariant pair
    assert_eq!(check(&db, poly, mono).status, Status::Proven);
    // An optional parameter may be passed
    assert_eq!(check(&db, variadic, mono).status, Status::Proven);
    assert!(has(
        &check(&db, optional, schema(&db, &[one])),
        Issue::Contradiction(Contradiction::Missing(0))
    ));
    assert_eq!(check(&db, one, union).status, Status::Proven);
    assert_eq!(check(&db, a, b).status, Status::Proven);
    assert_eq!(check(&db, union, union).status, Status::Proven);
}

#[test]
fn limits_are_residual_not_proof_or_contradiction() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let mut s = Solver::with_limits(
        &db,
        Limits {
            work: 0,
            depth: 256,
        },
    );
    s.constrain(s.closed(one), s.closed(one), Provenance::default());
    let result = s.solve().remove(0);
    assert_eq!(result.status, Status::Unresolved);
    assert!(has(&result, Residual::Limit.into()));
    let mut s = Solver::with_limits(
        &db,
        Limits {
            work: 100,
            depth: 0,
        },
    );
    s.constrain(s.closed(one), s.closed(one), Provenance::default());
    assert!(has(&s.solve()[0], Residual::Limit.into()));
}

#[test]
fn shared_reductions_preserve_each_root_and_dependency() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let a = alias(&mut db, "A", one);
    let b = alias(&mut db, "B", two);
    let Type::Decl(aid) = *db.ty(a) else {
        unreachable!()
    };
    let Type::Decl(bid) = *db.ty(b) else {
        unreachable!()
    };
    let span_a = db.declaration(aid).source.span;
    let span_b = db.declaration(bid).source.span;
    db.seal();
    let mut s = Solver::new(&db);
    for _ in 0..2 {
        s.constrain(
            s.closed(function(&db, &[], a)),
            s.closed(function(&db, &[], b)),
            Provenance {
                actual: Some(span_a),
                expected: Some(span_b),
            },
        );
    }
    let results = s.solve();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result.status, Status::Contradicted);
        assert_eq!(
            s.provenance(result.constraint).actual.unwrap().unit,
            span_a.unit
        );
        let diagnostic = result
            .diagnostics
            .iter()
            .find(|d| matches!(d.issue, Issue::Contradiction(_)))
            .unwrap();
        assert_eq!(diagnostic.path.len(), 2);
        let leaf = s.obligation(*diagnostic.path.last().unwrap());
        assert_eq!(leaf.relation.actual, s.closed(a));
        assert_eq!(leaf.relation.expected, s.closed(b));
        let parent = s.obligation(diagnostic.path[0]);
        assert!(
            parent
                .dependencies
                .iter()
                .any(|edge| edge.obligation == diagnostic.path[1] && edge.step == Step::Return)
        );
    }
    // The root, its parameter lists and its results
    assert_eq!(s.obligations.len(), 3);
}

#[test]
fn repeated_solve_without_new_constraints_does_no_work() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    s.constrain(s.closed(one), variable, Provenance::default());
    s.constrain(variable, s.closed(one), Provenance::default());
    assert!(s.solve().iter().all(|r| r.status == Status::Proven));
    let work = s.work.get();
    assert!(s.solve().iter().all(|r| r.status == Status::Proven));
    assert_eq!(s.work.get(), work);
}

#[test]
fn generic_instantiation_does_not_capture_nested_quantifier_references() {
    let mut db = Database::new();
    let local = reference(&db, 0, 0);
    let outer = reference(&db, 1, 0);
    let one = literal(&db, 1);
    let inner = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], outer),
    );
    let outer_type = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[inner], local),
    );
    let constructor = alias(&mut db, "HigherRank", outer_type);
    let instantiated = apply(&db, constructor, &[local]);
    let expected_inner = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], one),
    );
    let expected = function(&db, &[expected_inner], one);
    db.seal();
    let mut s = Solver::new(&db);
    let env = s.environment(s.empty_environment(), vec![s.closed(one)]);
    s.constrain(
        s.view(instantiated, env),
        s.closed(expected),
        Provenance::default(),
    );
    assert_eq!(s.solve()[0].status, Status::Proven);
}

#[test]
fn substitution_kind_misuse_panics() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let empty_schema = schema(&db, &[]);
    db.seal();
    let mut s = Solver::new(&db);
    let env = s.environment(s.empty_environment(), vec![s.closed(empty_schema)]);
    s.constrain(s.view(r, env), s.closed(db.top()), Provenance::default());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.solve())).is_err());
}

#[test]
fn shared_insertion_preserves_borrowed_solver_state() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let mut solver = Solver::new(&db);
    let variable = solver.infer();
    let variables: Vec<_> = (0..256).map(|_| solver.infer()).collect();
    let root = solver.constrain(solver.closed(one), variable, Provenance::default());
    let s = &solver;
    let Term::Infer(id) = variable else {
        unreachable!()
    };
    let bounds = s.bounds(id);
    let environment = s.intern_environment(s.empty_environment(), vec![variable]);
    let frame = s.environments.get_by_index(environment.0).unwrap();
    let obligation = s.obligation(s.roots[root.0].obligation);
    for next in variables {
        s.intern_environment(environment, vec![next]);
        s.derive(
            s.roots[root.0].obligation,
            variable,
            next,
            Step::BoundPropagation,
        );
    }
    assert_eq!(frame.group, vec![variable]);
    assert_eq!(
        environment,
        s.intern_environment(s.empty_environment(), vec![variable])
    );
    assert_eq!(obligation.relation.actual, s.closed(one));
    assert_eq!(bounds.lower().count(), 0);
    assert!(
        solver
            .solve()
            .iter()
            .all(|o| o.status == Status::Unresolved)
    );
    assert_eq!(
        solver.bounds(id).lower().collect::<Vec<_>>(),
        vec![solver.closed(one)]
    );
}
