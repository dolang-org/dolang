use super::*;

/// An origin of `kind` at a span of line 1
fn at(kind: OriginKind, column: u32) -> Option<Origin> {
    Some(Origin::local(kind, diag::Span::new(1, column, 1)))
}

/// The origins of each contradiction's leaf, with the steps on its path
fn leaves(s: &Solver<'_>, outcome: &Outcome) -> Vec<(Vec<Option<Step>>, Provenance)> {
    assert_eq!(outcome.status, Status::Contradicted);
    s.explain(outcome)
        .iter()
        .map(|cause| {
            let steps = cause.path.iter().map(|node| node.step.clone()).collect();
            let leaf = cause.path.last().expect("a path has its root");
            (steps, leaf.origins.clone())
        })
        .collect()
}

#[test]
fn an_argument_narrows_its_call_origin() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let callee = function(&db, &[one, two], two);
    db.seal();
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    let args = [one, one].map(|ty| {
        (
            Multiplicity::Required,
            CallArgument::Positional(s.closed(ty)),
        )
    });
    let call = Call {
        callee: s.closed(callee),
        arguments: s.arguments_schema(&args),
        result: s.closed(two),
        input: None,
        output: None,
    };
    let arguments = [diag::Span::new(1, 2, 1), diag::Span::new(1, 4, 1)];
    let provenance = Provenance {
        actual: Some(Origin::local(
            OriginKind::Call {
                arguments: arguments.to_vec().into(),
                callee: None,
            },
            diag::Span::new(1, 0, 5),
        )),
        expected: None,
    };
    s.constrain_call(call, provenance);
    let outcome = s.solve().remove(0);
    let leaves = leaves(&s, &outcome);
    // The second argument doesn't fit, and is where its relation comes from
    let (steps, origins) = leaves
        .iter()
        .find(|(steps, _)| steps.contains(&Some(Step::Item(1))))
        .expect("the second argument's contradiction");
    assert!(steps.contains(&Some(Step::Arguments)));
    assert_eq!(origins.actual, at(OriginKind::Expression, 4));
    assert_eq!(origins.expected, None);
}

#[test]
fn an_extra_argument_is_the_calls_arguments() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let callee = function(&db, &[one], one);
    db.seal();
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    let args = [one, one].map(|ty| {
        (
            Multiplicity::Required,
            CallArgument::Positional(s.closed(ty)),
        )
    });
    let call = Call {
        callee: s.closed(callee),
        arguments: s.arguments_schema(&args),
        result: s.closed(one),
        input: None,
        output: None,
    };
    let arguments: alias::Box<[diag::Span]> = [diag::Span::new(1, 2, 1), diag::Span::new(1, 4, 1)]
        .to_vec()
        .into();
    let provenance = Provenance {
        actual: Some(Origin::local(
            OriginKind::Call {
                arguments: arguments.clone(),
                callee: None,
            },
            diag::Span::new(1, 0, 5),
        )),
        expected: None,
    };
    s.constrain_call(call, provenance);
    let outcome = s.solve().remove(0);
    let excess = s
        .explain(&outcome)
        .iter()
        .find(|cause| cause.contradiction == Contradiction::Excess(1))
        .map(|cause| {
            cause
                .path
                .last()
                .expect("a path has its root")
                .origins
                .clone()
        })
        .expect("the extra argument's contradiction");
    assert_eq!(
        excess.actual,
        Some(Origin::local(
            OriginKind::Arguments(arguments),
            diag::Span::new(1, 0, 5)
        ))
    );
}

#[test]
fn a_propagated_bound_takes_each_side_from_its_own_root() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    db.seal();
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    let v = s.infer();
    let lower = Provenance {
        actual: at(OriginKind::Expression, 0),
        expected: None,
    };
    let upper = Provenance {
        actual: None,
        expected: at(OriginKind::ExpectedResult, 8),
    };
    s.constrain(s.closed(one), v, lower);
    s.constrain(v, s.closed(two), upper);
    let outcomes = s.solve();
    // Each root reaches `1 <: 2`, with the other as its partner
    for outcome in &outcomes {
        let leaves = leaves(&s, outcome);
        let (steps, origins) = leaves
            .iter()
            .find(|(steps, _)| steps.last() == Some(&Some(Step::BoundPropagation)))
            .expect("a propagated contradiction");
        assert_eq!(steps.len(), 2);
        assert_eq!(origins.actual, at(OriginKind::Expression, 0));
        assert_eq!(origins.expected, at(OriginKind::ExpectedResult, 8));
    }
}

#[test]
fn an_assignment_takes_where_its_variable_stands() {
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
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    let v = s.infer();
    let env = s.environment(s.empty_environment(), vec![v]);
    s.constrain(s.closed(union), v, Provenance::default());
    let stands = Provenance {
        actual: at(OriginKind::Held, 4),
        expected: None,
    };
    let held = s.constrain(v, s.closed(union), stands);
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    let checked = Provenance {
        actual: None,
        expected: at(OriginKind::ExpectedResult, 8),
    };
    s.constrain(s.view(open, env), s.closed(expected), checked);
    let outcomes = s.solve();
    // The variable's upper bound reaches `1 | 2 <: 1` only through its
    // assignment, below what checks it
    let leaves = leaves(&s, &outcomes[held.0]);
    let (_, origins) = leaves
        .iter()
        .find(|(steps, _)| steps.contains(&Some(Step::Assignment)))
        .expect("a contradiction through the assignment");
    assert_eq!(origins.actual, at(OriginKind::Held, 4));
    assert_eq!(origins.expected, at(OriginKind::ExpectedResult, 8));
}

#[test]
fn a_contravariant_step_swaps_origins() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let narrow = function(&db, &[two], one);
    let wide = function(&db, &[one], one);
    db.seal();
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    let provenance = Provenance {
        actual: at(OriginKind::Expression, 0),
        expected: at(OriginKind::ExpectedResult, 8),
    };
    s.constrain(s.closed(narrow), s.closed(wide), provenance);
    let outcome = s.solve().remove(0);
    let leaves = leaves(&s, &outcome);
    let (steps, origins) = leaves.first().expect("a contradiction");
    assert!(steps.contains(&Some(Step::Parameters)));
    // `1 <: 2`: the expected function's parameter is the actual side
    assert_eq!(origins.actual, at(OriginKind::ExpectedResult, 8));
    assert_eq!(origins.expected, at(OriginKind::Expression, 0));
}
