use super::*;

fn sym(db: &Database, name: &str) -> TypeId {
    db.intern(Type::Literal(Literal::Sym(db.intern_symbol(name))))
}

fn union(db: &Database, types: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        types.iter().copied().map(UnionMember::Type).collect(),
    ))
}

/// A union of one projection of `schema`
fn projection(db: &Database, member: fn(TypeId) -> UnionMember, schema: TypeId) -> TypeId {
    db.intern(Type::Union(vec![member(schema)].into()))
}

fn schema_reference(db: &Database, slot: usize) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(0, slot),
        kind: Kind::Schema,
    })
}

#[test]
fn projections_wait_for_their_schema() {
    let db = &mut Database::new();
    let (a, b, c) = (sym(db, "a"), sym(db, "b"), sym(db, "c"));
    let (one, two) = (literal(db, 1), literal(db, 2));
    let closed = items(
        db,
        vec![
            keyed(Multiplicity::Required, a, one),
            keyed(Multiplicity::Required, b, two),
        ],
    );
    let keys = projection(db, UnionMember::Keys, schema_reference(db, 0));
    db.seal();
    for (key, status) in [(a, Status::Proven), (c, Status::Contradicted)] {
        let mut s = Solver::new(db);
        let u = s.infer_kind(Kind::Schema, Rest::All);
        let env = s.environment(s.empty_environment(), vec![u]);
        s.constrain(s.closed(key), s.view(keys, env), Provenance::default());
        let waiting = s.solve().remove(0);
        assert_eq!(waiting.status, Status::Unresolved);
        assert!(has(&waiting, Residual::Inference.into()));
        s.constrain(s.closed(closed), u, Provenance::default());
        s.constrain(u, s.closed(closed), Provenance::default());
        let outcome = s.solve().remove(0);
        assert_eq!(outcome.status, status, "{key:?}");
        if status == Status::Contradicted {
            assert!(contradiction(&outcome, Contradiction::Outside));
        }
    }
}

#[test]
fn a_projection_of_a_rigid_is_below_that_of_its_bound() {
    let db = &mut Database::new();
    let (a, b) = (sym(db, "a"), sym(db, "b"));
    let (one, two) = (literal(db, 1), literal(db, 2));
    let bound = items(
        db,
        vec![
            keyed(Multiplicity::Required, a, one),
            keyed(Multiplicity::Optional, b, two),
        ],
    );
    let keys = projection(db, UnionMember::Keys, schema_reference(db, 0));
    let values = projection(db, UnionMember::Values, schema_reference(db, 0));
    let ab = union(db, &[a, b]);
    let one_two = union(db, &[one, two]);
    let decl = generic(
        db,
        vec![bounded(Kind::Schema, Binding::Positional, Some(bound))],
        db.top(),
    );
    db.seal();
    assert_eq!(under(db, decl, keys, ab).status, Status::Proven);
    assert_eq!(under(db, decl, values, one_two).status, Status::Proven);
    // `S` may have both keys, so its keys aren't only `:a:`
    assert!(contradiction(
        &under(db, decl, keys, a),
        Contradiction::DistinctLiterals
    ));
    // A projection is only ever itself above
    assert_eq!(under(db, decl, keys, keys).status, Status::Proven);
    assert_eq!(under(db, decl, a, keys).status, Status::Unresolved);
}

#[test]
fn a_value_outside_every_member_is_outside_the_union() {
    let db = &mut Database::new();
    let int = int(db);
    let sym_class = nominal(db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym_class);
    let str_class = nominal(db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str_class);
    let bool_class = nominal(db, "Bool", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Bool, bool_class);
    let (a, b) = (sym(db, "a"), sym(db, "b"));
    let (one, two, three) = (literal(db, 1), literal(db, 2), literal(db, 3));
    let ab = union(db, &[a, b]);
    let one_two = union(db, &[one, two]);
    let one_or_str = union(db, &[one, str_class]);
    let (yes, no) = (
        db.intern(Type::Literal(Literal::Bool(true))),
        db.intern(Type::Literal(Literal::Bool(false))),
    );
    let bools = union(db, &[yes, no]);
    db.seal();
    for (actual, expected) in [
        (three, one_two),
        (sym_class, ab),
        (int, one_or_str),
        (str_class, one_two),
    ] {
        assert!(
            contradiction(&check(db, actual, expected), Contradiction::Outside),
            "{actual:?} <: {expected:?}"
        );
    }
    assert_eq!(check(db, one, one_or_str).status, Status::Proven);
    // `Bool` has only its two literals, so they may cover it
    assert_eq!(check(db, bool_class, bools).status, Status::Unresolved);
}

