use super::*;

fn union(db: &Database, members: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        members.iter().copied().map(UnionMember::Type).collect(),
    ))
}

/// `?0`, the first variable of an environment
fn var(db: &Database) -> TypeId {
    reference(db, 0, 0)
}

fn lower(s: &Solver<'_>, variable: Term) -> Vec<Term> {
    s.bounds(variable_id(variable)).lower().collect()
}

#[test]
fn a_rejected_member_leaves_the_one_to_infer_through() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let expected = union(&db, &[b, var(&db)]);
    db.seal();
    let mut s = Solver::new(&db);
    let t = s.infer();
    let e = s.environment(s.empty_environment(), vec![t]);
    s.constrain(s.closed(a), s.view(expected, e), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    let lower = lower(&s, t);
    assert_eq!(lower.len(), 1);
    assert!(s.same(lower[0], s.closed(a)).unwrap());
    assert_eq!(default_all(&mut s)[0].status, Status::Proven);
}

#[test]
fn several_possible_members_are_ambiguous_and_leak_nothing() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let expected = union(&db, &[reference(&db, 0, 0), reference(&db, 0, 1)]);
    db.seal();
    let mut s = Solver::new(&db);
    let (t, u) = (s.infer(), s.infer());
    let e = s.environment(s.empty_environment(), vec![t, u]);
    s.constrain(s.closed(a), s.view(expected, e), Provenance::default());
    let outcome = s.solve().remove(0);
    assert_eq!(outcome.status, Status::Unresolved);
    assert!(has(&outcome, Residual::Ambiguous.into()));
    for variable in [t, u] {
        let bounds = s.bounds(variable_id(variable));
        assert_eq!(bounds.lower().count() + bounds.upper().count(), 0);
    }
    // Nothing bounds either from below, so defaulting can't choose
    assert!(has(&default_all(&mut s)[0], Residual::Ambiguous.into()));
}

#[test]
fn a_member_proven_without_bounds_is_chosen_beside_possible_ones() {
    let mut db = Database::new();
    let parent = nominal(&mut db, "Parent", vec![], vec![]);
    let child = nominal(&mut db, "Child", vec![], vec![parent]);
    let expected = union(&db, &[parent, var(&db)]);
    db.seal();
    let mut s = Solver::new(&db);
    let t = s.infer();
    let e = s.environment(s.empty_environment(), vec![t]);
    s.constrain(s.closed(child), s.view(expected, e), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
    assert!(lower(&s, t).is_empty());
}

/// A choice waits for bounds that reject the other members, and doesn't depend
/// on the order constraints arrive in
#[test]
fn growing_bounds_settle_an_ambiguity_in_any_order() {
    for later in [false, true] {
        let mut db = Database::new();
        let a = nominal(&mut db, "A", vec![], vec![]);
        let b = nominal(&mut db, "B", vec![], vec![]);
        let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
        let actual = apply(&db, boxed, &[var(&db)]);
        let boxed_a = apply(&db, boxed, &[a]);
        let boxed_b = apply(&db, boxed, &[b]);
        let expected = union(&db, &[boxed_a, boxed_b]);
        db.seal();
        let mut s = Solver::new(&db);
        let x = s.infer();
        let e = s.environment(s.empty_environment(), vec![x]);
        if !later {
            s.constrain(s.closed(a), x, Provenance::default());
        }
        let root = s.constrain(s.view(actual, e), s.closed(expected), Provenance::default());
        if later {
            assert!(has(&s.solve()[root.0], Residual::Ambiguous.into()));
            s.constrain(s.closed(a), x, Provenance::default());
        }
        let outcome = s.solve().remove(root.0);
        assert!(!has(&outcome, Residual::Ambiguous.into()), "{later}");
        let upper: Vec<Term> = s.bounds(variable_id(x)).upper().collect();
        assert_eq!(upper.len(), 1);
        assert!(s.same(upper[0], s.closed(a)).unwrap());
        assert_eq!(default_all(&mut s)[root.0].status, Status::Proven);
    }
}

#[test]
fn a_union_inside_a_chosen_member_is_judged_in_turn() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let c = nominal(&mut db, "C", vec![], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let actual = apply(&db, boxed, &[a]);
    let inner = union(&db, &[b, var(&db)]);
    let boxed_inner = apply(&db, boxed, &[inner]);
    let expected = union(&db, &[boxed_inner, c]);
    db.seal();
    let mut s = Solver::new(&db);
    let t = s.infer();
    let e = s.environment(s.empty_environment(), vec![t]);
    s.constrain(s.closed(actual), s.view(expected, e), Provenance::default());
    s.solve();
    let lower = lower(&s, t);
    assert_eq!(lower.len(), 1);
    assert!(s.same(lower[0], s.closed(a)).unwrap());
    assert_eq!(default_all(&mut s)[0].status, Status::Proven);
}

#[test]
fn a_function_infers_through_the_function_member() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let c = nominal(&mut db, "C", vec![], vec![]);
    let func = nominal(&mut db, "Func", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Func, func);
    let actual = function(&db, &[var(&db)], a);
    let callback = function(&db, &[b], a);
    let expected = union(&db, &[callback, c]);
    db.seal();
    let mut s = Solver::new(&db);
    let p = s.infer();
    let e = s.environment(s.empty_environment(), vec![p]);
    s.constrain(s.view(actual, e), s.closed(expected), Provenance::default());
    s.solve();
    let lower = lower(&s, p);
    assert_eq!(lower.len(), 1);
    assert!(s.same(lower[0], s.closed(b)).unwrap());
}

#[test]
fn rejecting_every_member_contradicts_a_literal_but_not_a_class() {
    let mut db = Database::new();
    int(&mut db);
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let boxed_var = apply(&db, boxed, &[var(&db)]);
    let literals = union(&db, &[two, boxed_var]);
    let classes = union(&db, &[b, boxed_var]);
    db.seal();
    let mut s = Solver::new(&db);
    let t = s.infer();
    let e = s.environment(s.empty_environment(), vec![t]);
    s.constrain(s.closed(one), s.view(literals, e), Provenance::default());
    s.constrain(s.closed(a), s.view(classes, e), Provenance::default());
    let outcomes = s.solve();
    assert!(contradiction(&outcomes[0], Contradiction::Outside));
    // A generic member's rejected arguments needn't exclude every `A`
    assert_eq!(outcomes[1].status, Status::Unresolved);
    assert!(lower(&s, t).is_empty());
}

/// Exhausting the budget inside a trial is never a proof
#[test]
fn exhaustion_in_a_trial_is_residual() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let expected = union(&db, &[b, var(&db)]);
    db.seal();
    let mut exhausted = false;
    for work in 0..200 {
        let mut s = Solver::with_limits(&db, Limits { work, depth: 256 });
        let t = s.infer();
        let e = s.environment(s.empty_environment(), vec![t]);
        s.constrain(s.closed(a), s.view(expected, e), Provenance::default());
        let outcome = default_all(&mut s).remove(0);
        // A probe that exhausts its share leaves a default untaken, so a short
        // budget can leave the judgment unresolved without a limit of its own
        assert_ne!(outcome.status, Status::Contradicted, "{work}");
        if has(&outcome, Residual::Limit.into()) {
            exhausted = true;
            assert_eq!(outcome.status, Status::Unresolved, "{work}");
        }
    }
    assert!(exhausted);
}
