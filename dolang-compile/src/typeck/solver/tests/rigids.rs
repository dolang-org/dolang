use super::*;

#[test]
fn rigids_are_below_only_themselves_and_their_bounds() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let unknown = db.unknown();
    let [t, u, v] = [0, 1, 2].map(|slot| reference(&db, 0, slot));
    let body = function(&db, &[t, u, v], t);
    // f[T @ Num, U @ T, V]
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Type, Binding::Positional, Some(num)),
            bounded(Kind::Type, Binding::Positional, Some(t)),
            binder(Variance::Invariant),
        ],
        body,
    );
    db.seal();
    let top = db.top();
    let bottom = db.bottom();
    for (a, b) in [
        (t, t),
        (t, top),
        (t, unknown),
        (unknown, t),
        (bottom, t),
        (t, num),
        (u, t),
        (u, num),
    ] {
        assert_eq!(under(&db, f, a, b).status, Status::Proven, "{a:?} <: {b:?}");
    }
    for (a, b) in [(t, int), (int, t), (t, v), (v, num), (v, t)] {
        let outcome = under(&db, f, a, b);
        assert_eq!(outcome.status, Status::Contradicted, "{a:?} <: {b:?}");
    }
    assert!(has(
        &under(&db, f, int, t),
        Issue::Contradiction(Contradiction::Rigid)
    ));
}

#[test]
fn f_bounds_and_unions_hold_through_bounds() {
    let mut db = Database::new();
    let nil = db.intern(Type::Literal(Literal::Nil));
    let one = literal(&db, 1);
    let cmp = nominal(&mut db, "Cmp", vec![binder(Variance::Covariant)], vec![]);
    let boxed = nominal(&mut db, "Box", vec![binder(Variance::Covariant)], vec![]);
    let [t, v] = [0, 1].map(|slot| reference(&db, 0, slot));
    let cmp_t = apply(&db, cmp, &[t]);
    let body = function(&db, &[t, v], t);
    // f[T @ Cmp[T], V]
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Type, Binding::Positional, Some(cmp_t)),
            binder(Variance::Invariant),
        ],
        body,
    );
    let t_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(t), UnionMember::Type(nil)].into(),
    ));
    let cmp_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(cmp_t), UnionMember::Type(nil)].into(),
    ));
    let box_t = apply(&db, boxed, &[t]);
    let box_top = apply(&db, boxed, &[db.top()]);
    let box_or_nil = db.intern(Type::Union(
        vec![UnionMember::Type(box_top), UnionMember::Type(nil)].into(),
    ));
    let one_or_unknown = db.intern(Type::Union(
        vec![UnionMember::Type(one), UnionMember::Type(db.unknown())].into(),
    ));
    db.seal();
    for (a, b) in [
        (t, cmp_t),
        (t, t_or_nil),
        (t, cmp_or_nil),
        (box_t, box_or_nil),
        (v, one_or_unknown),
    ] {
        assert_eq!(under(&db, f, a, b).status, Status::Proven, "{a:?} <: {b:?}");
    }
}

#[test]
fn rest_rigids_are_bounded_by_their_shapes() {
    for registered in [false, true] {
        let mut db = Database::new();
        let sym = nominal(&mut db, "Sym", vec![], vec![]);
        if registered {
            db.set_intrinsic(Intrinsic::Sym, sym);
        }
        let key = if registered { sym } else { db.unknown() };
        let top = db.top();
        let item = |multiplicity, element| SchemaItem {
            multiplicity,
            element,
        };
        let positional = item(Multiplicity::Repeated, Element::Positional(top));
        let keyed = item(Multiplicity::Repeated, Element::Keyed { key, value: top });
        let shapes = [
            vec![positional.clone()],
            vec![keyed.clone()],
            vec![positional, keyed],
        ]
        .map(|items| db.intern(Type::Schema(items.into())));
        let rests = [Rest::Positional, Rest::Keyed, Rest::All];
        let refs = [0, 1, 2].map(|slot| {
            db.intern(Type::Bound {
                reference: BoundRef::new(0, slot),
                kind: Kind::Schema,
            })
        });
        let body = db.intern(Type::Function(Function {
            params: db.intern(Type::Schema(
                refs.iter()
                    .map(|&ty| item(Multiplicity::Required, Element::Include(ty)))
                    .collect(),
            )),
            result: top,
            input: None,
            output: None,
        }));
        let f = generic(
            &mut db,
            rests
                .iter()
                .map(|&rest| bounded(Kind::Schema, Binding::Rest(rest), None))
                .collect(),
            body,
        );
        db.seal();
        for (rest, shape) in refs.into_iter().zip(shapes) {
            assert_eq!(under(&db, f, rest, shape).status, Status::Proven);
            assert_eq!(under(&db, f, rest, rest).status, Status::Proven);
            assert_eq!(
                under(&db, f, rest, db.unknown_schema()).status,
                Status::Proven
            );
        }
    }
}

