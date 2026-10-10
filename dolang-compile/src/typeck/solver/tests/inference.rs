use super::*;

#[test]
fn inheritance_with_an_inference_argument_produces_bounds() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let one = literal(&db, 1);
    let parent = nominal(&mut db, "Parent", vec![binder(Variance::Covariant)], vec![]);
    let supertype = apply(&db, parent, &[r]);
    let child = nominal(
        &mut db,
        "Child",
        vec![binder(Variance::Covariant)],
        vec![supertype],
    );
    let actual = apply(&db, child, &[r]);
    let expected = apply(&db, parent, &[one]);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    let Term::Infer(id) = variable else {
        unreachable!()
    };
    let e = s.intern_environment(s.empty_environment(), vec![variable]);
    s.constrain(s.view(actual, e), s.closed(expected), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    assert_eq!(s.bounds(id).upper().count(), 1);
    let upper = s.bounds(id).upper().next().unwrap();
    assert!(s.same(upper, s.closed(one)).unwrap());
}

#[test]
fn inference_chains_conflicts_duplicates_and_insertion_order() {
    for reverse in [false, true] {
        let mut db = Database::new();
        let one = literal(&db, 1);
        let two = literal(&db, 2);
        db.seal();
        let mut s = Solver::new(&db);
        let a = s.infer();
        let b = s.infer();
        let c = s.infer();
        let mut constraints = vec![(s.closed(one), a), (a, b), (b, c), (c, s.closed(two))];
        if reverse {
            constraints.reverse();
        }
        for &(a, b) in &constraints {
            s.constrain(a, b, Provenance::default());
        }
        for &(a, b) in &constraints {
            s.constrain(a, b, Provenance::default());
        }
        let results = s.solve();
        assert!(results.iter().all(|r| r.status == Status::Contradicted));
        assert_eq!(
            s.obligations
                .iter()
                .map(|o| o.relation)
                .collect::<HashSet<_>>()
                .len(),
            s.obligations.len(),
        );
    }
}

#[test]
fn multiple_bounds_and_variable_cycles_stabilize() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    db.seal();
    let mut s = Solver::new(&db);
    let a = s.infer();
    let b = s.infer();
    for (a, b) in [
        (s.closed(one), a),
        (s.closed(two), a),
        (a, b),
        (b, a),
        (b, s.closed(db.top())),
    ] {
        s.constrain(a, b, Provenance::default());
    }
    let results = s.solve();
    assert_eq!(results[4].status, Status::Proven);
    assert!(results[..4].iter().all(|r| r.status == Status::Unresolved));
    let Term::Infer(a) = a else { unreachable!() };
    let Term::Infer(b) = b else { unreachable!() };
    assert_eq!(
        s.bounds(a).lower().collect::<HashSet<_>>(),
        HashSet::from([s.closed(one), s.closed(two), Term::Infer(b)])
    );
    assert_eq!(
        s.bounds(b).lower().collect::<HashSet<_>>(),
        HashSet::from([s.closed(one), s.closed(two), Term::Infer(a)])
    );
    assert!(!s.exhausted.get());
}

#[test]
fn new_constraints_after_quiescence_propagate() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    db.seal();
    let mut s = Solver::new(&db);
    let a = s.infer();
    s.constrain(s.closed(one), a, Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    s.constrain(a, s.closed(two), Provenance::default());
    assert!(s.solve().iter().all(|r| r.status == Status::Contradicted));
}

#[test]
fn bare_variables_default_to_the_meet_of_their_upper_bounds() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![a]);
    db.seal();
    let mut s = Solver::new(&db);
    let id = |term| match term {
        Term::Infer(id) => id,
        _ => unreachable!(),
    };
    let met = s.infer();
    s.constrain(met, s.closed(a), Provenance::default());
    s.constrain(met, s.closed(b), Provenance::default());
    let never = s.infer();
    s.constrain(never, s.closed(db.bottom()), Provenance::default());
    let lower = s.infer();
    s.constrain(s.closed(b), lower, Provenance::default());
    s.constrain(lower, s.closed(a), Provenance::default());
    let unbounded = s.infer();
    s.solve();
    assert_eq!(s.default_upper(id(met)), Ok(b));
    assert_eq!(s.default_upper(id(never)), Ok(db.bottom()));
    assert!(s.default_upper(id(lower)).is_err());
    assert!(s.default_upper(id(unbounded)).is_err());
    // The defaulted variables' bounds hold; the one with a lower bound waits
    assert!(s.solve()[..3].iter().all(|r| r.status == Status::Proven));
}