#[test]
fn a_union_expansion_is_evaluated_once_substituted() {
    let db = &mut Database::new();
    let (one, two, three) = (literal(db, 1), literal(db, 2), literal(db, 3));
    let pack = schema(db, &[one, two]);
    let expanded = projection(db, UnionMember::Expand, schema_reference(db, 0));
    let one_two = union(db, &[one, two]);
    db.seal();
    for (expected, status) in [(one_two, Status::Proven), (three, Status::Contradicted)] {
        let mut s = Solver::new(db);
        let env = s.environment(s.empty_environment(), vec![s.closed(pack)]);
        s.constrain(
            s.view(expanded, env),
            s.closed(expected),
            Provenance::default(),
        );
        assert_eq!(s.solve().remove(0).status, status);
    }
}

/// A union of one item projection
fn selecting(db: &Database, meet: bool, schema: TypeId, key: TypeId) -> TypeId {
    let member = match meet {
        false => UnionMember::IndexItem(schema, key),
        true => UnionMember::AssignItem(schema, key),
    };
    db.intern(Type::Union(vec![member].into()))
}

/// What an item projection evaluates to, or the issue evaluating it raises
fn evaluated(db: &Database, projection: TypeId) -> Result<TypeId, Issue> {
    let s = Solver::new(db);
    Ok(s.evaluate_items(projection)?.expect("an item projection"))
}

