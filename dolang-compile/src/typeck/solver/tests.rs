use super::*;
use crate::typeck::r#type::{
    BinderSource, BoundRef, DeclKind, DeclSource, Declaration, SchemaItem, UnionMember,
};

fn literal(db: &Database, n: i128) -> TypeId {
    db.intern(Type::Literal(Literal::Int(n)))
}

fn reference(db: &Database, depth: usize, slot: usize) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(depth, slot),
        kind: Kind::Type,
    })
}

fn binder(variance: Variance) -> Binder {
    Binder {
        kind: Kind::Type,
        binding: Binding::Positional,
        bound: None,
        default: None,
        variance,
    }
}

fn quantified(db: &Database, binders: Vec<Binder>, body: TypeId) -> TypeId {
    db.intern(Type::Quantified {
        binders: binders.into(),
        body,
    })
}

fn apply(db: &Database, base: TypeId, args: &[TypeId]) -> TypeId {
    db.intern(Type::Apply {
        base,
        args: args.iter().copied().map(Argument::Positional).collect(),
        kind: Kind::Type,
    })
}

fn schema(db: &Database, params: &[TypeId]) -> TypeId {
    db.intern(Type::Schema(
        params
            .iter()
            .map(|&ty| SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Positional(ty),
            })
            .collect(),
    ))
}

fn function(db: &Database, params: &[TypeId], result: TypeId) -> TypeId {
    db.intern(Type::Function(Function {
        params: schema(db, params),
        result,
        input: None,
        output: None,
    }))
}

fn reserve(db: &mut Database, kind: DeclKind, name: &str) -> (DeclId, TypeId, DeclSource) {
    let id = db.allocate();
    let ty = db.intern(Type::Decl(id));
    let source = DeclSource {
        kind,
        result_kind: Kind::Type,
        name: Some(db.intern_symbol(name)),
        span: UnitSpan {
            unit: db.allocate_unit(),
            span: (0u32..1).into(),
        },
    };
    (id, ty, source)
}

fn populate(
    db: &mut Database,
    id: DeclId,
    source: DeclSource,
    ty: TypeId,
    supertypes: Vec<TypeId>,
) {
    let binders = match db.ty(ty) {
        Type::Quantified { binders, .. } => (0..binders.len())
            .map(|_| BinderSource {
                name: source.name.unwrap(),
                span: source.span,
                bound: None,
                default: None,
            })
            .collect(),
        _ => Default::default(),
    };
    db.populate(
        id,
        Declaration {
            source,
            ty,
            binders,
            supertypes: supertypes.into(),
        },
    );
}

fn nominal(db: &mut Database, name: &str, binders: Vec<Binder>, supers: Vec<TypeId>) -> TypeId {
    let (id, ty, source) = reserve(db, DeclKind::Class, name);
    let body = quantified(db, binders, ty);
    populate(db, id, source, body, supers);
    ty
}

fn alias(db: &mut Database, name: &str, ty: TypeId) -> TypeId {
    let (id, result, source) = reserve(db, DeclKind::Alias, name);
    populate(db, id, source, ty, vec![]);
    result
}

fn check(db: &Database, a: TypeId, b: TypeId) -> Outcome {
    let mut solver = Solver::new(db);
    solver.constrain(solver.closed(a), solver.closed(b), Provenance::default());
    solver.solve().remove(0)
}

fn has(outcome: &Outcome, issue: Issue) -> bool {
    outcome.diagnostics.iter().any(|d| d.issue == issue)
}

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
fn transparent_chains_and_nominal_transitivity() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let middle = nominal(&mut db, "Middle", vec![], vec![base]);
    let leaf = nominal(&mut db, "Leaf", vec![], vec![middle]);
    let wrapper = alias(&mut db, "A", leaf);
    let wrapper = alias(&mut db, "B", wrapper);
    let unrelated = nominal(&mut db, "Unrelated", vec![], vec![]);
    db.seal();
    assert_eq!(check(&db, wrapper, base).status, Status::Proven);
    assert_eq!(check(&db, base, leaf).status, Status::Contradicted);
    assert_eq!(check(&db, leaf, unrelated).status, Status::Contradicted);
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
fn variance_in_both_directions() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sub = nominal(&mut db, "Sub", vec![], vec![base]);
    let constructors: Vec<_> = [
        Variance::Covariant,
        Variance::Contravariant,
        Variance::Invariant,
    ]
    .into_iter()
    .map(|v| nominal(&mut db, "Generic", vec![binder(v)], vec![]))
    .collect();
    db.seal();
    for (constructor, forward, backward) in [
        (constructors[0], Status::Proven, Status::Contradicted),
        (constructors[1], Status::Contradicted, Status::Proven),
        (constructors[2], Status::Contradicted, Status::Contradicted),
    ] {
        let a = apply(&db, constructor, &[sub]);
        let b = apply(&db, constructor, &[base]);
        assert_eq!(check(&db, a, b).status, forward);
        assert_eq!(check(&db, b, a).status, backward);
        assert_eq!(check(&db, a, a).status, Status::Proven);
    }
}

