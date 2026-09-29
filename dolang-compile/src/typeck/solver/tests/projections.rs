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
