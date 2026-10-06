use super::*;

fn assert_panics(f: impl FnOnce()) {
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err());
}

fn intern(db: &mut Database, ty: Type) -> TypeId {
    db.intern(ty)
}

fn reference(db: &mut Database, depth: usize, slot: usize, kind: Kind) -> TypeId {
    intern(
        db,
        Type::Bound {
            reference: BoundRef::new(depth, slot),
            kind,
        },
    )
}

fn binder(kind: Kind) -> Binder {
    Binder {
        kind,
        binding: Binding::Positional,
        bound: None,
        default: None,
        variance: Variance::Invariant,
    }
}

fn quantify(db: &mut Database, binders: Vec<Binder>, body: TypeId) -> TypeId {
    intern(
        db,
        Type::Quantified {
            binders: binders.into(),
            body,
        },
    )
}

fn schema(db: &mut Database, types: &[TypeId]) -> TypeId {
    intern(
        db,
        Type::Schema(
            types
                .iter()
                .map(|&ty| SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Positional(ty),
                })
                .collect(),
        ),
    )
}

fn function(db: &mut Database, types: &[TypeId], result: TypeId) -> TypeId {
    let params = schema(db, types);
    intern(
        db,
        Type::Function(Function {
            params,
            result,
            input: None,
            output: None,
        }),
    )
}

fn union(db: &mut Database, types: &[TypeId]) -> TypeId {
    intern(
        db,
        Type::Union(types.iter().copied().map(UnionMember::Type).collect()),
    )
}

fn source(db: &mut Database, kind: DeclKind, name: &str) -> DeclSource {
    let unit = db.allocate_unit();
    let name = Some(db.intern_symbol(name));
    DeclSource {
        kind,
        result_kind: Kind::Type,
        name,
        span: UnitSpan {
            unit,
            span: (0u32..10).into(),
        },
    }
}

fn declare(db: &mut Database, kind: DeclKind, name: &str) -> (DeclId, TypeId, DeclSource) {
    let source = source(db, kind, name);
    let id = db.allocate();
    let ty = intern(db, Type::Decl(id));
    (id, ty, source)
}

fn definition(source: DeclSource, ty: TypeId) -> Declaration {
    Declaration {
        source,
        ty,
        binders: alias::Box::default(),
        supertypes: alias::Box::default(),
        members: alias::Box::default(),
    }
}

#[test]
fn structural_identity_is_not_source_identity() {
    let mut db = Database::new();
    let src = source(&mut db, DeclKind::Annotation, "x");
    let a = db.allocate();
    let b = db.allocate();
    assert_ne!(a, b);
    let literal = intern(&mut db, Type::Literal(Literal::Int(42)));
    assert_eq!(literal, intern(&mut db, Type::Literal(Literal::Int(42))));
    db.populate(a, definition(src.clone(), literal));
    db.populate(b, definition(src.clone(), literal));
    let ar = intern(&mut db, Type::Decl(a));
    let br = intern(&mut db, Type::Decl(b));
    assert_ne!(ar, br);
    assert_eq!(db.expose(ar).unwrap().ty, literal);
    assert_eq!(db.expose(br).unwrap().ty, literal);
    assert_ne!(union(&mut db, &[ar, br]), literal);
}

#[test]
fn literals_are_exact_values_not_builtin_types() {
    let mut db = Database::new();
    let sym = db.intern_symbol("ready");
    let values = [
        Literal::Nil,
        Literal::Bool(false),
        Literal::Bool(true),
        Literal::Int(0),
        Literal::Int(i128::MIN),
        Literal::Int(i128::MAX),
        Literal::Str("ready".into()),
        Literal::Sym(sym),
    ];
    let types: Vec<_> = values
        .iter()
        .map(|v| intern(&mut db, Type::Literal(v.clone())))
        .collect();
    assert_eq!(types.iter().collect::<HashSet<_>>().len(), values.len());
    for (literal, ty) in values.into_iter().zip(types) {
        assert_eq!(intern(&mut db, Type::Literal(literal)), ty);
    }
    let (int, int_ty, int_source) = declare(&mut db, DeclKind::Class, "Int");
    db.populate(int, definition(int_source.clone(), int_ty));
    assert_eq!(db.expose(int_ty).unwrap().ty, int_ty);
}

#[test]
fn decay_keeps_regular_literals_exact_keys_and_binder_bounds() {
    let mut db = Database::new();
    let (int, int_ty, int_source) = declare(&mut db, DeclKind::Class, "Int");
    db.populate(int, definition(int_source, int_ty));
    db.set_intrinsic(Intrinsic::Int, int_ty);
    let key = db.intern_symbol("name");
    let key = intern(&mut db, Type::Literal(Literal::Sym(key)));
    let one = intern(&mut db, Type::Fresh(Literal::Int(1)));
    let two = intern(&mut db, Type::Fresh(Literal::Int(2)));
    let exact = intern(&mut db, Type::Literal(Literal::Int(1)));

    // Fresh literals decay through unions and function types
    let both = union(&mut db, &[one, two]);
    assert_eq!(db.decay(both), int_ty);
    let f = function(&mut db, &[one], two);
    let decayed = function(&mut db, &[int_ty], int_ty);
    assert_eq!(db.decay(f), decayed);

    // A regular literal was written in a type, as a parameter's annotation is
    assert_eq!(db.decay(exact), exact);
    let g = function(&mut db, &[exact], two);
    let decayed = function(&mut db, &[exact], int_ty);
    assert_eq!(db.decay(g), decayed);

    // An exact key keeps its literal
    let keyed = |db: &mut Database, value| {
        intern(
            db,
            Type::Schema(
                vec![SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Keyed { key, value },
                }]
                .into(),
            ),
        )
    };
    let record = keyed(&mut db, one);
    let decayed = keyed(&mut db, int_ty);
    assert_eq!(db.decay(record), decayed);

    // A binder's bound keeps its literal; the body decays
    let t = reference(&mut db, 0, 0, Kind::Type);
    let bounded = |db: &mut Database, result| {
        let body = function(db, &[t], result);
        quantify(
            db,
            vec![Binder {
                bound: Some(both),
                ..binder(Kind::Type)
            }],
            body,
        )
    };
    let generic = bounded(&mut db, one);
    let decayed = bounded(&mut db, int_ty);
    assert_eq!(db.decay(generic), decayed);
}

#[test]
fn fresh_literals_give_way_to_their_regular_twins() {
    let mut db = Database::new();
    let fresh = intern(&mut db, Type::Fresh(Literal::Int(1)));
    let regular = intern(&mut db, Type::Literal(Literal::Int(1)));
    let two = intern(&mut db, Type::Fresh(Literal::Int(2)));
    assert_ne!(fresh, regular);
    assert_eq!(db.literal(fresh), Some(&Literal::Int(1)));
    assert_eq!(db.literal(regular), Some(&Literal::Int(1)));
    assert_eq!(db.regular(fresh), regular);
    assert_eq!(db.regular(regular), regular);
    let top = db.top();
    assert_eq!(db.regular(top), top);
    // A union keeps the regular twin, whatever the order
    assert_eq!(union(&mut db, &[fresh, regular]), regular);
    assert_eq!(union(&mut db, &[regular, fresh]), regular);
    let kept = union(&mut db, &[regular, two]);
    assert_eq!(union(&mut db, &[fresh, two, regular]), kept);
}