#[test]
fn multiple_upper_bounds_are_checked_separately() {
    let mut db = Database::new();
    let a = nominal(&mut db, "A", vec![], vec![]);
    let b = nominal(&mut db, "B", vec![], vec![]);
    let c = nominal(&mut db, "C", vec![], vec![a, b]);
    let d = nominal(&mut db, "D", vec![], vec![]);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    s.constrain(s.closed(c), variable, Provenance::default());
    s.constrain(variable, s.closed(a), Provenance::default());
    s.constrain(variable, s.closed(b), Provenance::default());
    assert!(s.solve().iter().all(|r| r.status == Status::Unresolved));
    let Term::Infer(id) = variable else {
        unreachable!()
    };
    assert_eq!(s.bounds(id).upper().count(), 2);
    s.constrain(variable, s.closed(d), Provenance::default());
    let results = s.solve();
    assert_eq!(results[0].status, Status::Contradicted);
    assert_eq!(results[1].status, Status::Unresolved);
    assert_eq!(results[2].status, Status::Unresolved);
    assert_eq!(results[3].status, Status::Contradicted);
}

#[test]
fn recursive_inference_bounds_are_not_solutions() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let constructor = nominal(
        &mut db,
        "Container",
        vec![binder(Variance::Covariant)],
        vec![],
    );
    let recursive = apply(&db, constructor, &[r]);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    let env = s.intern_environment(s.empty_environment(), vec![variable]);
    s.constrain(variable, s.view(recursive, env), Provenance::default());
    s.constrain(s.view(recursive, env), variable, Provenance::default());
    assert!(s.solve().iter().all(|r| r.status == Status::Unresolved));
    assert!(!s.exhausted.get());
}

#[test]
fn exact_assignments_propagate_in_both_orders_and_through_cycles() {
    for reverse in [false, true] {
        let mut db = Database::new();
        let int = nominal(&mut db, "Int", vec![], vec![]);
        db.seal();
        let mut s = Solver::new(&db);
        let a = s.infer();
        let b = s.infer();
        let c = s.infer();
        let mut pairs = vec![
            (s.closed(int), a),
            (a, b),
            (b, a),
            (b, c),
            (c, s.closed(int)),
        ];
        if reverse {
            pairs.reverse();
        }
        for (a, b) in pairs {
            s.constrain(a, b, Provenance::default());
        }
        assert!(s.solve().iter().all(|o| o.status == Status::Proven));
        for v in [a, b, c] {
            assert_eq!(s.solution(variable_id(v)), Some(int));
        }
        assert_eq!(s.unresolved().count(), 0);
    }
}

#[test]
fn forced_union_checks_all_uppers_and_preserves_one_sided_bounds() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let union = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(two)].into(),
    ));
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    let unused = s.infer();
    s.constrain(s.closed(one), v, Provenance::default());
    s.constrain(s.closed(two), v, Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Unresolved));
    assert_eq!(
        s.unresolved().collect::<Vec<_>>(),
        vec![variable_id(v), variable_id(unused)]
    );
    s.constrain(v, s.closed(union), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.solution(variable_id(v)), Some(union));
    let work = s.work.get();
    s.solve();
    assert_eq!(s.work.get(), work);
    s.constrain(v, s.closed(one), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Contradicted));
    assert_eq!(s.solution(variable_id(v)), Some(union));
}