#[test]
fn bound_reductions_are_labeled() {
    let mut db = Database::new();
    let iter = nominal(&mut db, "Iter", vec![], vec![]);
    let [t, input] = [0, 1].map(|slot| reference(&db, 0, slot));
    let body = function(&db, &[t, input], t);
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Type, Binding::Positional, Some(iter)),
            bounded(Kind::Type, Binding::Implicit, Some(iter)),
        ],
        body,
    );
    db.seal();
    for (binder, step) in [(t, Step::RigidBound), (input, Step::ImplicitBound)] {
        let mut s = Solver::new(&db);
        let env = s.rigid_environment(f);
        s.constrain(
            s.view(binder, env),
            s.view(iter, env),
            Provenance::default(),
        );
        assert_eq!(s.solve()[0].status, Status::Proven);
        let root = s.obligation(s.roots[0].obligation);
        assert!(root.active.borrow().iter().any(|(_, found)| *found == step));
    }
}

#[test]
fn rigids_in_scope_can_be_assigned() {
    let mut db = Database::new();
    let t = reference(&db, 0, 0);
    let f = generic(&mut db, vec![binder(Variance::Invariant)], t);
    db.seal();
    let rigid = db.rigids(f)[0];
    let mut s = Solver::new(&db);
    let env = s.rigid_environment(f);
    let v = s.infer();
    s.constrain(s.view(t, env), v, Provenance::default());
    s.constrain(v, s.view(t, env), Provenance::default());
    assert!(s.solve().iter().all(|o| o.status == Status::Proven));
    assert_eq!(s.solution(variable_id(v)), Some(rigid));
}

#[test]
fn rigids_out_of_scope_have_escaped() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let f = generic(&mut db, vec![binder(Variance::Invariant)], t);
    let g = generic(
        &mut db,
        vec![bounded(Kind::Type, Binding::Positional, Some(num))],
        t,
    );
    db.seal();
    let foreign = db.rigids(g)[0];
    let mut s = Solver::new(&db);
    s.rigid_environment(f);
    for (a, b, status) in [
        (foreign, foreign, Status::Proven),
        (foreign, num, Status::Unresolved),
        (num, foreign, Status::Unresolved),
    ] {
        s.constrain(s.closed(a), s.closed(b), Provenance::default());
        let outcome = s.solve().pop().unwrap();
        assert_eq!(outcome.status, status);
        if status == Status::Unresolved {
            assert!(has(&outcome, Residual::Escape.into()));
        }
    }
    assert_eq!(s.reify(s.closed(foreign)), Err(Residual::Escape));
}