#[test]
fn generic_inheritance_composes_open_substitutions() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let value = literal(&db, 1);
    let parent = nominal(&mut db, "Parent", vec![binder(Variance::Covariant)], vec![]);
    let container = nominal(
        &mut db,
        "Container",
        vec![binder(Variance::Covariant)],
        vec![],
    );
    let nested = apply(&db, container, &[r]);
    let supertype = apply(&db, parent, &[nested]);
    let middle = nominal(
        &mut db,
        "Middle",
        vec![binder(Variance::Covariant)],
        vec![supertype],
    );
    let supertype = apply(&db, middle, &[r]);
    let child = nominal(
        &mut db,
        "Child",
        vec![binder(Variance::Covariant)],
        vec![supertype],
    );
    let actual = apply(&db, child, &[r]);
    let expected = apply(&db, parent, &[apply(&db, container, &[value])]);
    db.seal();
    let mut s = Solver::new(&db);
    let e = s.environment(s.empty_environment(), vec![s.closed(value)]);
    s.constrain(s.view(actual, e), s.closed(expected), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
}

#[test]
fn generic_alias_instantiation_preserves_open_arguments() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let value = literal(&db, 7);
    let body = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        function(&db, &[r], r),
    );
    let a = alias(&mut db, "Identity", body);
    let actual = apply(&db, a, &[r]);
    let expected = function(&db, &[value], value);
    db.seal();
    let mut s = Solver::new(&db);
    let e = s.environment(s.empty_environment(), vec![s.closed(value)]);
    s.constrain(s.view(actual, e), s.closed(expected), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
}

#[test]
fn mro_first_match_wins_even_if_a_later_path_would_succeed() {
    let mut db = Database::new();
    let a = literal(&db, 1);
    let b = literal(&db, 2);
    let parent = nominal(&mut db, "Parent", vec![binder(Variance::Invariant)], vec![]);
    let pa = apply(&db, parent, &[a]);
    let pb = apply(&db, parent, &[b]);
    let left = nominal(&mut db, "Left", vec![], vec![pa]);
    let right = nominal(&mut db, "Right", vec![], vec![pb]);
    let diamond = nominal(&mut db, "Diamond", vec![], vec![left, right]);
    let reverse = nominal(&mut db, "Reverse", vec![], vec![right, left]);
    db.seal();
    assert_eq!(check(&db, diamond, pa).status, Status::Proven);
    assert_eq!(check(&db, diamond, pb).status, Status::Contradicted);
    assert_eq!(check(&db, reverse, pb).status, Status::Proven);
}

#[test]
fn repeated_nonmatching_diamond_is_not_a_cycle() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let a = nominal(&mut db, "A", vec![], vec![base]);
    let b = nominal(&mut db, "B", vec![], vec![base]);
    let c = nominal(&mut db, "C", vec![], vec![a, b]);
    let other = nominal(&mut db, "Other", vec![], vec![]);
    db.seal();
    assert_eq!(check(&db, c, base).status, Status::Proven);
    assert_eq!(check(&db, c, other).status, Status::Contradicted);
}

#[test]
fn unused_supertype_bounds_do_not_create_obligations() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let mut bounded = binder(Variance::Covariant);
    bounded.bound = Some(one);
    let other = nominal(&mut db, "Other", vec![bounded], vec![]);
    let other_one = apply(&db, other, &[one]);
    let target = nominal(&mut db, "Target", vec![], vec![]);
    let child = nominal(&mut db, "Child", vec![], vec![other_one, target]);
    db.seal();
    let mut s = Solver::new(&db);
    s.constrain(s.closed(child), s.closed(target), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
    // Searching the earlier, well-formed Other[1] branch must not generate 1 <: 1.
    assert_eq!(s.obligations.len(), 1);
}

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
fn fixed_function_variance_and_arity() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sub = nominal(&mut db, "Sub", vec![], vec![base]);
    let broad_input = function(&db, &[base], sub);
    let narrow_input = function(&db, &[sub], base);
    let nullary = function(&db, &[], sub);
    db.seal();
    assert_eq!(check(&db, broad_input, narrow_input).status, Status::Proven);
    assert_eq!(
        check(&db, narrow_input, broad_input).status,
        Status::Contradicted
    );
    assert_eq!(
        check(&db, broad_input, nullary).status,
        Status::Contradicted
    );
}

