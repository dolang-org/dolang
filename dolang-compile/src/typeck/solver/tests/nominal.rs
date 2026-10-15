use super::*;

fn union(db: &Database, members: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        members.iter().copied().map(UnionMember::Type).collect(),
    ))
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
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
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
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
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
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
    s.constrain(s.closed(child), s.closed(target), Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Proven);
    // Searching the earlier, well-formed Other[1] branch must not generate 1 <: 1.
    assert_eq!(s.obligation_count(), 1);
}

#[test]
fn alias_cycles_are_recursive() {
    let mut db = Database::new();
    let (aid, a, asrc) = reserve(&mut db, DeclKind::Alias, "A");
    let (bid, b, bsrc) = reserve(&mut db, DeclKind::Alias, "B");
    populate(&mut db, aid, asrc, b, vec![]);
    populate(&mut db, bid, bsrc, a, vec![]);
    db.seal();
    let outcome = check(&db, a, b);
    assert!(has(&outcome, Residual::Recursive.into()));
}

#[test]
fn generic_alias_cycles_are_recursive() {
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
    let outcome = check(&db, applied, int);
    assert!(has(&outcome, Residual::Recursive.into()));
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
    // What can't be exposed is still below top and the dynamic type
    for expected in [db.top(), db.unknown()] {
        assert_eq!(check(&db, keyed, expected).status, Status::Proven);
    }
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
    let gas = Gas::default();
    let mut s = Solver::new(&db, &gas);
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
    assert!(has(&result, Residual::Malformed.into()));
    assert_eq!(check(&db, supplied, supplied).status, Status::Proven);
}

#[test]
fn metatypes_reach_class_objects_and_relate_only_to_their_own() {
    let mut db = Database::new();
    let ty = nominal(&mut db, "Type", vec![binder(Variance::Covariant)], vec![]);
    db.set_intrinsic(Intrinsic::Type, ty);
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sub = nominal(&mut db, "Sub", vec![], vec![base]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let meta = |class, args: &[TypeId]| {
        let &Type::Decl(decl) = db.ty(class) else {
            unreachable!("a class")
        };
        db.meta(
            decl,
            args.iter().copied().map(Argument::Positional).collect(),
        )
    };
    let base_meta = meta(base, &[]);
    let sub_meta = meta(sub, &[]);
    let box_base = meta(boxed, &[base]);
    let box_sub = meta(boxed, &[sub]);
    let base_object = apply(&db, ty, &[base]);
    let box_base_object = apply(&db, ty, &[apply(&db, boxed, &[base])]);
    db.seal();
    // A metatype reaches its class's object, and so its supertypes' objects
    for (actual, expected) in [
        (base_meta, base_meta),
        (base_meta, base_object),
        (sub_meta, base_object),
        (box_sub, box_base_object),
    ] {
        assert_eq!(check(&db, actual, expected).status, Status::Proven);
    }
    // No other class's metatype or class object is exactly the class's object,
    // and a metatype is invariant in its arguments
    for (actual, expected) in [
        (sub_meta, base_meta),
        (base_object, base_meta),
        (box_sub, box_base),
        (box_base, box_sub),
    ] {
        assert_eq!(check(&db, actual, expected).status, Status::Contradicted);
    }
    // Different metatypes' least common supertype is what they reach in common
    let gas = Gas::default();
    let s = Solver::new(&db, &gas);
    let joined = s.lub(sub_meta, base_meta);
    assert_eq!(s.common_supertype(joined), Some(base_object));
}

#[test]
fn unreached_generic_members_refute_any_class() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Invariant)], vec![]);
    let leaf = final_class(&mut db, "Leaf", vec![]);
    let open = nominal(&mut db, "Open", vec![], vec![]);
    let members = union(&db, &[int, apply(&db, boxed, &[int])]);
    db.seal();
    // An `Open` of its own, or of a subclass inheriting only it, is no `Box`
    for class in [leaf, open, str] {
        let outcome = check(&db, class, members);
        assert_eq!(outcome.status, Status::Contradicted);
        assert!(has(&outcome, Issue::Contradiction(Contradiction::Outside)));
    }
}

#[test]
fn reached_generic_members_refute_only_alone() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let covariant = || binder(Variance::Covariant);
    let boxed = nominal(&mut db, "Box", vec![covariant()], vec![]);
    let g = nominal(&mut db, "G", vec![covariant()], vec![]);
    let h = nominal(&mut db, "H", vec![covariant()], vec![]);
    let either = union(&db, &[int, str]);
    let box_str = apply(&db, boxed, &[str]);
    let open = nominal(&mut db, "Open", vec![], vec![box_str]);
    let supers = vec![apply(&db, g, &[either]), apply(&db, h, &[either])];
    let both = nominal(&mut db, "Both", vec![], supers);
    let box_int = apply(&db, boxed, &[int]);
    let one = union(&db, &[int, box_int]);
    let boxes = union(&db, &[box_int, box_str]);
    let split = union(&db, &[apply(&db, g, &[int]), apply(&db, h, &[str])]);
    db.seal();
    // No other member collects the `Open`s that aren't `Box[Int]`s
    let outcome = check(&db, open, one);
    assert_eq!(outcome.status, Status::Contradicted);
    assert!(has(&outcome, Issue::Contradiction(Contradiction::Outside)));
    // Every `Box[Int | Str]` may be a `Box[Int]` or a `Box[Str]`
    let mixed = apply(&db, boxed, &[either]);
    assert_eq!(check(&db, mixed, boxes).status, Status::Unresolved);
    // A `Both` may be a `G[Int]` or an `H[Str]`
    assert_eq!(check(&db, both, split).status, Status::Unresolved);
}