#[test]
fn reach_walks_rigids_through_their_bounds() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let base = nominal(&mut db, "Base", vec![binder(Variance::Covariant)], vec![]);
    let base_one = apply(&db, base, &[one]);
    let mid = nominal(&mut db, "Mid", vec![], vec![base_one]);
    let other = nominal(&mut db, "Other", vec![], vec![]);
    let [t, v] = [0, 1].map(|slot| reference(&db, 0, slot));
    let body = function(&db, &[t, v], t);
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Type, Binding::Positional, Some(mid)),
            binder(Variance::Invariant),
        ],
        body,
    );
    db.seal();
    let Type::Decl(base_decl) = *db.ty(base) else {
        unreachable!()
    };
    let mut s = Solver::new(&db);
    let env = s.rigid_environment(f);
    let Ok(Reach::Reached(args)) = s.reach(s.view(t, env), base_decl) else {
        panic!("a bounded rigid reaches its bound's ancestor");
    };
    assert_eq!(
        args.iter().map(|&arg| s.reify(arg)).collect::<Vec<_>>(),
        vec![Ok(one)]
    );
    for ty in [v, other] {
        assert!(matches!(
            s.reach(s.view(ty, env), base_decl),
            Ok(Reach::Unreached)
        ));
    }
    assert!(matches!(
        s.reach(s.closed(db.unknown()), base_decl),
        Ok(Reach::Dynamic)
    ));
}

#[test]
fn item_projections_imply_rigid_key_bounds() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let bound = items(&db, vec![keyed(Multiplicity::Repeated, str, int)]);
    let s = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    });
    let k = reference(&db, 0, 1);
    let binders = || {
        vec![
            bounded(Kind::Schema, Binding::Positional, Some(bound)),
            binder(Variance::Invariant),
        ]
    };
    // f[S @ {*(Str): Int}, K] (K) -> IndexItem[S, K]
    let body = function(&db, &[k], selecting(&db, false, s, k));
    let f = generic(&mut db, binders(), body);
    // g[S @ {*(Str): Int}, K] (K) -> Int
    let body = function(&db, &[k], int);
    let g = generic(&mut db, binders(), body);
    db.seal();
    // `K` is below `Keys[S]`, so below `Str`
    let mut solver = Solver::new(&db);
    let env = solver.rigid_environment(f);
    solver.constrain(
        solver.view(k, env),
        solver.view(str, env),
        Provenance::default(),
    );
    let outcome = solver.solve().remove(0);
    assert_eq!(outcome.status, Status::Proven, "{outcome:?}");
    assert!((solver.obligations.iter()).any(|o| {
        o.active
            .borrow()
            .iter()
            .any(|(_, step)| *step == Step::ImpliedBound)
    }));
    let outcome = under(&db, f, k, int);
    assert_eq!(outcome.status, Status::Contradicted, "{outcome:?}");
    // Without a projection by it, `K` is unbounded
    let outcome = under(&db, g, k, str);
    assert_eq!(outcome.status, Status::Contradicted, "{outcome:?}");
}

#[test]
fn a_lifted_binder_bounds_as_the_enclosing_rigid() {
    let mut db = Database::new();
    let s = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    });
    let keys = db.intern(Type::Union(vec![UnionMember::Keys(s)].into()));
    let schema = || bounded(Kind::Schema, Binding::Positional, None);
    let top = db.top();
    // c[S] encloses f[S, K @ Keys[S]], lifted over `S`
    let c = generic(&mut db, vec![schema()], top);
    let f = generic(
        &mut db,
        vec![
            schema(),
            bounded(Kind::Type, Binding::Positional, Some(keys)),
        ],
        top,
    );
    db.seal();
    let outer = db.rigids(c)[0];
    let k = db.rigids(f)[1];
    let expected = db.intern(Type::Union(vec![UnionMember::Keys(outer)].into()));
    // Checked as its body is, `f`'s `S` is `c`'s, so `K` is below `Keys` of it
    for grouped in [false, true] {
        let mut solver = Solver::new(&db);
        solver.assume(c);
        match grouped {
            true => solver.assume_group(f, vec![outer, k]),
            false => solver.assume(f),
        }
        let (k, expected) = (solver.closed(k), solver.closed(expected));
        solver.constrain(k, expected, Provenance::default());
        let outcome = solver.solve().remove(0);
        assert_eq!(
            outcome.status == Status::Proven,
            grouped,
            "{grouped}: {outcome:?}"
        );
    }
}
