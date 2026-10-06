use super::*;

fn residual(outcome: &Outcome, residual: Residual) -> bool {
    outcome.status == Status::Unresolved && has(outcome, residual.into())
}

#[test]
fn schema_keyword_and_rest_binders_are_instantiated() {
    let mut db = Database::new();
    let int = int(&mut db);
    let [one, two] = [1, 2].map(|n| literal(&db, n));
    let rest = |binding| Binder {
        variance: Variance::Covariant,
        ..bounded(Kind::Schema, binding, None)
    };
    let tuple = nominal(
        &mut db,
        "Tuple",
        vec![rest(Binding::Rest(Rest::Positional))],
        vec![],
    );
    let keyword = db.intern_symbol("T");
    let named = nominal(
        &mut db,
        "Named",
        vec![Binder {
            binding: Binding::Keyword(keyword),
            ..binder(Variance::Covariant)
        }],
        vec![],
    );
    let s = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    });
    let body = quantified(&db, vec![rest(Binding::Positional)], s);
    let (id, pack, mut source) = reserve(&mut db, DeclKind::Alias, "Pack");
    source.result_kind = Kind::Schema;
    populate(&mut db, id, source, body, vec![]);
    let ones = items(
        &db,
        vec![item(Multiplicity::Repeated, Element::Positional(one))],
    );
    let ints = items(
        &db,
        vec![item(Multiplicity::Repeated, Element::Positional(int))],
    );
    let schema_apply = |db: &Database, base, arg, kind| {
        db.intern(Type::Apply {
            base,
            args: vec![Argument::Positional(arg)].into(),
            kind,
        })
    };
    let tuple_ones = schema_apply(&db, tuple, ones, Kind::Type);
    let tuple_ints = schema_apply(&db, tuple, ints, Kind::Type);
    let packed = schema_apply(&db, pack, schema(&db, &[one, two]), Kind::Schema);
    let expanded = db.intern(Type::Apply {
        base: tuple,
        args: vec![Argument::Expand(ones)].into(),
        kind: Kind::Type,
    });
    let [named_one, named_two] = [one, two].map(|arg| apply(&db, named, &[arg]));
    db.seal();
    assert_eq!(check(&db, tuple_ones, tuple_ints).status, Status::Proven);
    assert_eq!(
        check(&db, tuple_ints, tuple_ones).status,
        Status::Unresolved
    );
    assert_eq!(check(&db, packed, ints).status, Status::Proven);
    assert_eq!(check(&db, named_one, named_one).status, Status::Proven);
    assert_eq!(
        check(&db, named_one, named_two).status,
        Status::Contradicted
    );
    assert!(has(
        &check(&db, expanded, tuple_ints),
        Residual::GenericArguments.into()
    ));
}

#[test]
#[should_panic(expected = "implicit binder applied")]
fn applying_an_implicit_binder_panics() {
    let mut db = Database::new();
    let ambient = nominal(
        &mut db,
        "Ambient",
        vec![bounded(Kind::Type, Binding::Implicit, None)],
        vec![],
    );
    let applied = apply(&db, ambient, &[db.top()]);
    db.seal();
    check(&db, applied, applied);
}

#[test]
fn rest_shapes_admit_items_of_their_kinds() {
    let mut db = Database::new();
    let int = int(&mut db);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let [one, two] = [1, 2].map(|n| literal(&db, n));
    let top = db.top();
    let name = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("a"))));
    let text = db.intern(Type::Literal(Literal::Str("a".into())));
    let positional = |ty| item(Multiplicity::Repeated, Element::Positional(ty));
    let keyed = |key, value| item(Multiplicity::Repeated, Element::Keyed { key, value });
    let ints = items(&db, vec![positional(int)]);
    let options = items(&db, vec![keyed(sym, top)]);
    let anything = items(&db, vec![positional(top), keyed(sym, top)]);
    let empty = items(&db, vec![]);
    let pair = schema(&db, &[one, two]);
    let named = items(
        &db,
        vec![item(
            Multiplicity::Required,
            Element::Keyed {
                key: name,
                value: one,
            },
        )],
    );
    let texted = items(
        &db,
        vec![item(
            Multiplicity::Required,
            Element::Keyed {
                key: text,
                value: one,
            },
        )],
    );
    let maybe = items(
        &db,
        vec![item(Multiplicity::Optional, Element::Positional(one))],
    );
    let nested = items(
        &db,
        vec![
            item(
                Multiplicity::Required,
                Element::Include(schema(&db, &[one])),
            ),
            item(
                Multiplicity::Repeated,
                Element::Include(schema(&db, &[two])),
            ),
            positional(one),
        ],
    );
    db.seal();
    for (a, b) in [
        (pair, ints),
        (nested, ints),
        (named, options),
        (named, anything),
        (pair, anything),
        (empty, empty),
        (empty, ints),
    ] {
        assert_eq!(check(&db, a, b).status, Status::Proven);
    }
    let item = Issue::Contradiction(Contradiction::Excess(0));
    for (a, b) in [(pair, options), (named, ints), (maybe, empty)] {
        let result = check(&db, a, b);
        assert_eq!(result.status, Status::Contradicted);
        assert!(has(&result, item));
    }
    assert!(has(
        &check(&db, texted, anything),
        Issue::Contradiction(Contradiction::UnrelatedNominals)
    ));
}