#[test]
fn symbols_are_interned_independently_of_units() {
    let mut db = Database::new();
    let mut locals = Vec::new();
    for _ in 0..2 {
        let unit = db.allocate_unit();
        let merged = db.intern_symbol("key");
        let unique = db.fresh_symbol("key");
        assert_eq!(db.symbol(merged), "key");
        assert_eq!(db.symbol(unique), "key");
        assert_ne!(merged, unique);
        locals.push((unit, merged, unique));
    }
    assert_eq!(locals[0].1, locals[1].1);
    assert_ne!(locals[0].2, locals[1].2);
    let value = intern(&mut db, Type::Top);
    let mut types = Vec::new();
    for (unit, key, _) in locals {
        let key_ty = intern(&mut db, Type::Literal(Literal::Sym(key)));
        let ty = intern(
            &mut db,
            Type::Schema(
                vec![SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Keyed { key: key_ty, value },
                }]
                .into(),
            ),
        );
        let decl = db.allocate();
        let decl_source = DeclSource {
            kind: DeclKind::Alias,
            result_kind: Kind::Schema,
            name: Some(key),
            span: UnitSpan {
                unit,
                span: (2u32..7).into(),
            },
        };
        db.populate(decl, definition(decl_source.clone(), ty));
        types.push((decl, ty));
    }
    assert_eq!(types[0].1, types[1].1);
    assert_ne!(
        db.declaration(types[0].0).source.span.unit,
        db.declaration(types[1].0).source.span.unit
    );
}

#[test]
fn declaration_lifecycle_and_post_seal_interning() {
    let mut db = Database::new();
    let src = source(&mut db, DeclKind::Alias, "A");
    let id = db.allocate();
    let r = intern(&mut db, Type::Decl(id));
    assert_panics(|| {
        let _ = db.expose(r);
    });
    assert_panics(|| {
        db.seal();
    });
    let top = db.top();
    db.populate(id, definition(src.clone(), top));
    assert_panics(|| {
        db.populate(id, definition(src.clone(), top));
    });
    db.seal();
    assert_panics(|| db.seal());
    assert_panics(|| {
        db.allocate();
    });
    assert_panics(|| {
        db.allocate_unit();
    });
    assert_panics(|| {
        db.populate(id, definition(src.clone(), top));
    });
    let later = db.intern_symbol("later");
    assert_eq!(db.symbol(later), "later");
    let bottom = db.bottom();
    assert_eq!(union(&mut db, &[top, bottom]), top);
    assert_eq!(
        db.expose(r).unwrap(),
        Exposure {
            ty: top,
            declarations: vec![id]
        }
    );
}

#[test]
fn exposure_retains_chains_and_reports_only_head_cycles() {
    let mut db = Database::new();
    let (a, ar, a_source) = declare(&mut db, DeclKind::Annotation, "annotation");
    let (b, br, b_source) = declare(&mut db, DeclKind::Alias, "Alias");
    let (c, cr, c_source) = declare(&mut db, DeclKind::Class, "Class");
    db.populate(a, definition(a_source.clone(), br));
    db.populate(b, definition(b_source.clone(), cr));
    db.populate(c, definition(c_source.clone(), cr));
    assert_eq!(db.deref_one(ar), Some(br));
    assert_eq!(
        db.expose(ar).unwrap(),
        Exposure {
            ty: cr,
            declarations: vec![a, b]
        }
    );

    let (x, xr, x_source) = declare(&mut db, DeclKind::Alias, "X");
    let (y, yr, y_source) = declare(&mut db, DeclKind::Alias, "Y");
    db.populate(x, definition(x_source.clone(), yr));
    db.populate(y, definition(y_source.clone(), xr));
    assert_eq!(db.expose(xr), Err(ExposureCycle(vec![x, y, x])));

    let (recursive, rr, recursive_source) = declare(&mut db, DeclKind::Alias, "Recursive");
    let app = intern(
        &mut db,
        Type::Apply {
            base: cr,
            args: vec![Argument::Positional(rr)].into(),
            kind: Kind::Type,
        },
    );
    db.populate(recursive, definition(recursive_source.clone(), app));
    assert_eq!(db.expose(rr).unwrap().ty, app);
    db.seal();
}

#[test]
fn generic_definitions_remain_quantified_and_metadata_is_parallel() {
    let mut db = Database::new();
    let a = reference(&mut db, 0, 0, Kind::Type);
    let body = function(&mut db, &[a], a);
    let poly = quantify(&mut db, vec![binder(Kind::Type)], body);
    let mut declarations = Vec::new();
    for (kind, name) in [(DeclKind::Function, "T"), (DeclKind::Closure, "U")] {
        let (id, ty, id_source) = declare(&mut db, kind, name);
        assert_panics(|| {
            db.populate(id, definition(id_source.clone(), poly));
        });
        let span = id_source.span;
        let binder_name = db.intern_symbol(name);
        db.populate(
            id,
            Declaration {
                source: id_source,
                ty: poly,
                binders: vec![BinderSource {
                    name: binder_name,
                    span,
                    bound: None,
                    default: None,
                    origin: BinderOrigin::Written,
                }]
                .into(),
                supertypes: alias::Box::default(),
                members: alias::Box::default(),
            },
        );
        assert_eq!(db.expose(ty).unwrap().ty, poly);
        declarations.push(id);
    }
    assert_ne!(
        db.declaration(declarations[0]).binders[0].name,
        db.declaration(declarations[1]).binders[0].name
    );
    let again = quantify(&mut db, vec![binder(Kind::Type)], body);
    assert_eq!(poly, again);
    assert_eq!(db.shift(poly, 0, 3).unwrap(), poly);
    let rank_two = function(&mut db, &[poly], a);
    let outer = quantify(&mut db, vec![binder(Kind::Type)], rank_two);
    let mut occurrences = HashSet::new();
    db.walk(outer, |id, depth| {
        if id == a {
            occurrences.insert(depth);
        }
    });
    assert_eq!(occurrences, HashSet::from([1, 2]));
}

#[test]
fn nominal_supertypes_use_the_structural_binder_scope() {
    let mut db = Database::new();
    let (parent, pr, parent_source) = declare(&mut db, DeclKind::Protocol, "Parent");
    let (child, cr, child_source) = declare(&mut db, DeclKind::Class, "Child");
    let a = reference(&mut db, 0, 0, Kind::Type);
    let supertype = intern(
        &mut db,
        Type::Apply {
            base: pr,
            args: vec![Argument::Positional(a)].into(),
            kind: Kind::Type,
        },
    );
    for (id, ty, src, supers) in [
        (parent, pr, parent_source, vec![]),
        (child, cr, child_source, vec![supertype]),
    ] {
        let mut b = binder(Kind::Type);
        b.variance = Variance::Covariant;
        let ty = quantify(&mut db, vec![b], ty);
        let metadata = BinderSource {
            name: src.name.unwrap(),
            span: src.span,
            bound: None,
            default: None,
            origin: BinderOrigin::Written,
        };
        db.populate(
            id,
            Declaration {
                source: src,
                ty,
                binders: vec![metadata].into(),
                supertypes: supers
                    .into_iter()
                    .map(|ty| Supertype { ty, runtime: true })
                    .collect::<Vec<_>>()
                    .into(),
                members: alias::Box::default(),
            },
        );
    }
    assert_eq!(
        db.declaration(child).supertypes.as_ref(),
        &[Supertype {
            ty: supertype,
            runtime: true
        }]
    );
    assert_eq!(db.expose(cr).unwrap().ty, cr);
    // A structural walk must not follow declarations into recursive definitions.
    let mut seen = Vec::new();
    db.walk(cr, |id, _| seen.push(id));
    assert_eq!(seen, [cr]);
    db.seal();
}