#[test]
fn ambient_channels_must_match() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let make = |input, output, result| {
        db.intern(Type::Function(Function {
            params: schema(&db, &[]),
            result,
            input,
            output,
        }))
    };
    let a = make(Some(one), Some(two), one);
    let b = make(Some(one), Some(two), db.top());
    let c = make(Some(two), Some(two), db.top());
    let d = make(None, Some(two), db.top());
    db.seal();
    assert_eq!(check(&db, a, b).status, Status::Proven);
    for other in [c, d] {
        assert!(has(&check(&db, a, other), Residual::AmbientChannels.into()));
    }
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
    for (a, b) in [
        (poly, mono),
        (variadic, mono),
        (optional, schema(&db, &[one])),
    ] {
        assert_eq!(check(&db, a, b).status, Status::Unresolved);
    }
    assert_eq!(check(&db, one, union).status, Status::Proven);
    assert_eq!(check(&db, a, b).status, Status::Proven);
    assert_eq!(check(&db, union, union).status, Status::Proven);
}

#[test]
fn structural_recursion_does_not_prove_constraints() {
    let mut db = Database::new();
    let (rid, r, rsrc) = reserve(&mut db, DeclKind::Alias, "Recursive");
    let body = function(&db, &[], r);
    populate(&mut db, rid, rsrc, body, vec![]);
    let (sid, s, ssrc) = reserve(&mut db, DeclKind::Alias, "OtherRecursive");
    let body = function(&db, &[], s);
    populate(&mut db, sid, ssrc, body, vec![]);
    db.seal();
    assert!(has(&check(&db, r, s), Residual::Recursive.into()));
}

#[test]
#[should_panic(expected = "transparent declaration cycle")]
fn alias_cycles_panic() {
    let mut db = Database::new();
    let (aid, a, asrc) = reserve(&mut db, DeclKind::Alias, "A");
    let (bid, b, bsrc) = reserve(&mut db, DeclKind::Alias, "B");
    populate(&mut db, aid, asrc, b, vec![]);
    populate(&mut db, bid, bsrc, a, vec![]);
    db.seal();
    check(&db, a, b);
}

#[test]
#[should_panic(expected = "transparent declaration cycle")]
fn generic_alias_cycles_panic() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let r = reference(&db, 0, 0);
    let (aid, a, asrc) = reserve(&mut db, DeclKind::Alias, "A");
    let (bid, b, bsrc) = reserve(&mut db, DeclKind::Alias, "B");
    let body = quantified(&db, vec![binder(Variance::Covariant)], apply(&db, b, &[r]));
    populate(&mut db, aid, asrc, body, vec![]);
    let body = quantified(&db, vec![binder(Variance::Covariant)], apply(&db, a, &[r]));
    populate(&mut db, bid, bsrc, body, vec![]);
    let applied = apply(&db, a, &[int]);
    db.seal();
    check(&db, applied, int);
}

#[test]
fn nested_generic_alias_applications_are_not_cycles() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let r = reference(&db, 0, 0);
    let body = quantified(&db, vec![binder(Variance::Covariant)], r);
    let id = alias(&mut db, "Id", body);
    let inner = apply(&db, id, &[int]);
    let outer = apply(&db, id, &[inner]);
    let chained = alias(&mut db, "Chained", outer);
    db.seal();
    assert_eq!(check(&db, outer, int).status, Status::Proven);
    assert_eq!(check(&db, chained, int).status, Status::Proven);
}

#[test]
fn cyclic_and_expanding_inheritance_remain_residual() {
    let mut db = Database::new();
    let (id, ty, source) = reserve(&mut db, DeclKind::Class, "Recursive");
    let r = reference(&db, 0, 0);
    let nested = apply(&db, ty, &[r]);
    let supertype = apply(&db, ty, &[nested]);
    let body = quantified(&db, vec![binder(Variance::Covariant)], ty);
    populate(&mut db, id, source, body, vec![supertype]);
    let other = nominal(&mut db, "Other", vec![], vec![]);
    let applied = apply(&db, ty, &[db.top()]);
    db.seal();
    assert!(has(&check(&db, applied, other), Residual::Recursive.into()));
}

#[test]
fn unsupported_generic_matching_is_residual() {
    let mut db = Database::new();
    let generic = nominal(
        &mut db,
        "Generic",
        vec![binder(Variance::Covariant)],
        vec![],
    );
    let sym = db.intern_symbol("T");
    let keyed = db.intern(Type::Apply {
        base: generic,
        args: vec![Argument::Keyword(sym, db.top())].into(),
        kind: Kind::Type,
    });
    db.seal();
    assert!(has(
        &check(&db, keyed, keyed),
        Residual::GenericArguments.into()
    ));
}

