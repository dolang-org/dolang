use super::*;

/// A parent solver with `1 <: a` solved, `a` defaulted to `1`, and `b` unbounded
fn parent<'a>(db: &'a Database, gas: &'a Gas) -> (Solver<'a>, InferVarId, InferVarId) {
    let one = literal(db, 1);
    let mut s = Solver::new(db, gas);
    let a = s.infer();
    let b = s.infer();
    s.constrain(s.closed(one), a, Provenance::default());
    s.solve();
    s.default(variable_id(a)).unwrap();
    s.solve();
    (s, variable_id(a), variable_id(b))
}

#[test]
fn a_child_sees_its_parents_bounds_and_assignments() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let gas = Gas::default();
    let (s, a, b) = parent(&db, &gas);
    let child = s.fork(&gas);
    let lower: Vec<_> = child.bounds(a).lower().collect();
    assert_eq!(lower.len(), 1);
    assert!(child.same(lower[0], child.closed(one)).unwrap());
    assert_eq!(child.assignment(a), s.assignment(a));
    assert!(child.assignment(a).is_some());
    assert!(child.assignment(b).is_none());
    assert_eq!(child.relation(ObligationId(0)), s.relation(ObligationId(0)));
}

#[test]
fn what_a_child_solves_doesnt_reach_its_parent() {
    let mut db = Database::new();
    let two = literal(&db, 2);
    db.seal();
    let gas = Gas::default();
    let (s, a, b) = parent(&db, &gas);
    let (variables, obligations) = (s.variable_count(), s.obligation_count());
    let dependencies = s.dependencies(ObligationId(0));
    {
        let mut child = s.fork(&gas);
        let fresh = child.infer();
        child.constrain(child.closed(two), Term::Infer(b), Provenance::default());
        child.constrain(Term::Infer(b), fresh, Provenance::default());
        child.solve();
        child.default(b).unwrap();
        child.solve();
        let added = ObligationId(obligations);
        child.add_dependency(ObligationId(0), added, Step::Assignment);
        assert!(child.assignment(b).is_some());
        assert!(
            (child.bounds(b).lower()).any(|bound| child.same(bound, child.closed(two)).unwrap())
        );
        assert!(child.bounds(variable_id(fresh)).lower().next().is_some());
        assert!(child.obligation_count() > obligations);
        assert_eq!(
            child.dependencies(ObligationId(0)).len(),
            dependencies.len() + 1
        );
        // The parent's own assignment is unchanged
        assert_eq!(child.assignment(a), s.assignment(a));
    }
    assert!(s.assignment(b).is_none());
    assert_eq!(s.bounds(b).lower().count(), 0);
    assert_eq!(s.bounds(b).upper().count(), 0);
    assert_eq!(s.variable_count(), variables);
    assert_eq!(s.obligation_count(), obligations);
    assert_eq!(s.dependencies(ObligationId(0)).len(), dependencies.len());
    // Another child may be nested once the first is gone
    let child = s.fork(&gas);
    assert!(child.assignment(b).is_none());
}

#[test]
fn a_childs_ids_continue_from_its_parents() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let gas = Gas::default();
    let (mut s, a, _) = parent(&db, &gas);
    let known = s.environment(s.empty_environment(), vec![Term::Infer(a)]);
    let mut child = s.fork(&gas);
    let Term::Infer(fresh) = child.infer() else {
        unreachable!()
    };
    assert_eq!(fresh.0, s.variable_count());
    let environment = child.environment(child.empty_environment(), vec![Term::Infer(fresh)]);
    assert!(!s.has_environment(environment));
    assert!(child.has_environment(environment));
    // A frame the parent interned keeps its ID
    let again = child.environment(child.empty_environment(), vec![Term::Infer(a)]);
    assert_eq!(again, known);
    let id = child.constrain(
        child.view(one, environment),
        child.closed(one),
        Provenance::default(),
    );
    let root = child.roots[id.0].obligation;
    assert_eq!(root.0, s.obligation_count());
}

#[test]
#[should_panic(expected = "already has a live one")]
fn a_solver_has_one_live_child() {
    let mut db = Database::new();
    db.seal();
    let gas = Gas::default();
    let s = Solver::new(&db, &gas);
    let _first = s.fork(&gas);
    let _second = s.fork(&gas);
}

#[test]
#[should_panic(expected = "changed while a solver nested in it is live")]
fn a_parent_is_unchanged_while_its_child_is_live() {
    let mut db = Database::new();
    db.seal();
    let gas = Gas::default();
    let s = Solver::new(&db, &gas);
    let _child = s.fork(&gas);
    s.fresh(Kind::Type, Rest::All, ScopeId(0));
}