#[test]
fn unions_normalize_without_exposing_or_expanding() {
    let mut db = Database::new();
    let a = intern(&mut db, Type::Literal(Literal::Int(1)));
    let b = intern(&mut db, Type::Literal(Literal::Int(2)));
    let bottom = db.bottom();
    let top = db.top();
    let ab = union(&mut db, &[a, b]);
    assert_eq!(union(&mut db, &[b, a, a, bottom]), ab);
    assert_eq!(union(&mut db, &[ab, b, a]), ab);
    assert_eq!(union(&mut db, &[]), bottom);
    assert!(matches!(db.ty(bottom), Type::Union(members) if members.is_empty()));
    assert_eq!(intern(&mut db, Type::Top), top);
    assert_eq!(union(&mut db, &[bottom, bottom]), bottom);
    assert_eq!(union(&mut db, &[a, bottom]), a);
    let pack = reference(&mut db, 0, 0, Kind::Schema);
    let expanded = intern(&mut db, Type::Union(vec![UnionMember::Expand(pack)].into()));
    assert_ne!(expanded, pack);
    assert_eq!(union(&mut db, &[expanded, expanded]), expanded);
    assert_eq!(union(&mut db, &[expanded, top]), top);
    assert_eq!(db.kind(expanded), Kind::Type);
    assert_panics(|| {
        db.intern(Type::Union(vec![UnionMember::Type(pack)].into()));
    });
}

#[test]
fn union_applications_normalize_to_unions() {
    let mut db = Database::new();
    let (id, union_alias, source) = declare(&mut db, DeclKind::OpaqueAlias, "Union");
    db.set_intrinsic(Intrinsic::Union, union_alias);
    db.populate(id, definition(source, union_alias));
    let a = intern(&mut db, Type::Literal(Literal::Int(1)));
    let b = intern(&mut db, Type::Literal(Literal::Int(2)));
    let apply = |db: &mut Database, schema| {
        intern(
            db,
            Type::Apply {
                base: union_alias,
                args: vec![Argument::Positional(schema)].into(),
                kind: Kind::Type,
            },
        )
    };
    // Positional items, through inclusions, are the union's members
    let ab = schema(&mut db, &[a, b]);
    let included = intern(
        &mut db,
        Type::Schema(
            vec![SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Include(ab),
            }]
            .into(),
        ),
    );
    let expected = union(&mut db, &[a, b]);
    assert_eq!(apply(&mut db, ab), expected);
    assert_eq!(apply(&mut db, included), expected);
    let single = schema(&mut db, &[a]);
    assert_eq!(apply(&mut db, single), a);
    // A pack that isn't known yet stays expanded
    let pack = reference(&mut db, 0, 0, Kind::Schema);
    let expanded = intern(&mut db, Type::Union(vec![UnionMember::Expand(pack)].into()));
    assert_eq!(apply(&mut db, pack), expanded);
    // As does a schema with keyed items
    let key = intern(&mut db, Type::Literal(Literal::Int(3)));
    let keyed = intern(
        &mut db,
        Type::Schema(
            vec![SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Keyed { key, value: a },
            }]
            .into(),
        ),
    );
    let kept = apply(&mut db, keyed);
    assert!(matches!(
        db.ty(kept),
        Type::Union(members) if members[..] == [UnionMember::Expand(keyed)]
    ));
    db.seal();
}

#[test]
fn projections_fold_what_their_schemas_are_known_to_hold() {
    let mut db = Database::new();
    let project = |db: &mut Database, intrinsic, name| {
        let (id, alias, source) = declare(db, DeclKind::OpaqueAlias, name);
        db.set_intrinsic(intrinsic, alias);
        db.populate(id, definition(source, alias));
        move |db: &mut Database, schema| {
            intern(
                db,
                Type::Apply {
                    base: alias,
                    args: vec![Argument::Positional(schema)].into(),
                    kind: Kind::Type,
                },
            )
        }
    };
    let keys = project(&mut db, Intrinsic::Keys, "Keys");
    let values = project(&mut db, Intrinsic::Values, "Values");
    let entries = project(&mut db, Intrinsic::Entries, "Entries");
    let item = |multiplicity, element| SchemaItem {
        multiplicity,
        element,
    };
    let items = |db: &mut Database, items: Vec<SchemaItem>| intern(db, Type::Schema(items.into()));
    let sym = |db: &mut Database, name| {
        let name = db.intern_symbol(name);
        intern(db, Type::Literal(Literal::Sym(name)))
    };
    let (a, b) = (sym(&mut db, "a"), sym(&mut db, "b"));
    let [zero, one, two, three] =
        [0, 1, 2, 3].map(|i| intern(&mut db, Type::Literal(Literal::Int(i))));
    let (top, bottom, unknown) = (db.top(), db.bottom(), db.unknown());
    let keyed = |key, value| Element::Keyed { key, value };
    // `{a: 1, *(b): 2, 3}`, whose position has a fixed index
    let closed = items(
        &mut db,
        vec![
            item(Multiplicity::Required, keyed(a, one)),
            item(Multiplicity::Repeated, keyed(b, two)),
            item(Multiplicity::Required, Element::Positional(three)),
        ],
    );
    let keys_expected = union(&mut db, &[a, b, zero]);
    assert_eq!(keys(&mut db, closed), keys_expected);
    let values_expected = union(&mut db, &[one, two, three]);
    assert_eq!(values(&mut db, closed), values_expected);
    // Without a designated `Tuple`, entries stay whole
    let kept = entries(&mut db, closed);
    assert!(matches!(
        db.ty(kept),
        Type::Union(members) if members[..] == [UnionMember::Entries(closed)]
    ));
    // A position's key is its index, and an empty schema projects to nothing
    let positional = schema(&mut db, &[three]);
    assert_eq!(keys(&mut db, positional), zero);
    let empty = schema(&mut db, &[]);
    assert_eq!(values(&mut db, empty), bottom);
    // A position that may be missing varies, so its key is `Int`, which stays a
    // projection until `Int` is designated
    let varying = items(
        &mut db,
        vec![
            item(Multiplicity::Required, Element::Positional(one)),
            item(Multiplicity::Optional, Element::Positional(two)),
        ],
    );
    let kept = keys(&mut db, varying);
    assert!(matches!(
        db.ty(kept),
        Type::Union(members) if members[..] == [UnionMember::Keys(varying)]
    ));
    let (id, int, source) = declare(&mut db, DeclKind::Class, "Int");
    db.set_intrinsic(Intrinsic::Int, int);
    db.populate(id, definition(source, int));
    assert_eq!(keys(&mut db, varying), int);
    // `{...}` has every key and value
    let open = items(
        &mut db,
        vec![
            item(Multiplicity::Repeated, Element::Positional(top)),
            item(Multiplicity::Repeated, keyed(top, top)),
        ],
    );
    assert_eq!(keys(&mut db, open), top);
    assert_eq!(values(&mut db, open), top);
    // The dynamic schema projects to the dynamic type
    let dynamic = db.unknown_schema();
    assert_eq!(keys(&mut db, dynamic), unknown);
    // An inclusion not known yet stays a projection of it, beside what is known
    let pack = reference(&mut db, 0, 0, Kind::Schema);
    let partial = items(
        &mut db,
        vec![
            item(Multiplicity::Required, keyed(a, one)),
            item(Multiplicity::Required, Element::Include(pack)),
        ],
    );
    let expected = intern(
        &mut db,
        Type::Union(vec![UnionMember::Type(a), UnionMember::Keys(pack)].into()),
    );
    assert_eq!(keys(&mut db, partial), expected);
    // Substituting the pack evaluates the rest
    let substituted = db.substitute(expected, &[positional]);
    let a_zero = union(&mut db, &[a, zero]);
    assert_eq!(substituted, a_zero);
    // Beside a position, its positions' indexes depend on it, so the whole
    // projection waits
    let after = items(
        &mut db,
        vec![
            item(Multiplicity::Required, Element::Positional(one)),
            item(Multiplicity::Required, Element::Include(pack)),
        ],
    );
    let kept = keys(&mut db, after);
    assert!(matches!(
        db.ty(kept),
        Type::Union(members) if members[..] == [UnionMember::Keys(after)]
    ));
    // With `Tuple`, each keyed item and position is an entry
    let (id, tuple, source) = declare(&mut db, DeclKind::Class, "Tuple");
    db.set_intrinsic(Intrinsic::Tuple, tuple);
    db.populate(id, definition(source, tuple));
    let pair = |db: &mut Database, key, value| {
        let items = schema(db, &[key, value]);
        intern(
            db,
            Type::Apply {
                base: tuple,
                args: vec![Argument::Positional(items)].into(),
                kind: Kind::Type,
            },
        )
    };
    let pairs = [
        pair(&mut db, a, one),
        pair(&mut db, b, two),
        pair(&mut db, zero, three),
    ];
    let expected = union(&mut db, &pairs);
    assert_eq!(entries(&mut db, closed), expected);
    let pairs = [pair(&mut db, zero, one), pair(&mut db, int, two)];
    let expected = union(&mut db, &pairs);
    assert_eq!(entries(&mut db, varying), expected);
    db.seal();
}