#[test]
fn rest_shape_inclusions_of_rigids_and_unknown() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let [s, ts] = [0, 1].map(|slot| {
        db.intern(Type::Bound {
            reference: BoundRef::new(0, slot),
            kind: Kind::Schema,
        })
    });
    let positional = |ty| item(Multiplicity::Repeated, Element::Positional(ty));
    let include = |ty| item(Multiplicity::Required, Element::Include(ty));
    let ints = items(&db, vec![positional(int)]);
    let nums = items(&db, vec![positional(num)]);
    let tops = items(&db, vec![positional(db.top())]);
    let keys = items(
        &db,
        vec![item(
            Multiplicity::Repeated,
            Element::Keyed {
                key: db.top(),
                value: db.top(),
            },
        )],
    );
    let of_s = items(&db, vec![include(s)]);
    let of_ts = items(&db, vec![include(ts)]);
    let of_unknown = items(&db, vec![include(db.unknown_schema())]);
    let one = schema(&db, &[literal(&db, 1)]);
    let body = function(&db, &[], db.top());
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Schema, Binding::Positional, Some(ints)),
            bounded(Kind::Schema, Binding::Rest(Rest::Positional), None),
        ],
        body,
    );
    db.seal();
    assert_eq!(under(&db, f, of_s, nums).status, Status::Proven);
    assert_eq!(under(&db, f, of_ts, tops).status, Status::Proven);
    assert_eq!(under(&db, f, of_ts, keys).status, Status::Contradicted);
    assert_eq!(check(&db, of_unknown, ints).status, Status::Proven);
    assert_eq!(check(&db, one, db.unknown_schema()).status, Status::Proven);

    let mut solver = Solver::new(&db);
    let env = solver.rigid_environment(f);
    solver.constrain(
        solver.view(of_s, env),
        solver.view(nums, env),
        Provenance::default(),
    );
    assert_eq!(solver.solve()[0].status, Status::Proven);
    let root = solver.obligation(solver.roots[0].obligation);
    let (child, _) = root
        .active
        .borrow()
        .iter()
        .find(|(_, step)| *step == Step::Item(0))
        .cloned()
        .expect("the inclusion is derived");
    let child = solver.obligation(child);
    assert!(
        child
            .active
            .borrow()
            .iter()
            .any(|(_, step)| *step == Step::RigidBound)
    );
}