#[test]
#[should_panic(expected = "generic argument arity mismatch")]
fn generic_arity_mismatch_panics() {
    let mut db = Database::new();
    let generic = nominal(
        &mut db,
        "Generic",
        vec![binder(Variance::Covariant)],
        vec![],
    );
    let empty = apply(&db, generic, &[]);
    db.seal();
    check(&db, empty, empty);
}

#[test]
#[should_panic(expected = "generic argument arity mismatch")]
fn applying_a_non_generic_type_panics() {
    let mut db = Database::new();
    let plain = nominal(&mut db, "Plain", vec![], vec![]);
    let applied = apply(&db, plain, &[db.top()]);
    db.seal();
    check(&db, applied, applied);
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
    assert_eq!(s.obligations.len(), 2);
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
fn incomplete_earlier_inheritance_cannot_be_skipped() {
    let mut db = Database::new();
    let target = nominal(&mut db, "Target", vec![], vec![]);
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let unsupported = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(two)].into(),
    ));
    let child = nominal(&mut db, "Child", vec![], vec![unsupported, target]);
    db.seal();
    assert_eq!(check(&db, child, target).status, Status::Unresolved);
}

#[test]
fn mro_commits_to_an_inference_path_without_combining_alternatives() {
    let mut db = Database::new();
    let r = reference(&db, 0, 0);
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let parent = nominal(&mut db, "Parent", vec![binder(Variance::Invariant)], vec![]);
    let first = apply(&db, parent, &[r]);
    let second = apply(&db, parent, &[two]);
    let child = nominal(
        &mut db,
        "Child",
        vec![binder(Variance::Covariant)],
        vec![first, second],
    );
    let actual = apply(&db, child, &[r]);
    let expected = apply(&db, parent, &[one]);
    db.seal();
    let mut s = Solver::new(&db);
    let variable = s.infer();
    let env = s.intern_environment(s.empty_environment(), vec![variable]);
    s.constrain(
        s.view(actual, env),
        s.closed(expected),
        Provenance::default(),
    );
    assert_eq!(s.solve()[0].status, Status::Proven);
    let Term::Infer(id) = variable else {
        unreachable!()
    };
    assert_eq!(s.bounds(id).lower().count(), 1);
    assert_eq!(s.bounds(id).upper().count(), 1);
    let bounds: Vec<_> = s.bounds(id).lower().chain(s.bounds(id).upper()).collect();
    for bound in bounds {
        assert!(s.same(bound, s.closed(one)).unwrap());
    }
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
fn omitted_generic_defaults_are_not_arity_errors() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let mut parameter = binder(Variance::Covariant);
    parameter.default = Some(one);
    let constructor = nominal(&mut db, "Defaulted", vec![parameter], vec![]);
    let omitted = apply(&db, constructor, &[]);
    let supplied = apply(&db, constructor, &[one]);
    db.seal();
    let result = check(&db, omitted, supplied);
    assert_eq!(result.status, Status::Unresolved);
    assert!(has(&result, Residual::GenericArguments.into()));
    assert_eq!(check(&db, supplied, supplied).status, Status::Proven);
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

#[test]
fn functions_have_intrinsic_nominal_supertype() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let func = nominal(&mut db, "Func", vec![], vec![base]);
    let unrelated = nominal(&mut db, "Other", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Func, func);
    let plain = function(&db, &[db.top()], db.top());
    let wrapped = alias(&mut db, "Callable", plain);
    let r = reference(&db, 0, 0);
    let generic = quantified(
        &db,
        vec![binder(Variance::Covariant)],
        function(&db, &[r], r),
    );
    db.seal();
    for ty in [plain, wrapped, generic] {
        assert_eq!(check(&db, ty, func).status, Status::Proven);
        assert_eq!(check(&db, ty, base).status, Status::Proven);
        assert_eq!(check(&db, ty, unrelated).status, Status::Contradicted);
    }
    // A nominal Func carries no signature from which to prove this direction.
    assert_eq!(check(&db, func, plain).status, Status::Unresolved);
}

#[test]
fn missing_func_intrinsic_is_residual() {
    let mut db = Database::new();
    let nominal = nominal(&mut db, "Func", vec![], vec![]);
    let function = function(&db, &[], db.top());
    db.seal();
    let result = check(&db, function, nominal);
    assert_eq!(result.status, Status::Unresolved);
    assert!(has(
        &result,
        Residual::MissingIntrinsic(Intrinsic::Func).into()
    ));
}

fn variable_id(term: Term) -> InferVarId {
    let Term::Infer(id) = term else {
        panic!("expected inference variable")
    };
    id
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
    let params = schema(&db, &[]);
    let function = function(&db, &[], one);
    let expanded = db.intern(Type::Union(vec![UnionMember::Expand(params)].into()));
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