#[test]
fn keys_that_may_be_indexes_conflict_with_positions() {
    let mut db = Database::new();
    let (id, int, source) = declare(&mut db, DeclKind::Class, "Int");
    db.set_intrinsic(Intrinsic::Int, int);
    db.populate(id, definition(source, int));
    let (id, str, source) = declare(&mut db, DeclKind::Class, "Str");
    db.populate(id, definition(source, str));
    let item = |multiplicity, element| SchemaItem {
        multiplicity,
        element,
    };
    let items = |db: &mut Database, items: Vec<SchemaItem>| intern(db, Type::Schema(items.into()));
    let [minus, zero, three] = [-1, 0, 3].map(|i| intern(&mut db, Type::Literal(Literal::Int(i))));
    let top = db.top();
    let keyed = |key, value| Element::Keyed { key, value };
    let positional = |ty| item(Multiplicity::Required, Element::Positional(ty));
    let repeated = |ty| item(Multiplicity::Repeated, Element::Positional(ty));
    let promotion = |db: &mut Database, list| {
        let schema = items(db, list);
        db.promoted(schema)
    };
    let conflicts = [
        // `{Int, 0: Str}`
        vec![
            positional(int),
            item(Multiplicity::Required, keyed(zero, str)),
        ],
        // `{*Int, 3: Str}`, where position 3 may exist
        vec![
            repeated(int),
            item(Multiplicity::Required, keyed(three, str)),
        ],
        // `{*Int, *(Int): Str}`
        vec![repeated(int), item(Multiplicity::Repeated, keyed(int, str))],
    ];
    for list in conflicts {
        assert_eq!(promotion(&mut db, list), Promotion::Conflict);
    }
    let fine = [
        // `{Int, 3: Str}`, where position 3 can't exist
        vec![
            positional(int),
            item(Multiplicity::Required, keyed(three, str)),
        ],
        // `{*Int, -1: Str}`: negative keys are never indexes
        vec![
            repeated(int),
            item(Multiplicity::Required, keyed(minus, str)),
        ],
        // `{...}`
        vec![repeated(top), item(Multiplicity::Repeated, keyed(top, top))],
        // `{0: Str}` has no positions
        vec![item(Multiplicity::Required, keyed(zero, str))],
    ];
    for list in fine {
        assert!(matches!(promotion(&mut db, list), Promotion::Promoted(_)));
    }
}

#[test]
fn unknown_is_a_type_that_unions_do_not_absorb() {
    let mut db = Database::new();
    let unknown = db.unknown();
    assert_eq!(intern(&mut db, Type::Unknown(Kind::Type)), unknown);
    assert_ne!(unknown, db.top());
    assert_eq!(db.kind(unknown), Kind::Type);
    let a = intern(&mut db, Type::Literal(Literal::Int(1)));
    let au = union(&mut db, &[a, unknown]);
    assert!(matches!(db.ty(au), Type::Union(members) if members.len() == 2));
    assert_eq!(union(&mut db, &[unknown, unknown]), unknown);
    let top = db.top();
    assert_eq!(union(&mut db, &[unknown, top]), top);
}

#[test]
fn unknown_schema_is_a_distinct_schema() {
    let mut db = Database::new();
    let unknown = db.unknown_schema();
    assert_eq!(intern(&mut db, Type::Unknown(Kind::Schema)), unknown);
    assert_ne!(unknown, db.unknown());
    assert_eq!(db.kind(unknown), Kind::Schema);
    assert_eq!(db.unknown_of(Kind::Schema), unknown);
    assert_eq!(db.unknown_of(Kind::Type), db.unknown());
    // It stands where a schema must, as in a function's parameters
    let result = db.unknown();
    let func = Type::Function(Function {
        params: unknown,
        result,
        input: None,
        output: None,
    });
    intern(&mut db, func);
    assert_panics(|| {
        db.intern(Type::Union(vec![UnionMember::Type(unknown)].into()));
    });
}

#[test]
fn substitution_replaces_the_outer_group_and_shifts_under_quantifiers() {
    let mut db = Database::new();
    let a = reference(&mut db, 0, 0, Kind::Type);
    let b = reference(&mut db, 0, 1, Kind::Type);
    let one = intern(&mut db, Type::Literal(Literal::Int(1)));
    // The arguments are themselves open, in the scope where the result is used
    let outer = reference(&mut db, 0, 3, Kind::Type);
    let body = function(&mut db, &[a], b);
    assert_eq!(
        db.substitute(body, &[one, outer]),
        function(&mut db, &[one], outer)
    );
    // Under a nested quantifier, local references stay and arguments shift
    let local = reference(&mut db, 0, 0, Kind::Type);
    let a_inner = reference(&mut db, 1, 0, Kind::Type);
    let inner = function(&mut db, &[local], a_inner);
    let nested = quantify(&mut db, vec![binder(Kind::Type)], inner);
    let outer_inner = reference(&mut db, 1, 3, Kind::Type);
    let expected = function(&mut db, &[local], outer_inner);
    let expected = quantify(&mut db, vec![binder(Kind::Type)], expected);
    assert_eq!(db.substitute(nested, &[outer, one]), expected);
    // A closed type has no reference beyond its group
    let beyond = reference(&mut db, 1, 0, Kind::Type);
    assert_panics(|| {
        db.substitute(beyond, &[one]);
    });
}

#[test]
fn merging_groups_undoes_splitting() {
    let mut db = Database::new();
    let a = reference(&mut db, 0, 0, Kind::Type);
    let b = reference(&mut db, 0, 1, Kind::Type);
    let c = reference(&mut db, 0, 2, Kind::Type);
    // The last binder's bound mentions the first, and a nested quantifier refers
    // past its own group
    let bounded = Binder {
        bound: Some(a),
        ..binder(Kind::Type)
    };
    let local = reference(&mut db, 0, 0, Kind::Type);
    let b_inner = reference(&mut db, 1, 1, Kind::Type);
    let inner = function(&mut db, &[local], b_inner);
    let inner = quantify(&mut db, vec![binder(Kind::Type)], inner);
    let body = function(&mut db, &[a, b, inner], c);
    let binders = vec![binder(Kind::Type), binder(Kind::Type), bounded];
    let flat = quantify(&mut db, binders.clone(), body);
    for count in 0..=binders.len() {
        let split = db.split(flat, count);
        assert_eq!(db.merge_groups(&binders[..count], split), flat, "{count}");
    }
}