#[test]
fn assignments_wake_nested_applications_functions_and_channels() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let r = reference(&db, 0, 0);
    let array = nominal(&mut db, "Array", vec![binder(Variance::Covariant)], vec![]);
    let open_array = apply(&db, array, &[r]);
    let closed_array = apply(&db, array, &[one]);
    let open = db.intern(Type::Function(Function {
        params: schema(&db, &[open_array]),
        result: open_array,
        input: Some(r),
        output: Some(open_array),
    }));
    let closed = db.intern(Type::Function(Function {
        params: schema(&db, &[closed_array]),
        result: closed_array,
        input: Some(one),
        output: Some(closed_array),
    }));
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    let env = s.environment(s.empty_environment(), vec![v]);
    s.constrain(s.view(open, env), s.closed(closed), Provenance::default());
    // Function variance already forces the argument; channel equality must wake too.
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.reify(s.view(open, env)), Ok(closed));
}

#[test]
fn resolution_contradictions_retain_all_roots_and_assignment_paths() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let r = reference(&db, 0, 0);
    let union = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(two)].into(),
    ));
    let open = function(&db, &[], r);
    let expected = function(&db, &[], one);
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    let env = s.environment(s.empty_environment(), vec![v]);
    s.constrain(s.closed(union), v, Provenance::default());
    s.constrain(v, s.closed(union), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    s.constrain(s.view(open, env), s.closed(expected), Provenance::default());
    let outcomes = s.solve();
    assert!(outcomes.iter().all(|o| o.status == Status::Contradicted));
    assert!(
        s.obligations
            .iter()
            .any(|o| o.dependencies.iter().any(|d| d.step == Step::Assignment))
    );
    assert!(outcomes.iter().all(|o| {
        o.diagnostics
            .iter()
            .any(|d| matches!(d.issue, Issue::Contradiction(_)))
    }));
}

#[test]
fn reification_keeps_replacement_scopes_and_local_binders() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let local = reference(&db, 0, 0);
    let free = reference(&db, 1, 0);
    let open = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], free),
    );
    let closed = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[local], one),
    );
    db.seal();
    let mut s = Solver::new(&db);
    let v = s.infer();
    let inner = s.environment(s.empty_environment(), vec![v]);
    let outer = s.environment(s.empty_environment(), vec![s.view(local, inner)]);
    assert_eq!(s.reify(s.view(open, outer)), Err(Residual::Inference));
    s.constrain(s.closed(one), v, Provenance::default());
    s.constrain(v, s.closed(one), Provenance::default());
    s.solve();
    assert_eq!(s.reify(s.view(open, outer)), Ok(closed));
}

#[test]
fn upper_only_and_unanchored_variable_cycles_stay_unsolved() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    db.seal();
    let mut s = Solver::new(&db);
    let a = s.infer();
    let b = s.infer();
    let c = s.infer();
    for (a, b) in [(a, b), (b, a), (c, s.closed(one))] {
        s.constrain(a, b, Provenance::default());
    }
    assert!(s.solve().iter().all(|o| o.status == Status::Unresolved));
    assert_eq!(s.unresolved().count(), 3);
    let work = s.work.get();
    s.solve();
    assert_eq!(s.work.get(), work);
}

#[test]
fn direct_and_indirect_recursive_candidates_are_explicit_residuals() {
    for indirect in [false, true] {
        let mut db = Database::new();
        let r = reference(&db, 0, 0);
        let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
        let open = apply(&db, array, &[r]);
        db.seal();
        let mut s = Solver::new(&db);
        let a = s.infer();
        let b = if indirect { s.infer() } else { a };
        let env = s.environment(s.empty_environment(), vec![b]);
        let recursive = s.view(open, env);
        for (left, right) in [(a, recursive), (recursive, a)] {
            s.constrain(left, right, Provenance::default());
        }
        if indirect {
            s.constrain(a, b, Provenance::default());
            s.constrain(b, a, Provenance::default());
        }
        let outcomes = s.solve();
        assert!(outcomes.iter().all(|o| o.status == Status::Unresolved));
        assert!(outcomes.iter().any(|o| has(o, Residual::Recursive.into())));
        assert_eq!(s.solution(variable_id(a)), None);
        assert_eq!(s.solution(variable_id(b)), None);
        assert!(!s.exhausted.get());
    }
}