#[test]
fn positional_items_are_distributed_by_count() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let s = |db: &Database, items: Vec<SchemaItem>| self::items(db, items);
    let ints = |db: &Database, ms: &[Multiplicity]| {
        s(db, ms.iter().map(|&m| positional(m, int)).collect())
    };
    let one = ints(&db, &[Req]);
    let two = ints(&db, &[Req, Req]);
    let three = ints(&db, &[Req, Req, Req]);
    let empty = ints(&db, &[]);
    let any = ints(&db, &[Rep]);
    let maybe = ints(&db, &[Opt]);
    let prefix = ints(&db, &[Req, Opt]);
    let signature = s(
        &db,
        vec![
            positional(Req, num),
            positional(Opt, num),
            positional(Rep, str),
        ],
    );
    let spill = s(&db, vec![positional(Req, int), positional(Rep, str)]);
    let optional_rest = s(&db, vec![positional(Opt, num), positional(Rep, num)]);
    let optional_str = s(&db, vec![positional(Opt, int), positional(Rep, str)]);
    let twice = ints(&db, &[Rep, Rep]);
    let nested = s(&db, vec![positional(Req, int), include(Req, two)]);
    db.seal();
    for (a, b) in [
        (one, signature),
        (two, signature),
        (prefix, prefix),
        (prefix, signature),
        (any, optional_rest),
        (empty, maybe),
        (three, ints(&db, &[Req, Opt, Rep])),
        (nested, three),
    ] {
        assert_eq!(check(&db, a, b).status, Status::Proven, "{a:?} <: {b:?}");
    }
    // The second item goes to the optional parameter, as the runtime binds it
    assert!(contradiction(
        &check(&db, three, signature),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, two, spill),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, any, optional_str),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, empty, one),
        Contradiction::Missing(0)
    ));
    assert!(contradiction(
        &check(&db, maybe, one),
        Contradiction::Missing(0)
    ));
    assert!(contradiction(
        &check(&db, three, prefix),
        Contradiction::Excess(2)
    ));
    assert!(contradiction(
        &check(&db, any, maybe),
        Contradiction::Excess(0)
    ));
    // The excess item is the inclusion it came from
    assert!(contradiction(
        &check(&db, nested, two),
        Contradiction::Excess(1)
    ));
    assert!(residual(&check(&db, one, twice), Residual::Alignment));
}

#[test]
fn literal_keys_own_their_items_and_others_go_to_the_domain() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let [a, b] = ["a", "b"].map(|k| db.intern(Type::Literal(Literal::Sym(db.intern_symbol(k)))));
    let text = db.intern(Type::Literal(Literal::Str("b".into())));
    let s = |db: &Database, items: Vec<SchemaItem>| self::items(db, items);
    let named = |db: &Database, keys: &[(Multiplicity, TypeId)]| {
        s(db, keys.iter().map(|&(m, k)| keyed(m, k, int)).collect())
    };
    let a_once = named(&db, &[(Req, a)]);
    let a_twice = named(&db, &[(Req, a), (Req, a)]);
    let a_maybe = named(&db, &[(Opt, a)]);
    let a_many = named(&db, &[(Rep, a)]);
    let a_b = named(&db, &[(Req, a), (Req, b)]);
    let b_once = named(&db, &[(Req, b)]);
    let texted = s(&db, vec![keyed(Req, text, int)]);
    let empty = named(&db, &[]);
    let options = named(&db, &[(Rep, sym)]);
    let with_options = |db: &Database, m| s(db, vec![keyed(m, a, int), keyed(Rep, sym, int)]);
    let required = with_options(&db, Req);
    let optional = with_options(&db, Opt);
    let repeated = with_options(&db, Rep);
    let a_str = s(&db, vec![keyed(Req, a, str)]);
    db.seal();
    for (x, y) in [
        (a_once, a_once),
        (a_once, a_maybe),
        (empty, a_maybe),
        (a_b, required),
        (b_once, optional),
        (options, repeated),
        (a_twice, a_many),
    ] {
        assert_eq!(check(&db, x, y).status, Status::Proven, "{x:?} <: {y:?}");
    }
    assert!(contradiction(
        &check(&db, empty, a_once),
        Contradiction::Missing(0)
    ));
    // Items of a literal key past its count go to a remainder that admits it
    let outcome = check(&db, a_twice, required);
    assert_eq!(outcome.status, Status::Proven, "{outcome:?}");
    assert!(contradiction(
        &check(&db, a_many, a_maybe),
        Contradiction::Excess(0)
    ));
    assert!(contradiction(
        &check(&db, b_once, a_maybe),
        Contradiction::Excess(0)
    ));
    // A domain might hold the literal key any number of times, which only a
    // remainder admitting it can take
    let outcome = check(&db, options, optional);
    assert_eq!(outcome.status, Status::Proven, "{outcome:?}");
    assert!(contradiction(
        &check(&db, options, a_maybe),
        Contradiction::Excess(0)
    ));
    assert!(contradiction(
        &check(&db, options, required),
        Contradiction::Missing(0)
    ));
    assert!(contradiction(
        &check(&db, texted, optional),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, a_once, a_str),
        Contradiction::UnrelatedNominals
    ));
}