#[test]
fn members_and_overloads_are_checked() {
    let mut db = Database::new();
    let (class, class_ty, class_source) = declare(&mut db, DeclKind::Class, "Box");
    let (method, _, method_source) = declare(&mut db, DeclKind::Function, "get");
    let (overload, _, overload_source) = declare(&mut db, DeclKind::Function, "get");
    let name = db.intern_symbol("get");
    let field = db.intern_symbol("item");
    let schema = db.unknown_schema();
    let int = intern(&mut db, Type::Literal(Literal::Int(1)));
    let field_of = |ty| Member::Field {
        ty,
        scope: Scope::Instance,
        public: true,
    };
    // A field is a type, and only a class has members
    let mut bad = definition(class_source.clone(), class_ty);
    bad.members = vec![(
        MemberKey {
            name: field,
            special: false,
            private: false,
        },
        field_of(schema),
    )]
    .into();
    assert_panics(|| db.populate(class, bad.clone()));
    let mut not_class = definition(method_source.clone(), int);
    not_class.members = bad.members.clone();
    assert_panics(|| db.populate(method, not_class.clone()));

    let mut good = definition(class_source, class_ty);
    good.members = vec![
        (
            MemberKey {
                name: field,
                special: false,
                private: false,
            },
            field_of(int),
        ),
        (
            MemberKey {
                name,
                special: false,
                private: false,
            },
            Member::Method {
                decl: method,
                scope: Scope::Instance,
                public: true,
            },
        ),
    ]
    .into();
    db.populate(class, good);
    db.populate(method, definition(method_source, int));
    db.populate(overload, definition(overload_source, int));
    assert_panics(|| {
        let mut db = Database::new();
        let id = db.allocate();
        db.set_overloads(id, vec![]);
    });
    db.set_overloads(method, vec![overload]);
    db.seal();
    assert_eq!(db.overloads(method), [overload]);
    assert_eq!(db.implementation(method), Some(method));
    assert!(db.overloads(overload).is_empty());
    assert_eq!(db.declarations().count(), 3);
    assert_eq!(db.declaration(class).members.len(), 2);
}

#[test]
#[should_panic(expected = "is not a function")]
fn a_method_member_must_be_a_function() {
    let mut db = Database::new();
    let (class, class_ty, class_source) = declare(&mut db, DeclKind::Class, "Box");
    let name = db.intern_symbol("get");
    let mut declaration = definition(class_source, class_ty);
    declaration.members = vec![(
        MemberKey {
            name,
            special: false,
            private: false,
        },
        Member::Method {
            decl: class,
            scope: Scope::Instance,
            public: true,
        },
    )]
    .into();
    db.populate(class, declaration);
    db.seal();
}

#[test]
fn schema_order_multiplicity_and_argument_modes_are_structural() {
    let mut db = Database::new();
    let a = intern(&mut db, Type::Literal(Literal::Int(1)));
    let b = intern(&mut db, Type::Literal(Literal::Int(2)));
    let ab = schema(&mut db, &[a, b]);
    assert_ne!(ab, schema(&mut db, &[b, a]));
    let mut forms = HashSet::new();
    for multiplicity in [
        Multiplicity::Required,
        Multiplicity::Optional,
        Multiplicity::Repeated,
    ] {
        for element in [
            Element::Positional(a),
            Element::Keyed { key: a, value: b },
            Element::Include(ab),
        ] {
            forms.insert(intern(
                &mut db,
                Type::Schema(
                    vec![SchemaItem {
                        multiplicity,
                        element,
                    }]
                    .into(),
                ),
            ));
        }
    }
    assert_eq!(forms.len(), 9);
    let (_, record, __source) = declare(&mut db, DeclKind::Class, "Record");
    let key = db.intern_symbol("item");
    let mut apps = HashSet::new();
    for arg in [
        Argument::Positional(ab),
        Argument::Keyword(key, ab),
        Argument::Expand(ab),
    ] {
        apps.insert(intern(
            &mut db,
            Type::Apply {
                base: record,
                args: vec![arg].into(),
                kind: Kind::Type,
            },
        ));
    }
    assert_eq!(apps.len(), 3);
}

#[test]
fn binder_semantics_participate_in_identity() {
    let mut db = Database::new();
    let top = db.top();
    let empty = schema(&mut db, &[]);
    let key = db.intern_symbol("K");
    let base = binder(Kind::Type);
    let plain = quantify(&mut db, vec![base.clone()], top);
    let variations = [
        Binder {
            binding: Binding::Keyword(key),
            ..base.clone()
        },
        Binder {
            bound: Some(top),
            ..base.clone()
        },
        Binder {
            default: Some(top),
            ..base.clone()
        },
        Binder {
            variance: Variance::Covariant,
            ..base.clone()
        },
        Binder {
            variance: Variance::Contravariant,
            ..base.clone()
        },
    ];
    let mut distinct = HashSet::from([plain]);
    for b in variations {
        distinct.insert(quantify(&mut db, vec![b], top));
    }
    assert_eq!(distinct.len(), 6);
    let mut rest_types = HashSet::new();
    for rest in [Rest::All, Rest::Positional, Rest::Keyed] {
        for bound in [None, Some(empty)] {
            rest_types.insert(quantify(
                &mut db,
                vec![Binder {
                    kind: Kind::Schema,
                    binding: Binding::Rest(rest),
                    bound,
                    default: None,
                    variance: Variance::Invariant,
                }],
                top,
            ));
        }
    }
    assert_eq!(rest_types.len(), 6);
    assert_eq!(quantify(&mut db, vec![], top), top);
}

#[test]
fn local_kind_checks_reject_invalid_shapes() {
    let mut db = Database::new();
    let top = db.top();
    let empty = schema(&mut db, &[]);
    let bad = [
        Type::Function(Function {
            params: top,
            result: top,
            input: None,
            output: None,
        }),
        Type::Function(Function {
            params: empty,
            result: empty,
            input: None,
            output: None,
        }),
        Type::Function(Function {
            params: empty,
            result: top,
            input: Some(empty),
            output: None,
        }),
        Type::Schema(
            vec![SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Include(top),
            }]
            .into(),
        ),
        Type::Schema(
            vec![SchemaItem {
                multiplicity: Multiplicity::Required,
                element: Element::Keyed {
                    key: empty,
                    value: top,
                },
            }]
            .into(),
        ),
        Type::Apply {
            base: top,
            args: vec![Argument::Expand(top)].into(),
            kind: Kind::Type,
        },
        Type::Union(vec![UnionMember::Expand(top)].into()),
    ];
    for ty in bad {
        assert_panics(|| {
            db.intern(ty);
        });
    }
    let rest = Binder {
        binding: Binding::Rest(Rest::All),
        ..binder(Kind::Type)
    };
    assert_panics(|| {
        db.intern(Type::Quantified {
            binders: vec![rest].into(),
            body: top,
        });
    });
    for (kind, bound) in [(Kind::Type, empty), (Kind::Schema, top)] {
        let wrong_bound = Binder {
            bound: Some(bound),
            ..binder(kind)
        };
        assert_panics(|| {
            db.intern(Type::Quantified {
                binders: vec![wrong_bound].into(),
                body: top,
            });
        });
    }
    let wrong_default = Binder {
        default: Some(empty),
        ..binder(Kind::Type)
    };
    assert_panics(|| {
        db.intern(Type::Quantified {
            binders: vec![wrong_default].into(),
            body: top,
        });
    });
    let (a, _, a_source) = declare(&mut db, DeclKind::Alias, "A");
    assert_panics(|| {
        db.populate(a, definition(a_source.clone(), empty));
    });
    let mut def = definition(a_source, top);
    def.supertypes = vec![Supertype {
        ty: top,
        runtime: true,
    }]
    .into();
    assert_panics(|| {
        db.populate(a, def);
    });
}