#[test]
fn unsupported_upper_bounds_block_commitment_without_becoming_proofs() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let function = function(&db, &[], one);
    // A keyed item's key isn't a positional item's type, so the expansion stays
    let keyed = items(&db, vec![keyed(Multiplicity::Required, one, two)]);
    let expanded = db.intern(Type::Union(vec![UnionMember::Expand(keyed)].into()));
    let alternatives = db.intern(Type::Union(
        vec![UnionMember::Type(two), UnionMember::Type(function)].into(),
    ));
    db.seal();
    for upper in [expanded, alternatives] {
        let mut s = Solver::new(&db);
        let v = s.infer();
        for (a, b) in [(s.closed(one), v), (v, s.closed(one)), (v, s.closed(upper))] {
            s.constrain(a, b, Provenance::default());
        }
        assert!(s.solve().iter().all(|o| o.status == Status::Unresolved));
        assert_eq!(s.solution(variable_id(v)), None);
    }
}

#[test]
fn bounds_keep_context_and_sources_after_assignment_and_duplicate_roots() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let r = reference(&db, 0, 0);
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let open = apply(&db, array, &[r]);
    let closed = apply(&db, array, &[one]);
    db.seal();
    let mut s = Solver::new(&db);
    let a = s.infer();
    let b = s.infer();
    let env = s.environment(s.empty_environment(), vec![a]);
    let contextual = s.view(open, env);
    s.constrain(contextual, b, Provenance::default());
    s.constrain(b, s.closed(closed), Provenance::default());
    s.constrain(s.closed(one), a, Provenance::default());
    s.constrain(a, s.closed(one), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert!(
        s.bounds(variable_id(b))
            .lower()
            .any(|term| term == contextual)
    );
    assert_eq!(s.reify(contextual), Ok(closed));
    let work = s.work.get();
    s.constrain(contextual, b, Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.work.get(), work);
    s.constrain(b, s.closed(db.top()), Provenance::default());
    s.solve();
    assert!(
        s.bounds(variable_id(b))
            .upper()
            .any(|term| term == s.closed(db.top()))
    );
}

#[test]
fn reification_preserves_binder_bounds_defaults_and_declaration_recursion() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let local = reference(&db, 0, 0);
    let free = reference(&db, 1, 0);
    let mut open_binder = binder(Variance::Invariant);
    open_binder.bound = Some(free);
    open_binder.default = Some(free);
    let open = quantified(&db, vec![open_binder.clone()], local);
    open_binder.bound = Some(one);
    open_binder.default = Some(one);
    let closed = quantified(&db, vec![open_binder], local);
    let (decl, wrapper, source) = reserve(&mut db, DeclKind::Alias, "Recursive");
    let recursive = function(&db, &[], wrapper);
    populate(&mut db, decl, source, recursive, vec![]);
    db.seal();
    let mut s = Solver::new(&db);
    let env = s.environment(s.empty_environment(), vec![s.closed(one)]);
    assert_eq!(s.reify(s.view(open, env)), Ok(closed));
    assert_eq!(s.reify(s.closed(wrapper)), Ok(wrapper));
    let v = s.infer();
    s.constrain(s.closed(wrapper), v, Provenance::default());
    s.constrain(v, s.closed(wrapper), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.solution(variable_id(v)), Some(wrapper));
}

#[test]
fn explicit_exact_extreme_bounds_are_solutions_without_defaulting() {
    let mut db = Database::new();
    db.seal();
    for ty in [db.top(), db.bottom()] {
        let mut s = Solver::new(&db);
        let v = s.infer();
        let unused = s.infer();
        s.constrain(s.closed(ty), v, Provenance::default());
        s.constrain(v, s.closed(ty), Provenance::default());
        assert!(s.solve().iter().all(|o| o.status == Status::Proven));
        assert_eq!(s.solution(variable_id(v)), Some(ty));
        assert!(s.solution_sources(variable_id(v)).count() >= 2);
        assert_eq!(s.solution(variable_id(unused)), None);
    }
}