#[test]
fn domains_own_what_they_admit_narrowest_first() {
    use Multiplicity::{Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Str, str);
    let top = db.top();
    let [a, b, c] =
        ["a", "b", "c"].map(|k| db.intern(Type::Literal(Literal::Sym(db.intern_symbol(k)))));
    let text = db.intern(Type::Literal(Literal::Str("x".into())));
    // A lookup of `key`'s values as `value`: `{*(key): value, ...}`
    let lookup =
        |db: &Database, key, value| items(db, vec![keyed(Rep, key, value), keyed(Rep, top, top)]);
    let a_b = items(&db, vec![keyed(Req, a, int), keyed(Req, b, str)]);
    let a_once = items(&db, vec![keyed(Req, a, int)]);
    let syms = items(&db, vec![keyed(Rep, sym, int)]);
    let mixed = items(&db, vec![keyed(Req, a, int), keyed(Req, text, str)]);
    let open = items(&db, vec![keyed(Rep, top, top)]);
    let [a_int, a_str, c_int, sym_int, sym_str] =
        [(a, int), (a, str), (c, int), (sym, int), (sym, str)]
            .map(|(key, value)| lookup(&db, key, value));
    let k = reference(&db, 0, 0);
    let v = reference(&db, 0, 1);
    let variable = lookup(&db, k, v);
    db.seal();
    for (x, y) in [
        (a_b, a_int),
        (syms, a_int),
        // The key no domain but the widest admits goes there
        (mixed, sym_int),
        // Nothing has the key
        (a_once, c_int),
    ] {
        let outcome = check(&db, x, y);
        assert_eq!(
            outcome.status,
            Status::Proven,
            "{x:?} <: {y:?}: {outcome:?}"
        );
    }
    for (x, y) in [
        (a_b, a_str),
        // `Sym` owns the items, though the widest domain would take them
        (syms, sym_str),
    ] {
        assert!(
            contradiction(&check(&db, x, y), Contradiction::UnrelatedNominals),
            "{x:?} <: {y:?}"
        );
    }
    // Items of an open schema may have keys `Sym` owns, and `Top <: Int` isn't
    // decided
    let outcome = check(&db, open, sym_int);
    assert!(
        residual(
            &outcome,
            Residual::Unsupported("a structural type below a class")
        ),
        "{outcome:?}"
    );

    // An unsolved key waits for its solution
    let mut s = Solver::new(&db);
    let (key, value) = (s.infer(), s.infer());
    let e = s.intern_environment(s.empty_environment(), vec![key, value]);
    s.constrain(s.closed(a_b), s.view(variable, e), Provenance::default());
    assert!(residual(&s.solve()[0], Residual::Inference));
    s.constrain(s.closed(a), key, Provenance::default());
    s.solve();
    assert_eq!(s.default_with(variable_id(key), false), Ok(a));
    s.solve();
    // The key's item gives the value
    assert_eq!(s.default(variable_id(value)), Ok(int));
    assert_eq!(s.solve()[0].status, Status::Proven);
}

#[test]
fn inclusions_splice_or_take_on_their_multiplicity() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let one = items(&db, vec![positional(Req, int)]);
    let two = items(&db, vec![positional(Req, int), positional(Req, int)]);
    let spliced = items(&db, vec![include(Req, two)]);
    let maybe = items(&db, vec![include(Opt, one)]);
    let many = items(&db, vec![include(Rep, one)]);
    let pairs = items(&db, vec![include(Rep, two)]);
    let expected_maybe = items(&db, vec![positional(Opt, int)]);
    let prefix = items(&db, vec![positional(Req, int), positional(Opt, int)]);
    db.seal();
    assert_eq!(check(&db, spliced, two).status, Status::Proven);
    assert_eq!(check(&db, maybe, expected_maybe).status, Status::Proven);
    assert!(contradiction(
        &check(&db, many, one),
        Contradiction::Missing(0)
    ));
    let outcome = check(&db, pairs, prefix);
    assert!(
        residual(
            &outcome,
            Residual::Unsupported("a repeated inclusion of several items")
        ),
        "{outcome:?}"
    );
}

