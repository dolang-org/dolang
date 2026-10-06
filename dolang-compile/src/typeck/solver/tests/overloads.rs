//! Overloaded functions: one overload chosen by trials at a call

use super::*;

fn overloaded(db: &Database, overloads: &[TypeId], implementation: Option<TypeId>) -> TypeId {
    db.intern(Type::Overloaded {
        overloads: overloads.iter().copied().collect(),
        implementation,
    })
}

/// `?0` and `?1`, the first two variables of an environment
fn vars(db: &Database) -> (TypeId, TypeId) {
    (reference(db, 0, 0), reference(db, 0, 1))
}

fn lower(s: &Solver<'_>, variable: Term) -> Vec<Term> {
    s.bounds(variable_id(variable)).lower().collect()
}

#[test]
fn no_overload_fitting_contradicts_and_keeps_each_rejection() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let c = nominal(&mut db, "C", vec![], vec![]);
    let set = overloaded(&db, &[function(&db, &[a], a), function(&db, &[b], b)], None);
    let call = function(&db, &[c], reference(&db, 0, 0));
    db.seal();
    let mut s = Solver::new(&db);
    let r = s.infer();
    let e = s.environment(s.empty_environment(), vec![r]);
    s.constrain(s.closed(set), s.view(call, e), Provenance::default());
    let outcome = s.solve().remove(0);
    assert!(contradiction(&outcome, Contradiction::NoOverload));
    let root = outcome.diagnostics[0].path[0];
    let rejections = s.rejections(root);
    assert_eq!(rejections.len(), 2);
    for rejection in &rejections {
        assert_eq!(rejection.outcome.status, Status::Contradicted);
    }
    assert!(lower(&s, r).is_empty());
}

/// What a call's result is expected to be doesn't choose, but is related to
/// what's chosen
#[test]
fn only_the_arguments_choose_an_overload() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let c = nominal(&mut db, "C", vec![], vec![]);
    let set = overloaded(&db, &[function(&db, &[a], c), function(&db, &[b], b)], None);
    let call = function(&db, &[a], b);
    db.seal();
    let mut s = Solver::new(&db);
    s.constrain(s.closed(set), s.closed(call), Provenance::default());
    let outcome = s.solve().remove(0);
    assert!(contradiction(&outcome, Contradiction::UnrelatedNominals));
    assert!(!has(
        &outcome,
        Issue::Contradiction(Contradiction::NoOverload)
    ));
}

#[test]
fn a_rejected_overload_leaves_no_bounds() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let set = overloaded(&db, &[function(&db, &[b], b), function(&db, &[a], a)], None);
    let (x, r) = vars(&db);
    let call = function(&db, &[x], r);
    db.seal();
    let mut s = Solver::new(&db);
    let (x, r) = (s.infer(), s.infer());
    let e = s.environment(s.empty_environment(), vec![x, r]);
    s.constrain(s.closed(a), x, Provenance::default());
    s.constrain(s.closed(set), s.view(call, e), Provenance::default());
    s.solve();
    let upper: Vec<Term> = s.bounds(variable_id(x)).upper().collect();
    assert_eq!(upper.len(), 1);
    assert!(s.same(upper[0], s.closed(a)).unwrap());
    assert_eq!(default_all(&mut s)[1].status, Status::Proven);
    assert!(s.same(s.closed(s.reify(r).unwrap()), s.closed(a)).unwrap());
}

/// An ambiguous call waits for what its caller later bounds its arguments by,
/// as a default does
#[test]
fn an_ambiguity_waits_for_later_bounds() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let set = overloaded(&db, &[function(&db, &[a], a), function(&db, &[b], b)], None);
    let (x, r) = vars(&db);
    let call = function(&db, &[x], r);
    db.seal();
    let mut s = Solver::new(&db);
    let (x, r) = (s.infer(), s.infer());
    let e = s.environment(s.empty_environment(), vec![x, r]);
    let root = s.constrain(s.closed(set), s.view(call, e), Provenance::default());
    let outcome = s.solve().remove(root.0);
    assert!(has(&outcome, Residual::Ambiguous.into()));
    assert_eq!(s.possible(outcome.diagnostics[0].path[0]), vec![0, 1]);
    s.constrain(s.closed(b), x, Provenance::default());
    assert!(!has(&s.solve()[root.0], Residual::Ambiguous.into()));
    assert!(s.same(lower(&s, r)[0], s.closed(b)).unwrap());
    assert_eq!(default_all(&mut s)[root.0].status, Status::Proven);
}

/// Anywhere but on the left of a function type, an overloaded function is its
/// implementation; what's below it is below each of its signatures
#[test]
fn elsewhere_an_overloaded_function_is_its_implementation() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let (fa, fb) = (function(&db, &[a], a), function(&db, &[b], b));
    let set = overloaded(&db, &[fa, fb], Some(a));
    let unimplemented = overloaded(&db, &[fa, fb], None);
    db.seal();
    let mut s = Solver::new(&db);
    let outcomes = [
        s.constrain(s.closed(set), s.closed(a), Provenance::default()),
        s.constrain(s.closed(set), s.closed(b), Provenance::default()),
        s.constrain(s.closed(unimplemented), s.closed(b), Provenance::default()),
        s.constrain(s.closed(fa), s.closed(unimplemented), Provenance::default()),
    ];
    let solved = s.solve();
    let status = |index: usize| solved[outcomes[index].0].status;
    assert_eq!(status(0), Status::Proven);
    assert_eq!(status(1), Status::Contradicted);
    assert_eq!(status(2), Status::Proven);
    assert_eq!(status(3), Status::Contradicted);
}