/// A reference to the first schema binder of the innermost group
fn schema_reference(db: &Database) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    })
}

#[test]
fn raised_variables_are_those_at_outputs() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let one = literal(&db, 1);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Invariant)], vec![]);
    let source = nominal(&mut db, "Source", vec![binder(Variance::Covariant)], vec![]);
    let sink = nominal(
        &mut db,
        "Sink",
        vec![binder(Variance::Contravariant)],
        vec![],
    );
    let cases = [
        // An invariant argument
        (apply(&db, boxed, &[r]), true),
        // A callback's parameter, whatever the variance around it
        (apply(&db, boxed, &[function(&db, &[r], one)]), false),
        // A callback's result
        (apply(&db, boxed, &[function(&db, &[], r)]), true),
        // Contravariant twice over
        (apply(&db, sink, &[apply(&db, sink, &[r])]), true),
        (apply(&db, sink, &[r]), false),
    ];
    let schema_r = schema_reference(&db);
    let closed = schema(&db, &[one]);
    let projections = [
        // A held value can't choose the key that selects it
        (selecting(&db, false, closed, r), Kind::Type, false),
        (selecting(&db, true, closed, r), Kind::Type, false),
        // `IndexItem` joins the values it selects, and `AssignItem` meets them,
        // which wider values raise but more items lower
        (selecting(&db, false, schema_r, one), Kind::Schema, true),
        (selecting(&db, true, schema_r, one), Kind::Schema, true),
    ];
    let chained = apply(&db, source, &[r]);
    db.seal();
    let mut s = Solver::new(&db);
    let held = s.infer();
    let mut vars = Vec::new();
    let cases = (cases
        .into_iter()
        .map(|(ty, raised)| (ty, Kind::Type, raised)))
    .chain(projections);
    for (ty, kind, raised) in cases {
        let v = s.infer_kind(kind, Rest::All);
        let e = s.intern_environment(s.empty_environment(), vec![v]);
        s.constrain(held, s.view(ty, e), Provenance::default());
        vars.push((variable_id(v), raised));
    }
    // Through another variable's upper bound
    let middle = s.infer();
    let v = s.infer();
    let e = s.intern_environment(s.empty_environment(), vec![v]);
    s.constrain(held, middle, Provenance::default());
    s.constrain(middle, s.view(chained, e), Provenance::default());
    vars.push((variable_id(v), true));
    s.solve();
    let raised = s.raised(&[held]).unwrap();
    for (index, &(v, expected)) in vars.iter().enumerate() {
        assert_eq!(raised.contains(&v), expected, "case {index}");
    }
}