#[test]
fn opaque_rigids_pair_up_or_stand_for_their_bounds() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let [s, ts] = [0, 1].map(|slot| {
        db.intern(Type::Bound {
            reference: BoundRef::new(0, slot),
            kind: Kind::Schema,
        })
    });
    let ints = items(&db, vec![positional(Rep, int)]);
    let of_s = items(&db, vec![include(Req, s)]);
    let led = items(&db, vec![positional(Req, int), include(Req, ts)]);
    let led_maybe = items(&db, vec![positional(Opt, int), include(Req, ts)]);
    let trailed = items(&db, vec![include(Req, ts), positional(Req, int)]);
    let trailed_num = items(&db, vec![include(Req, ts), positional(Req, num)]);
    let nums = items(&db, vec![positional(Opt, num), positional(Rep, num)]);
    let one = items(&db, vec![positional(Req, int)]);
    let of_ts = items(&db, vec![include(Req, ts)]);
    let body = function(&db, &[], db.top());
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Schema, Binding::Positional, Some(ints)),
            bounded(Kind::Schema, Binding::Rest(Rest::Positional), None),
        ],
        body,
    );
    db.seal();
    for (a, b) in [(led, led), (trailed, trailed_num), (of_s, nums)] {
        assert_eq!(under(&db, f, a, b).status, Status::Proven, "{a:?} <: {b:?}");
    }
    let result = under(&db, f, one, of_ts);
    assert!(contradiction(&result, Contradiction::Rigid));
    assert!(residual(
        &under(&db, f, led, led_maybe),
        Residual::Alignment
    ));
    // A pack stands for its bound, which may be empty
    assert!(contradiction(
        &under(&db, f, of_ts, one),
        Contradiction::Missing(0)
    ));
}

#[test]
fn the_dynamic_schema_leaves_its_lanes_unchecked() {
    use Multiplicity::{Optional as Opt, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let [a, b] = ["a", "b"].map(|k| db.intern(Type::Literal(Literal::Sym(db.intern_symbol(k)))));
    let unknown = include(Req, db.unknown_schema());
    let open_int = items(&db, vec![positional(Req, int), unknown.clone()]);
    let strs = items(&db, vec![positional(Req, str)]);
    let open_a_str = items(&db, vec![keyed(Req, a, str), unknown.clone()]);
    let open_a_twice = items(
        &db,
        vec![keyed(Req, a, int), keyed(Req, a, int), unknown.clone()],
    );
    let open = items(&db, vec![unknown.clone()]);
    let a_int = items(&db, vec![keyed(Req, a, int)]);
    let a_str_b = items(&db, vec![keyed(Req, a, str), keyed(Req, b, int)]);
    let b_int = items(&db, vec![keyed(Req, b, int)]);
    let open_a_int = items(&db, vec![keyed(Req, a, int), unknown.clone()]);
    let open_maybe_a = items(&db, vec![keyed(Opt, a, int), unknown]);
    db.seal();
    for (x, y) in [(open_int, strs), (open, a_int), (b_int, open_maybe_a)] {
        assert_eq!(check(&db, x, y).status, Status::Proven, "{x:?} <: {y:?}");
    }
    // Explicit keyed items are still what they say
    assert!(contradiction(
        &check(&db, open_a_str, a_int),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, a_str_b, open_a_int),
        Contradiction::UnrelatedNominals
    ));
    assert!(contradiction(
        &check(&db, open_a_twice, a_int),
        Contradiction::Excess(1)
    ));
}

#[test]
fn positional_items_under_int_keys_are_not_yet_decided() {
    use Multiplicity::{Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = int(&mut db);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    let one = items(&db, vec![positional(Req, int)]);
    let by_int = items(&db, vec![keyed(Rep, int, int)]);
    let by_sym = items(&db, vec![keyed(Rep, sym, int)]);
    let a = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("a"))));
    let named_by_int = items(&db, vec![keyed(Req, a, int), keyed(Rep, int, int)]);
    db.seal();
    let int_keyed = Residual::Unsupported("a position that may be an Int-keyed item");
    let outcome = check(&db, one, by_int);
    assert!(residual(&outcome, int_keyed), "{outcome:?}");
    let outcome = check(&db, one, named_by_int);
    let int_keyed = Residual::Unsupported("positions that may be Int-keyed items");
    assert!(residual(&outcome, int_keyed), "{outcome:?}");
    assert!(contradiction(
        &check(&db, one, by_sym),
        Contradiction::Excess(0)
    ));
}

