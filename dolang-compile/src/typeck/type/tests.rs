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
                supertypes: supers.into(),
                members: alias::Box::default(),
            },
        );
    }
    assert_eq!(db.declaration(child).supertypes.as_ref(), &[supertype]);
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
            },
            field_of(int),
        ),
        (
            MemberKey {
                name,
                special: false,
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
    db.set_overloads(method, vec![overload, method]);
    db.seal();
    assert_eq!(db.overloads(method), [overload, method]);
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
    def.supertypes = vec![top].into();
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