#[test]
fn a_key_selects_items_positions_by_their_indexes() {
    let db = &mut Database::new();
    let int = int(db);
    let str = nominal(db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let sym = nominal(db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let (a, b, c) = (self::sym(db, "a"), self::sym(db, "b"), self::sym(db, "c"));
    let (zero, five) = (literal(db, 0), literal(db, 5));
    // `{Str, a: Int, b: Str}`
    let fixed = items(
        db,
        vec![
            positional(Multiplicity::Required, str),
            keyed(Multiplicity::Required, a, int),
            keyed(Multiplicity::Required, b, str),
        ],
    );
    // `{Str, *Int}`
    let varying = items(
        db,
        vec![
            positional(Multiplicity::Required, str),
            positional(Multiplicity::Repeated, int),
        ],
    );
    // `{*(Sym): Int, ...}`
    let open = items(
        db,
        vec![
            keyed(Multiplicity::Repeated, sym, int),
            keyed(Multiplicity::Repeated, db.top(), db.top()),
        ],
    );
    let ab = union(db, &[a, b]);
    let int_str = union(db, &[int, str]);
    let fresh_zero = fresh(db, 0);
    db.seal();
    let cases = [
        (false, fixed, a, Ok(int)),
        (false, fixed, zero, Ok(str)),
        // A fresh literal key selects as its regular twin
        (false, fixed, fresh_zero, Ok(str)),
        (false, fixed, ab, Ok(int_str)),
        (true, fixed, a, Ok(int)),
        // Nothing is both an `Int` and a `Str`
        (true, fixed, ab, Ok(db.bottom())),
        (false, fixed, c, Err(Contradiction::Unadmitted(c))),
        (false, fixed, five, Err(Contradiction::Unadmitted(five))),
        (false, fixed, sym, Err(Contradiction::Unadmitted(sym))),
        // Positions from the first repeated one on are an `Int` domain
        (false, varying, five, Ok(int)),
        (false, varying, int, Ok(int_str)),
        // The narrowest domain owns a key
        (false, open, c, Ok(int)),
        (false, open, zero, Ok(db.top())),
    ];
    for (meet, schema, key, expected) in cases {
        let projection = selecting(db, meet, schema, key);
        let expected = expected.map_err(Issue::Contradiction);
        assert_eq!(
            evaluated(db, projection),
            expected,
            "{meet} {schema:?} {key:?}"
        );
    }
}

#[test]
fn item_projections_meet_without_intersections() {
    let db = &mut Database::new();
    let int = int(db);
    let [p, q] = ["P", "Q"].map(|name| {
        let (id, ty, source) = reserve(db, DeclKind::Protocol, name);
        populate(db, id, source, ty, vec![]);
        ty
    });
    let (a, b) = (sym(db, "a"), sym(db, "b"));
    let (one, two) = (literal(db, 1), literal(db, 2));
    let ab = union(db, &[a, b]);
    let one_two = union(db, &[one, two]);
    let schema = |db: &Database, x, y| {
        items(
            db,
            vec![
                keyed(Multiplicity::Required, a, x),
                keyed(Multiplicity::Required, b, y),
            ],
        )
    };
    let ordered = schema(db, int, one_two);
    let literals = schema(db, one, two);
    let protocols = schema(db, p, q);
    db.seal();
    for (schema, expected) in [
        (ordered, one_two),
        (literals, db.bottom()),
        // Protocols may share a value, which takes an intersection
        (protocols, db.unknown()),
    ] {
        let projection = selecting(db, true, schema, ab);
        assert_eq!(evaluated(db, projection), Ok(expected), "{schema:?}");
    }
}

#[test]
fn item_projections_wait_for_their_key_and_report_on_exposure() {
    let db = &mut Database::new();
    let int = int(db);
    let (a, c) = (sym(db, "a"), sym(db, "c"));
    let schema = items(db, vec![keyed(Multiplicity::Required, a, int)]);
    let key = reference(db, 0, 0);
    let projection = selecting(db, false, schema, key);
    // `{Int, 0: Int}`, whose position and key collide
    let zero = literal(db, 0);
    let colliding = items(
        db,
        vec![
            positional(Multiplicity::Required, int),
            keyed(Multiplicity::Required, zero, int),
        ],
    );
    let conflicted = selecting(db, false, colliding, zero);
    db.seal();
    for (given, status) in [(a, Status::Proven), (c, Status::Contradicted)] {
        let mut s = Solver::new(db);
        let k = s.infer();
        let env = s.environment(s.empty_environment(), vec![k]);
        let bottom = s.closed(db.bottom());
        s.constrain(bottom, s.view(projection, env), Provenance::default());
        let waiting = s.solve().remove(0);
        assert!(has(&waiting, Residual::Inference.into()), "{waiting:?}");
        s.constrain(s.closed(given), k, Provenance::default());
        s.solve();
        let outcome = default_all(&mut s).remove(0);
        assert_eq!(outcome.status, status, "{given:?} {outcome:?}");
        if status == Status::Contradicted {
            assert!(contradiction(&outcome, Contradiction::Unadmitted(c)));
        }
    }
    let outcome = check(db, db.bottom(), conflicted);
    assert!(
        contradiction(&outcome, Contradiction::Conflict),
        "{outcome:?}"
    );
}

#[test]
fn item_projections_relate_before_they_can_be_evaluated() {
    let db = &mut Database::new();
    let int = int(db);
    let str = nominal(db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let (a, b) = (sym(db, "a"), sym(db, "b"));
    let ab = union(db, &[a, b]);
    let int_str = union(db, &[int, str]);
    // `S @ {a: Int, ?b: (Int | Str)}` and `K @ (:a: | :b:)`, where `AssignItem` by
    // the bound's keys is `Int`, not `Bottom`
    let bound = items(
        db,
        vec![
            keyed(Multiplicity::Required, a, int),
            keyed(Multiplicity::Optional, b, int_str),
        ],
    );
    let decl = generic(
        db,
        vec![
            bounded(Kind::Schema, Binding::Positional, Some(bound)),
            bounded(Kind::Type, Binding::Positional, Some(ab)),
        ],
        db.top(),
    );
    let (s, k) = (schema_reference(db, 0), reference(db, 0, 1));
    let same = items(
        db,
        vec![
            keyed(Multiplicity::Required, a, int),
            keyed(Multiplicity::Required, b, int),
        ],
    );
    let [read, read_a, read_ab] = [k, a, ab].map(|key| selecting(db, false, s, key));
    let [write_a, write_ab] = [a, ab].map(|key| selecting(db, true, s, key));
    let read_same = selecting(db, false, same, k);
    let write_same = selecting(db, true, same, k);
    let read_mixed = selecting(db, false, bound, k);
    db.seal();
    for (x, y) in [
        (read, read),
        // A rigid schema's projection is below that of its bound
        (read_a, int),
        (read_ab, int_str),
        // `IndexItem` is monotone in its key, and `AssignItem` antitone
        (read_a, read_ab),
        (write_ab, write_a),
        // Where every item a rigid key's bound selects has the same value, the
        // key selects it
        (read_same, int),
        (int, write_same),
    ] {
        let outcome = under(db, decl, x, y);
        assert_eq!(
            outcome.status,
            Status::Proven,
            "{x:?} <: {y:?}: {outcome:?}"
        );
    }
    // Otherwise a rigid key selects too little to decide
    let outcome = under(db, decl, read_mixed, int_str);
    assert_eq!(outcome.status, Status::Unresolved, "{outcome:?}");
    // A key selecting more is not below one selecting less
    let outcome = under(db, decl, read_ab, read_a);
    assert_ne!(outcome.status, Status::Proven, "{outcome:?}");
}

#[test]
fn a_class_outside_scalar_members_is_outside_the_union() {
    let mut db = Database::new();
    let int = int(&mut db);
    let nil_class = nominal(&mut db, "Nil", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Nil, nil_class);
    let nil = db.intern(Type::Literal(Literal::Nil));
    let other = nominal(&mut db, "Other", vec![], vec![]);
    let scalars = union(&db, &[nil, int]);
    db.seal();
    assert!(contradiction(
        &check(&db, other, scalars),
        Contradiction::Outside
    ));
    assert_eq!(check(&db, nil, scalars).status, Status::Proven);
    assert_eq!(check(&db, nil_class, scalars).status, Status::Proven);
}

#[test]
fn generic_union_alternatives_remain_conservative() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let actual = apply(&db, array, &[int]);
    let alternative = apply(&db, array, &[str]);
    let expected = union(&db, &[int, alternative]);
    db.seal();
    assert_eq!(check(&db, actual, expected).status, Status::Unresolved);
}