#[test]
fn shifting_respects_groups_slots_and_cutoffs() {
    let mut db = Database::new();
    let outer = reference(&mut db, 1, 3, Kind::Type);
    let local = reference(&mut db, 0, 0, Kind::Type);
    let params = schema(&mut db, &[local, outer]);
    let f = intern(
        &mut db,
        Type::Function(Function {
            params,
            result: outer,
            input: Some(outer),
            output: Some(local),
        }),
    );
    let b = Binder {
        bound: Some(outer),
        default: Some(local),
        ..binder(Kind::Type)
    };
    let root = quantify(&mut db, vec![b], f);
    let shifted = db.shift(root, 0, 2).unwrap();
    let expected_outer = reference(&mut db, 3, 3, Kind::Type);
    let mut nodes = HashSet::new();
    db.walk(shifted, |id, _| {
        nodes.insert(id);
    });
    assert!(nodes.contains(&local));
    assert!(nodes.contains(&expected_outer));
    assert!(!nodes.contains(&outer));
    assert_eq!(db.shift(shifted, 0, -2).unwrap(), root);
    assert_eq!(db.shift(root, 1, 1).unwrap(), root);
    assert_eq!(
        db.shift(root, 0, -1),
        Err(RemovedBinder(BoundRef { depth: 1, slot: 3 }))
    );
    let free = reference(&mut db, 0, 0, Kind::Type);
    assert_eq!(
        db.shift(free, 0, -1),
        Err(RemovedBinder(BoundRef { depth: 0, slot: 0 }))
    );
    assert_eq!(db.shift(root, 0, 0).unwrap(), root);
}

#[test]
fn scope_sensitive_memoization_distinguishes_shared_occurrences() {
    let mut db = Database::new();
    let same = reference(&mut db, 0, 0, Kind::Type);
    let local_function = function(&mut db, &[same], same);
    let quantified = quantify(&mut db, vec![binder(Kind::Type)], local_function);
    let mixed = function(&mut db, &[quantified, same], same);
    let shifted = db.shift(mixed, 0, 1).unwrap();
    let free_shifted = reference(&mut db, 1, 0, Kind::Type);
    let mut seen = HashSet::new();
    db.walk(shifted, |id, depth| {
        seen.insert((id, depth));
    });
    assert!(seen.contains(&(same, 1)));
    assert!(seen.contains(&(free_shifted, 0)));
    assert!(!seen.contains(&(same, 0)));
    assert!(seen.contains(&(quantified, 0)));
}

#[test]
fn all_structural_edges_are_walked_and_rebuilt() {
    let mut db = Database::new();
    let leaves: Vec<_> = (0..13)
        .map(|slot| reference(&mut db, 1, slot, Kind::Type))
        .collect();
    let pack = reference(&mut db, 1, 13, Kind::Schema);
    let key = db.intern_symbol("K");
    let app = intern(
        &mut db,
        Type::Apply {
            base: leaves[0],
            args: vec![
                Argument::Positional(leaves[1]),
                Argument::Keyword(key, leaves[2]),
                Argument::Expand(pack),
            ]
            .into(),
            kind: Kind::Type,
        },
    );
    let either = intern(
        &mut db,
        Type::Union(vec![UnionMember::Type(leaves[3]), UnionMember::Expand(pack)].into()),
    );
    let params = intern(
        &mut db,
        Type::Schema(
            vec![
                SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Positional(app),
                },
                SchemaItem {
                    multiplicity: Multiplicity::Optional,
                    element: Element::Keyed {
                        key: leaves[4],
                        value: leaves[5],
                    },
                },
                SchemaItem {
                    multiplicity: Multiplicity::Repeated,
                    element: Element::Include(pack),
                },
                SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Positional(either),
                },
            ]
            .into(),
        ),
    );
    let f = intern(
        &mut db,
        Type::Function(Function {
            params,
            result: leaves[6],
            input: Some(leaves[7]),
            output: Some(leaves[8]),
        }),
    );
    let rest_bound = intern(
        &mut db,
        Type::Schema(
            vec![SchemaItem {
                multiplicity: Multiplicity::Repeated,
                element: Element::Positional(leaves[11]),
            }]
            .into(),
        ),
    );
    let q = quantify(
        &mut db,
        vec![
            Binder {
                bound: Some(leaves[9]),
                default: Some(leaves[10]),
                ..binder(Kind::Type)
            },
            Binder {
                bound: Some(rest_bound),
                binding: Binding::Rest(Rest::All),
                default: Some(pack),
                ..binder(Kind::Schema)
            },
            Binder {
                bound: Some(pack),
                ..binder(Kind::Schema)
            },
            Binder {
                default: Some(leaves[12]),
                ..binder(Kind::Type)
            },
        ],
        f,
    );
    let mut seen = HashSet::new();
    db.walk(q, |id, depth| {
        seen.insert((id, depth));
    });
    for leaf in leaves.iter().copied().chain([pack]) {
        assert!(seen.contains(&(leaf, 1)));
    }
    let shifted = db.shift(q, 0, 1).unwrap();
    let mut after = HashSet::new();
    db.walk(shifted, |id, depth| {
        after.insert((id, depth));
    });
    for leaf in leaves.into_iter().chain([pack]) {
        assert!(!after.contains(&(leaf, 1)));
        let shifted_leaf = db.shift(leaf, 0, 1).unwrap();
        assert!(after.contains(&(shifted_leaf, 1)));
    }
    assert_eq!(db.shift(shifted, 0, -1).unwrap(), q);
}

#[test]
fn coordinate_limits_are_checked_without_truncation() {
    assert_panics(|| {
        BoundRef::new(65536, 0);
    });
    assert_panics(|| {
        BoundRef::new(0, 65536);
    });
    assert_eq!(
        BoundRef::new(65535, 65535),
        BoundRef {
            depth: 65535,
            slot: 65535
        }
    );
    let mut db = Database::new();
    let far = reference(&mut db, 65535, 0, Kind::Type);
    assert_panics(|| {
        let _ = db.shift(far, 0, 1);
    });
    assert_panics(|| {
        let _ = db.shift(far, 0, i32::MAX);
    });
    assert!(matches!(db.shift(far, 0, i32::MIN), Err(RemovedBinder(_))));
    let top = db.top();
    let too_many = Type::Quantified {
        binders: vec![binder(Kind::Type); 65537].into(),
        body: top,
    };
    assert_panics(|| {
        db.intern(too_many);
    });
    let maximum = Type::Quantified {
        binders: vec![binder(Kind::Type); 65536].into(),
        body: top,
    };
    db.intern(maximum);
}

#[test]
fn shared_interning_preserves_borrowed_types() {
    let mut db = Database::new();
    db.seal();
    let db = &db;
    let id = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Type,
    });
    let borrowed = db.ty(id);
    for value in 0..1024 {
        db.intern(Type::Literal(Literal::Int(value)));
    }
    let shifted = db.shift(id, 0, 1).unwrap();
    assert_ne!(shifted, id);
    assert_eq!(db.intern(borrowed.clone()), id);
    assert!(std::ptr::eq(borrowed, db.ty(id)));
    assert_eq!(db.shift(shifted, 0, -1).unwrap(), id);
}

#[test]
#[should_panic(expected = "unallocated unit ID")]
fn declarations_require_allocated_units() {
    let mut db = Database::new();
    let mut src = source(&mut db, DeclKind::Alias, "A");
    src.span.unit = UnitId::from_index(1);
    let id = db.allocate();
    db.populate(id, definition(src, db.top()));
}

#[test]
fn forward_schema_kinds_are_checked_when_sealing() {
    let mut db = Database::new();
    let (id, reference, mut source) = declare(&mut db, DeclKind::Alias, "Schema");
    source.result_kind = Kind::Schema;
    let callable = db.intern(Type::Function(Function {
        params: reference,
        result: db.top(),
        input: None,
        output: None,
    }));
    let empty = db.intern(Type::Schema(alias::Box::default()));
    db.populate(id, definition(source, empty));
    db.seal();
    assert!(matches!(db.declarations, Declarations::Frozen(_)));
    assert_eq!(db.kind(reference), Kind::Schema);
    assert_eq!(db.declaration(id).ty, empty);
    assert_eq!(db.intern(db.ty(callable).clone()), callable);
}