#[test]
fn a_remainder_admitting_a_literal_key_takes_its_further_items() {
    use Multiplicity::{Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = int(&mut db);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let base = nominal(&mut db, "A", vec![], vec![]);
    let derived = nominal(&mut db, "B", vec![], vec![base]);
    let from = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("from"))));
    // A method's `(self, :from, ...args)`: the rest may hold another `from`,
    // which the other's rest admits
    let method = |receiver| {
        let params = items(
            &db,
            vec![
                positional(Req, receiver),
                keyed(Req, from, str),
                positional(Rep, int),
                keyed(Rep, sym, int),
            ],
        );
        db.intern(Type::Function(Function {
            params,
            result: int,
            input: None,
            output: None,
        }))
    };
    let on_base = method(base);
    let on_derived = method(derived);
    let with_rest = items(&db, vec![keyed(Req, from, str), keyed(Rep, sym, int)]);
    let only = items(&db, vec![keyed(Req, from, str)]);
    let twice = items(&db, vec![keyed(Req, from, str), keyed(Req, from, str)]);
    db.seal();
    let outcome = check(&db, on_base, on_derived);
    assert_eq!(outcome.status, Status::Proven, "{outcome:?}");
    // The rest's other keys still need somewhere to go
    assert!(contradiction(
        &check(&db, with_rest, only),
        Contradiction::Excess(1)
    ));
    // Without a remainder there is nowhere for another `from` to go
    assert!(contradiction(
        &check(&db, twice, only),
        Contradiction::Excess(1)
    ));
    // Either `from` may be the one the remainder takes, so both must fit it
    assert!(contradiction(
        &check(&db, twice, with_rest),
        Contradiction::UnrelatedNominals
    ));
}

/// A schema reference to slot `slot` of the innermost group
fn pack(db: &Database, slot: usize) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(0, slot),
        kind: Kind::Schema,
    })
}

fn map(db: &Database, packs: &[TypeId], pattern: TypeId) -> TypeId {
    db.intern(Type::Map {
        packs: packs.iter().copied().collect(),
        pattern,
    })
}

/// A schema alias over schema binders whose body is `body`, applied to `args`,
/// so a mapping in it is viewed with its packs substituted
fn applied(db: &mut Database, name: &str, body: TypeId, args: &[TypeId]) -> TypeId {
    let binders = args
        .iter()
        .map(|_| bounded(Kind::Schema, Binding::Positional, None))
        .collect();
    let body = quantified(db, binders, body);
    let (id, alias, mut source) = reserve(db, DeclKind::Alias, name);
    source.result_kind = Kind::Schema;
    populate(db, id, source, body, vec![]);
    db.intern(Type::Apply {
        base: alias,
        args: args.iter().copied().map(Argument::Positional).collect(),
        kind: Kind::Schema,
    })
}

#[test]
fn a_mapping_over_known_packs_relates_item_by_item() {
    use Multiplicity::{Optional as Opt, Required as Req};
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let pair = nominal(
        &mut db,
        "Pair",
        vec![binder(Variance::Covariant), binder(Variance::Covariant)],
        vec![],
    );
    let key = db.intern(Type::Literal(Literal::Sym(db.intern_symbol("k"))));
    let item = reference(&db, 0, 0);
    let boxes = map(&db, &[pack(&db, 0)], apply(&db, boxed, &[item]));
    let pairs = map(
        &db,
        &[pack(&db, 0), pack(&db, 1)],
        apply(&db, pair, &[item, reference(&db, 0, 1)]),
    );
    let ints = items(&db, vec![positional(Req, int), positional(Opt, int)]);
    let keyed_int = items(&db, vec![keyed(Req, key, int)]);
    let one = schema(&db, &[int]);
    let two = schema(&db, &[int, int]);
    let unknown = db.unknown_schema();
    let mapped = |db: &mut Database, name, body, args: &[TypeId]| {
        let applied = applied(db, name, body, args);
        items(db, vec![include(Req, applied)])
    };
    let boxed_ints = mapped(&mut db, "BoxedInts", boxes, &[ints]);
    let boxed_key = mapped(&mut db, "BoxedKey", boxes, &[keyed_int]);
    let boxed_unknown = mapped(&mut db, "BoxedUnknown", boxes, &[unknown]);
    let paired = mapped(&mut db, "Paired", pairs, &[one, one]);
    let misaligned = mapped(&mut db, "Misaligned", pairs, &[one, two]);
    let box_num = apply(&db, boxed, &[num]);
    let box_nums = items(
        &db,
        vec![positional(Req, box_num), positional(Opt, box_num)],
    );
    let key_box_num = items(&db, vec![keyed(Req, key, box_num)]);
    let pair_ints = schema(&db, &[apply(&db, pair, &[int, int])]);
    let box_one = schema(&db, &[box_num]);
    db.seal();
    for (a, b) in [
        (boxed_ints, box_nums),
        (boxed_key, key_box_num),
        (boxed_unknown, box_one),
        (paired, pair_ints),
    ] {
        assert_eq!(check(&db, a, b).status, Status::Proven, "{a:?} <: {b:?}");
    }
    assert!(contradiction(
        &check(&db, boxed_ints, box_one),
        Contradiction::Excess(0)
    ));
    assert!(contradiction(
        &check(&db, misaligned, pair_ints),
        Contradiction::MappedPacks
    ));
}

