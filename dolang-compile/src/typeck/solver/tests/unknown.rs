use super::*;

#[test]
fn unknown_is_consistent_with_every_type_and_schema() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let unknown = db.unknown();
    let class = nominal(&mut db, "Class", vec![], vec![]);
    let func = function(&db, &[one], one);
    let params = schema(&db, &[one]);
    db.seal();
    for ty in [one, class, func, db.top(), db.bottom()] {
        assert_eq!(check(&db, unknown, ty).status, Status::Proven);
        assert_eq!(check(&db, ty, unknown).status, Status::Proven);
    }
    assert_eq!(
        check(&db, db.unknown_schema(), params).status,
        Status::Proven
    );
    assert_eq!(
        check(&db, params, db.unknown_schema()).status,
        Status::Proven
    );
}

#[test]
fn unknown_is_consistent_under_arguments_and_functions() {
    for variance in [
        Variance::Covariant,
        Variance::Contravariant,
        Variance::Invariant,
    ] {
        let mut db = Database::new();
        let one = literal(&db, 1);
        let unknown = db.unknown();
        let boxed = nominal(&mut db, "Box", vec![binder(variance)], vec![]);
        let dynamic = apply(&db, boxed, &[unknown]);
        let concrete = apply(&db, boxed, &[one]);
        db.seal();
        for (a, b) in [(dynamic, concrete), (concrete, dynamic)] {
            let outcome = check(&db, a, b);
            assert_eq!(outcome.status, Status::Proven, "{variance:?}: {outcome:?}");
        }
    }
    let mut db = Database::new();
    let one = literal(&db, 1);
    let unknown = db.unknown();
    let dynamic = function(&db, &[unknown], one);
    let concrete = function(&db, &[one], unknown);
    db.seal();
    assert_eq!(check(&db, dynamic, concrete).status, Status::Proven);
    assert_eq!(check(&db, concrete, dynamic).status, Status::Proven);
}

#[test]
fn unknown_channels_are_consistent_at_any_depth() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let unknown = db.unknown();
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let dynamic_iter = apply(&db, iter, &[unknown]);
    let concrete_iter = apply(&db, iter, &[one]);
    let channels = |db: &Database, input| {
        db.intern(Type::Function(Function {
            params: schema(db, &[]),
            result: one,
            input: Some(input),
            output: Some(one),
        }))
    };
    let dynamic = channels(&db, unknown);
    let concrete = channels(&db, concrete_iter);
    let nested = channels(&db, dynamic_iter);
    db.seal();
    assert_eq!(check(&db, dynamic, concrete).status, Status::Proven);
    assert_eq!(check(&db, concrete, dynamic).status, Status::Proven);
    assert_eq!(check(&db, nested, concrete).status, Status::Proven);
    assert_eq!(check(&db, concrete, nested).status, Status::Proven);
}

#[test]
fn unknown_union_members_are_checked_like_any_other() {
    let db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let unknown = db.unknown();
    let either = db.intern(Type::Union(
        vec![UnionMember::Type(two), UnionMember::Type(unknown)].into(),
    ));
    let both = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(unknown)].into(),
    ));
    let mut db = db;
    db.seal();
    assert_eq!(check(&db, one, either).status, Status::Proven);
    assert_eq!(check(&db, both, two).status, Status::Contradicted);
}

#[test]
fn unknown_bounds_never_force_assignments() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let unknown = db.unknown();
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let dynamic_array = apply(&db, array, &[unknown]);
    db.seal();
    let mut s = Solver::new(&db);

    // Only dynamic bounds
    let a = s.infer();
    s.constrain(s.closed(unknown), a, Provenance::default());
    s.constrain(a, s.closed(unknown), Provenance::default());
    let b = s.infer();
    s.constrain(s.closed(dynamic_array), b, Provenance::default());
    s.constrain(b, s.closed(dynamic_array), Provenance::default());

    // A concrete bound still forces, whatever dynamic bounds accompany it
    let c = s.infer();
    s.constrain(s.closed(one), c, Provenance::default());
    s.constrain(c, s.closed(one), Provenance::default());
    s.constrain(c, s.closed(unknown), Provenance::default());

    // No chain through the dynamic type relates 1 to 2
    let d = s.infer();
    let e = s.infer();
    s.constrain(s.closed(one), d, Provenance::default());
    s.constrain(d, s.closed(unknown), Provenance::default());
    s.constrain(s.closed(unknown), e, Provenance::default());
    s.constrain(e, s.closed(two), Provenance::default());

    let outcomes = s.solve();
    assert!(outcomes.iter().all(|o| o.status != Status::Contradicted));
    assert_eq!(s.solution(variable_id(c)), Some(one));
    for v in [a, b, d, e] {
        assert_eq!(s.solution(variable_id(v)), None);
    }
}