#[test]
#[should_panic(expected = "type kind mismatch")]
fn sealing_checks_forward_kinds_even_after_normalization() {
    let mut db = Database::new();
    let (id, reference, mut source) = declare(&mut db, DeclKind::Alias, "Schema");
    source.result_kind = Kind::Schema;
    // Top absorbs the reference, but its kind must still be checked.
    let top = db.top();
    assert_eq!(union(&mut db, &[top, reference]), top);
    let empty = db.intern(Type::Schema(alias::Box::default()));
    db.populate(id, definition(source, empty));
    db.seal();
}

#[test]
fn intrinsic_associations_are_optional_and_write_once() {
    let intrinsics = [
        Intrinsic::Union,
        Intrinsic::Func,
        Intrinsic::Int,
        Intrinsic::Bool,
        Intrinsic::Sym,
        Intrinsic::Nil,
        Intrinsic::Str,
        Intrinsic::Keys,
        Intrinsic::Values,
        Intrinsic::Entries,
        Intrinsic::Tuple,
    ];
    let mut db = Database::new();
    let mut registered = Vec::new();
    for intrinsic in intrinsics {
        assert_eq!(db.intrinsic(intrinsic), None);
        let (id, ty, source) = declare(&mut db, DeclKind::OpaqueAlias, "stub");
        // Registration supports forward declaration references.
        db.set_intrinsic(intrinsic, ty);
        db.populate(id, definition(source, ty));
        assert_panics(|| db.set_intrinsic(intrinsic, ty));
        assert_panics(|| db.set_intrinsic(intrinsic, db.top()));
        assert_eq!(db.intrinsic(intrinsic), Some(ty));
        registered.push((intrinsic, ty));
    }
    db.seal();
    for (intrinsic, ty) in registered {
        assert_eq!(db.intrinsic(intrinsic), Some(ty));
        assert_panics(|| db.set_intrinsic(intrinsic, ty));
    }

    let mut empty = Database::new();
    empty.seal();
    for intrinsic in intrinsics {
        assert_eq!(empty.intrinsic(intrinsic), None);
        assert_panics(|| empty.set_intrinsic(intrinsic, empty.top()));
        assert_eq!(empty.intrinsic(intrinsic), None);
    }
}

#[test]
fn intrinsic_associations_require_type_kind() {
    let mut db = Database::new();
    let schema = db.intern(Type::Schema(alias::Box::default()));
    assert_panics(|| db.set_intrinsic(Intrinsic::Union, schema));
    assert_eq!(db.intrinsic(Intrinsic::Union), None);
    let (id, ty, mut source) = declare(&mut db, DeclKind::Alias, "Schema");
    db.set_intrinsic(Intrinsic::Int, ty);
    source.result_kind = Kind::Schema;
    db.populate(id, definition(source, schema));
    assert_panics(|| db.seal());
}

/// A generic declaration of `count` type binders
fn generic(db: &mut Database, name: &str, count: usize, body: TypeId) -> DeclId {
    let (id, _, source) = declare(db, DeclKind::Annotation, name);
    let ty = quantify(db, vec![binder(Kind::Type); count], body);
    let binders = (0..count)
        .map(|_| BinderSource {
            name: source.name.unwrap(),
            span: source.span,
            bound: None,
            default: None,
            origin: BinderOrigin::Written,
        })
        .collect();
    db.populate(
        id,
        Declaration {
            binders,
            ..definition(source, ty)
        },
    );
    id
}

#[test]
fn rigids_abstract_back_to_their_group() {
    let mut db = Database::new();
    let t = reference(&mut db, 0, 0, Kind::Type);
    let u = reference(&mut db, 0, 1, Kind::Type);
    let outer = reference(&mut db, 1, 1, Kind::Type);
    let local = reference(&mut db, 0, 0, Kind::Type);
    let nested_body = union(&mut db, &[outer, local]);
    let nested = quantify(&mut db, vec![binder(Kind::Type)], nested_body);
    let body = function(&mut db, &[t, nested], u);
    let f = generic(&mut db, "f", 2, body);
    let g = generic(&mut db, "g", 1, t);

    let rigids = db.rigids(f);
    assert_eq!(rigids, db.rigids(f), "rigids are interned");
    assert_eq!(
        db.ty(rigids[1]),
        &Type::Rigid {
            decl: f,
            slot: 1,
            kind: Kind::Type
        }
    );
    let checked = db.substitute(body, &rigids);
    assert_ne!(checked, body);
    assert_eq!(db.abstract_rigids(checked, f), Ok(body));

    let foreign = db.rigids(g)[0];
    let escaped = union(&mut db, &[checked, foreign]);
    assert_eq!(db.abstract_rigids(escaped, f), Err(Escape(foreign)));
}

#[test]
#[should_panic(expected = "rigid in a declaration")]
fn declarations_never_contain_rigids() {
    let mut db = Database::new();
    let t = reference(&mut db, 0, 0, Kind::Type);
    let f = generic(&mut db, "f", 1, t);
    let rigid = db.rigids(f)[0];
    let (id, _, source) = declare(&mut db, DeclKind::Annotation, "leak");
    db.populate(id, definition(source, rigid));
}

#[test]
fn binder_bounds_substitute_arguments_or_take_rest_shapes() {
    let mut db = Database::new();
    let t = reference(&mut db, 0, 0, Kind::Type);
    let bounded = Binder {
        bound: Some(t),
        ..binder(Kind::Type)
    };
    let top = db.top();
    assert_eq!(db.binder_bound(&bounded, &[top]), Some(top));
    assert_eq!(db.binder_bound(&binder(Kind::Type), &[top]), None);
    // Without a designated `Sym`, keys are dynamic
    let (key, value) = (db.unknown(), db.top());
    let item = |element| SchemaItem {
        multiplicity: Multiplicity::Repeated,
        element,
    };
    let positional = item(Element::Positional(value));
    let keyed = item(Element::Keyed { key, value });
    for (rest, items) in [
        (Rest::Positional, vec![positional.clone()]),
        (Rest::Keyed, vec![keyed.clone()]),
        (Rest::All, vec![positional, keyed]),
    ] {
        let shape = intern(&mut db, Type::Schema(items.into()));
        let pack = Binder {
            binding: Binding::Rest(rest),
            ..binder(Kind::Schema)
        };
        assert_eq!(db.rest_shape(rest), shape);
        assert_eq!(db.binder_bound(&pack, &[]), Some(shape));
    }
}

#[test]
fn sealed_declarations_can_be_retyped_and_are_revalidated() {
    let mut db = Database::new();
    let t = reference(&mut db, 0, 0, Kind::Type);
    let f = generic(&mut db, "f", 1, t);
    let top = db.top();
    let constant = quantify(&mut db, vec![binder(Kind::Type)], top);
    db.seal();
    db.retype(f, constant);
    assert_eq!(db.declaration(f).ty, constant);
    let rigid = db.rigids(f)[0];
    let leak = quantify(&mut db, vec![binder(Kind::Type)], rigid);
    assert_panics(|| db.retype(f, leak));
    let unquantified = db.top();
    assert_panics(|| db.retype(f, unquantified));
}

fn item(multiplicity: Multiplicity, element: Element) -> SchemaItem {
    SchemaItem {
        multiplicity,
        element,
    }
}

fn items(db: &mut Database, items: Vec<SchemaItem>) -> TypeId {
    intern(db, Type::Schema(items.into()))
}

fn map(db: &mut Database, packs: &[TypeId], pattern: TypeId) -> TypeId {
    intern(
        db,
        Type::Map {
            packs: packs.iter().copied().collect(),
            pattern,
        },
    )
}

fn int(db: &mut Database, value: i128) -> TypeId {
    intern(db, Type::Literal(Literal::Int(value)))
}