#[test]
fn a_mapping_over_a_rigid_pairs_up_or_stands_for_its_bound() {
    use Multiplicity::Required as Req;
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let cell = nominal(&mut db, "Cell", vec![binder(Variance::Invariant)], vec![]);
    let item = reference(&db, 0, 0);
    let ints = items(&db, vec![positional(Multiplicity::Repeated, int)]);
    let mapped = |db: &Database, base| {
        let mapping = map(db, &[pack(db, 0)], apply(db, base, &[item]));
        items(db, vec![include(Req, mapping)])
    };
    let boxes = mapped(&db, boxed);
    let cells = mapped(&db, cell);
    let rest = |db: &Database, base, of| {
        items(
            db,
            vec![positional(Multiplicity::Repeated, apply(db, base, &[of]))],
        )
    };
    let box_nums = rest(&db, boxed, num);
    let cell_ints = rest(&db, cell, int);
    let box_int = schema(&db, &[apply(&db, boxed, &[int])]);
    let anything = items(&db, vec![positional(Multiplicity::Repeated, db.top())]);
    let body = function(&db, &[], db.top());
    let f = generic(
        &mut db,
        vec![bounded(
            Kind::Schema,
            Binding::Rest(Rest::Positional),
            Some(ints),
        )],
        body,
    );
    db.seal();
    assert_eq!(under(&db, f, boxes, boxes).status, Status::Proven);
    // A covariant pattern maps the pack's bound to a bound of the mapping
    assert_eq!(under(&db, f, boxes, box_nums).status, Status::Proven);
    assert!(residual(
        &under(&db, f, cells, cell_ints),
        Residual::Unsupported(
            "a mapping over a rigid that its pattern doesn't preserve the order of"
        )
    ));
    // Unless the expected side takes any item in the mapping's lanes
    assert_eq!(under(&db, f, cells, anything).status, Status::Proven);
    // Nothing but itself is known to be below the mapping
    assert!(contradiction(
        &under(&db, f, box_int, boxes),
        Contradiction::Rigid
    ));
}

#[test]
fn a_mapping_over_a_pack_being_inferred_pairs_with_one_of_its_pattern() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let mapping = map(
        &db,
        &[pack(&db, 0)],
        apply(&db, boxed, &[reference(&db, 0, 0)]),
    );
    let params = items(&db, vec![include(Multiplicity::Required, mapping)]);
    let takes = |db: &Database, params| {
        db.intern(Type::Function(Function {
            params,
            result: db.top(),
            input: None,
            output: None,
        }))
    };
    // [*Us] (...Box[Us]) -> Value
    let generic = quantified(
        &db,
        vec![bounded(Kind::Schema, Binding::Rest(Rest::Positional), None)],
        takes(&db, params),
    );
    let concrete = takes(&db, schema(&db, &[apply(&db, boxed, &[int])]));
    db.seal();
    assert_eq!(check(&db, generic, generic).status, Status::Proven);
    // Choosing the pack from the items it maps to is not supported
    assert!(residual(
        &check(&db, generic, concrete),
        Residual::Unsupported("a mapping over a pack being inferred")
    ));
}