#[test]
fn locked_variables_are_those_a_literal_would_fix() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let one = literal(&db, 1);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Invariant)], vec![]);
    let source = nominal(&mut db, "Source", vec![binder(Variance::Covariant)], vec![]);
    let sink = nominal(
        &mut db,
        "Sink",
        vec![binder(Variance::Contravariant)],
        vec![],
    );
    let cases = [
        // An invariant argument
        (apply(&db, boxed, &[r]), true),
        // A covariant one widens later
        (apply(&db, source, &[r]), false),
        (apply(&db, sink, &[r]), true),
        // A function's parameter, unlike its result
        (function(&db, &[r], one), true),
        (function(&db, &[], r), false),
    ];
    let schema_r = schema_reference(&db);
    let closed = schema(&db, &[one]);
    let projections = [
        // A wider key raises `IndexItem` and lowers `AssignItem`
        (selecting(&db, false, closed, r), Kind::Type, false),
        (selecting(&db, true, closed, r), Kind::Type, true),
        (selecting(&db, false, schema_r, one), Kind::Schema, false),
        (selecting(&db, true, schema_r, one), Kind::Schema, true),
    ];
    let boxed_r = apply(&db, boxed, &[r]);
    db.seal();
    let mut s = Solver::new(&db);
    let result = s.infer();
    let mut vars = Vec::new();
    let cases = (cases
        .into_iter()
        .map(|(ty, locked)| (ty, Kind::Type, locked)))
    .chain(projections);
    for (ty, kind, locked) in cases {
        let v = s.infer_kind(kind, Rest::All);
        let e = s.intern_environment(s.empty_environment(), vec![v]);
        s.constrain(s.view(ty, e), result, Provenance::default());
        vars.push((variable_id(v), locked));
    }
    // Through another variable's lower bound
    let middle = s.infer();
    let v = s.infer();
    let e = s.intern_environment(s.empty_environment(), vec![v]);
    s.constrain(middle, result, Provenance::default());
    s.constrain(s.view(boxed_r, e), middle, Provenance::default());
    vars.push((variable_id(v), true));
    // Not reached from the roots at all
    let apart = s.infer();
    s.constrain(s.closed(one), apart, Provenance::default());
    vars.push((variable_id(apart), false));
    s.solve();
    let locked = s.locked(&[(result, Variance::Covariant)]).unwrap();
    assert!(!locked.contains(&variable_id(result)));
    for (index, &(v, expected)) in vars.iter().enumerate() {
        assert_eq!(locked.contains(&v), expected, "case {index}");
    }
    // A root taken as an input is locked itself
    let locked = s.locked(&[(result, Variance::Contravariant)]).unwrap();
    assert!(locked.contains(&variable_id(result)));
}

#[test]
fn schema_upper_bounds_keep_shape_and_constrain_values() {
    let mut db = Database::new();
    let int = int(&mut db);
    let key = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("name"))));
    let unknown = db.unknown();
    let shape = items(
        &db,
        vec![
            item(Multiplicity::Required, Element::Positional(unknown)),
            item(
                Multiplicity::Required,
                Element::Keyed {
                    key,
                    value: unknown,
                },
            ),
        ],
    );
    let sym_class = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym_class);
    let bound = items(
        &db,
        vec![
            item(Multiplicity::Repeated, Element::Positional(int)),
            item(
                Multiplicity::Repeated,
                Element::Keyed {
                    key: sym_class,
                    value: int,
                },
            ),
        ],
    );
    let expected = items(
        &db,
        vec![
            item(Multiplicity::Required, Element::Positional(int)),
            item(Multiplicity::Required, Element::Keyed { key, value: int }),
        ],
    );
    db.seal();
    for upper in [[shape, bound], [bound, shape]] {
        let mut solver = Solver::new(&db);
        let variable = solver.infer_kind(Kind::Schema, Rest::All);
        for ty in upper {
            solver.constrain(variable, solver.closed(ty), Provenance::default());
        }
        solver.solve();
        assert_eq!(solver.default(variable_id(variable)), Ok(expected));
        assert!(solver.solve().iter().all(|o| o.status == Status::Proven));
    }
}

#[test]
fn schema_upper_bounds_do_not_invent_opaque_or_incompatible_shapes() {
    let mut db = Database::new();
    let int = int(&mut db);
    let a = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("a"))));
    let b = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("b"))));
    let shapes = [a, b].map(|key| {
        items(
            &db,
            vec![item(
                Multiplicity::Required,
                Element::Keyed { key, value: int },
            )],
        )
    });
    let repeated = items(
        &db,
        vec![item(Multiplicity::Repeated, Element::Positional(int))],
    );
    db.seal();
    for upper in [shapes.to_vec(), vec![repeated], vec![db.unknown_schema()]] {
        let mut solver = Solver::new(&db);
        let variable = solver.infer_kind(Kind::Schema, Rest::All);
        for ty in upper {
            solver.constrain(variable, solver.closed(ty), Provenance::default());
        }
        solver.solve();
        assert!(solver.default(variable_id(variable)).is_err());
        assert!(solver.reify(variable).is_err());
    }
}