#[test]
fn a_mapping_over_a_known_pack_reduces_item_by_item() {
    let mut db = Database::new();
    // `(item) -> T`, where `T` is outside the mapping
    let local = reference(&mut db, 0, 0, Kind::Type);
    let outer = reference(&mut db, 1, 0, Kind::Type);
    let pattern = function(&mut db, &[local], outer);
    let t = reference(&mut db, 0, 0, Kind::Type);
    let applied = |db: &mut Database, item| function(db, &[item], t);
    let [one, two, three, four] = [1, 2, 3, 4].map(|value| int(&mut db, value));
    let key = db.intern_symbol("a");
    let key = intern(&mut db, Type::Literal(Literal::Sym(key)));
    let rest = reference(&mut db, 0, 1, Kind::Schema);
    let nested = items(
        &mut db,
        vec![item(Multiplicity::Optional, Element::Positional(four))],
    );
    let pack = items(
        &mut db,
        vec![
            item(Multiplicity::Required, Element::Positional(one)),
            item(Multiplicity::Optional, Element::Positional(two)),
            item(Multiplicity::Repeated, Element::Keyed { key, value: three }),
            // A known schema is spliced, its items taking on the multiplicity
            item(Multiplicity::Repeated, Element::Include(nested)),
            // One not yet known is mapped in turn
            item(Multiplicity::Required, Element::Include(rest)),
        ],
    );
    let elements = [
        (
            Multiplicity::Required,
            Element::Positional(applied(&mut db, one)),
        ),
        (
            Multiplicity::Optional,
            Element::Positional(applied(&mut db, two)),
        ),
        (
            Multiplicity::Repeated,
            Element::Keyed {
                key,
                value: applied(&mut db, three),
            },
        ),
        (
            Multiplicity::Repeated,
            Element::Positional(applied(&mut db, four)),
        ),
        (
            Multiplicity::Required,
            Element::Include(map(&mut db, &[rest], pattern)),
        ),
    ];
    let expected = items(
        &mut db,
        elements
            .into_iter()
            .map(|(multiplicity, element)| item(multiplicity, element))
            .collect(),
    );
    assert_eq!(map(&mut db, &[pack], pattern), expected);
    // A pack not yet known keeps the mapping, which is a schema
    let mapped = map(&mut db, &[rest], pattern);
    assert!(matches!(db.ty(mapped), Type::Map { .. }));
    assert_eq!(db.kind(mapped), Kind::Schema);
    // The dynamic pack maps to the dynamic schema
    let unknown = db.unknown_schema();
    assert_eq!(map(&mut db, &[unknown], pattern), unknown);
}

#[test]
fn several_packs_reduce_only_when_their_items_correspond() {
    let mut db = Database::new();
    let first = reference(&mut db, 0, 0, Kind::Type);
    let second = reference(&mut db, 0, 1, Kind::Type);
    let top = db.top();
    let pattern = function(&mut db, &[first, second], top);
    let [one, two, three, four] = [1, 2, 3, 4].map(|value| int(&mut db, value));
    let [a, b] = ["a", "b"].map(|name| {
        let sym = db.intern_symbol(name);
        intern(&mut db, Type::Literal(Literal::Sym(sym)))
    });
    let pack = |db: &mut Database, positional, multiplicity, key, value| {
        items(
            db,
            vec![
                item(Multiplicity::Required, Element::Positional(positional)),
                item(multiplicity, Element::Keyed { key, value }),
            ],
        )
    };
    let left = pack(&mut db, one, Multiplicity::Repeated, a, two);
    let right = pack(&mut db, three, Multiplicity::Repeated, a, four);
    let applied = |db: &mut Database, x, y| function(db, &[x, y], top);
    let expected = vec![
        item(
            Multiplicity::Required,
            Element::Positional(applied(&mut db, one, three)),
        ),
        item(
            Multiplicity::Repeated,
            Element::Keyed {
                key: a,
                value: applied(&mut db, two, four),
            },
        ),
    ];
    let expected = items(&mut db, expected);
    assert_eq!(map(&mut db, &[left, right], pattern), expected);
    // Items that don't correspond keep the mapping
    let short = schema(&mut db, &[one]);
    let optional = pack(&mut db, three, Multiplicity::Optional, a, four);
    let rekeyed = pack(&mut db, three, Multiplicity::Repeated, b, four);
    let rest = reference(&mut db, 0, 0, Kind::Schema);
    let opaque = items(
        &mut db,
        vec![
            item(Multiplicity::Required, Element::Positional(one)),
            item(Multiplicity::Required, Element::Include(rest)),
        ],
    );
    let unknown = db.unknown_schema();
    for other in [short, optional, rekeyed, opaque, unknown] {
        let mapped = map(&mut db, &[left, other], pattern);
        assert!(matches!(db.ty(mapped), Type::Map { .. }));
    }
}

#[test]
fn a_mapping_is_its_own_group() {
    let mut db = Database::new();
    // `{...(item, T)}` over the pack `S`, both outside the mapping
    let local = reference(&mut db, 0, 0, Kind::Type);
    let outer = reference(&mut db, 1, 0, Kind::Type);
    let top = db.top();
    let pattern = function(&mut db, &[local, outer], top);
    let pack = reference(&mut db, 0, 1, Kind::Schema);
    let mapped = map(&mut db, &[pack], pattern);
    let mut seen = HashSet::new();
    db.walk(mapped, |id, depth| {
        seen.insert((id, depth));
    });
    assert!(seen.contains(&(pack, 0)));
    assert!(seen.contains(&(pattern, 1)));
    // Shifting leaves the item's references and moves the others
    let shifted_pack = reference(&mut db, 1, 1, Kind::Schema);
    let shifted_outer = reference(&mut db, 2, 0, Kind::Type);
    let shifted_pattern = function(&mut db, &[local, shifted_outer], top);
    let shifted = map(&mut db, &[shifted_pack], shifted_pattern);
    assert_eq!(db.shift(mapped, 0, 1), Ok(shifted));
    // Substituting a known pack reduces it, with what the pattern refers to
    // outside it substituted too
    let one = int(&mut db, 1);
    let two = int(&mut db, 2);
    let known = schema(&mut db, &[one]);
    let applied = function(&mut db, &[one, two], top);
    let expected = schema(&mut db, &[applied]);
    assert_eq!(db.substitute(mapped, &[two, known]), expected);
    // Only the number of packs is shape
    let other = map(&mut db, &[shifted_pack], pattern);
    assert!(db.ty(mapped).same_shape(db.ty(other)));
    let pair = map(&mut db, &[pack, pack], pattern);
    assert!(!db.ty(mapped).same_shape(db.ty(pair)));
}

#[test]
fn instantiating_opens_the_innermost_group() {
    let mut db = Database::new();
    let local = reference(&mut db, 0, 0, Kind::Type);
    let outer = reference(&mut db, 1, 2, Kind::Type);
    let top = db.top();
    let pattern = function(&mut db, &[local, outer], top);
    let one = int(&mut db, 1);
    let nearer = reference(&mut db, 0, 2, Kind::Type);
    let expected = function(&mut db, &[one, nearer], top);
    assert_eq!(db.instantiate(pattern, &[one]), expected);
}

#[test]
fn a_mapping_has_packs_of_schemas_and_a_type_pattern() {
    let mut db = Database::new();
    let local = reference(&mut db, 0, 0, Kind::Type);
    let schema_pack = reference(&mut db, 0, 0, Kind::Schema);
    let type_pack = reference(&mut db, 0, 0, Kind::Type);
    map(&mut db, &[schema_pack], local);
    assert_panics(|| {
        db.intern(Type::Map {
            packs: vec![].into(),
            pattern: local,
        });
    });
    assert_panics(|| {
        db.intern(Type::Map {
            packs: vec![type_pack].into(),
            pattern: local,
        });
    });
    assert_panics(|| {
        db.intern(Type::Map {
            packs: vec![schema_pack].into(),
            pattern: schema_pack,
        });
    });
}
