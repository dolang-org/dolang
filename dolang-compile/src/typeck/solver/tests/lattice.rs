use super::*;
use crate::typeck::solver::lattice::{WIDENING_LIMIT, Widening};

fn union(db: &Database, types: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        types.iter().copied().map(UnionMember::Type).collect(),
    ))
}

#[test]
fn joins_drop_subsumed_members() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sub = nominal(&mut db, "Sub", vec![], vec![base]);
    let array = nominal(&mut db, "Array", vec![binder(Variance::Invariant)], vec![]);
    let one = literal(&db, 1);
    db.seal();
    let s = Solver::new(&db);

    assert_eq!(s.lub(one, int), int);
    assert_eq!(s.lub(int, one), int);
    assert_eq!(s.lub(sub, base), base);
    let int_str = union(&db, &[int, str]);
    assert_eq!(s.lub(int, str), int_str);
    // Joining what is already there changes nothing
    assert_eq!(s.lub(int_str, int), int_str);
    assert_eq!(s.lub(int_str, one), int_str);
    // `Unknown` absorbs, and bottom is the identity
    assert_eq!(s.lub(int, db.unknown()), db.unknown());
    assert_eq!(s.lub(int_str, db.unknown()), db.unknown());
    assert_eq!(s.lub(db.bottom(), int), int);
    assert_eq!(s.lub(db.bottom(), db.bottom()), db.bottom());
    // Invariant applications aren't merged
    let array_int = apply(&db, array, &[int]);
    let array_str = apply(&db, array, &[str]);
    assert_eq!(
        s.lub(array_int, array_str),
        union(&db, &[array_int, array_str])
    );
    // A member containing `Unknown` neither subsumes nor is subsumed
    let array_unknown = apply(&db, array, &[db.unknown()]);
    assert_eq!(
        s.lub(array_unknown, array_int),
        union(&db, &[array_unknown, array_int])
    );
}

#[test]
fn common_supertypes_are_the_least_shared_ancestor() {
    let mut db = Database::new();
    let t = reference(&db, 0, 0);
    let comparable = nominal(&mut db, "Comparable", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![comparable]);
    db.set_intrinsic(Intrinsic::Int, int);
    let str = nominal(&mut db, "Str", vec![], vec![comparable]);
    db.set_intrinsic(Intrinsic::Str, str);
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let a = nominal(&mut db, "A", vec![], vec![base]);
    let b = nominal(&mut db, "B", vec![], vec![base]);
    let sub = nominal(&mut db, "Sub", vec![], vec![a]);
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let iter_t = apply(&db, iter, &[t]);
    let array = nominal(
        &mut db,
        "Array",
        vec![binder(Variance::Invariant)],
        vec![iter_t],
    );
    let sink = nominal(
        &mut db,
        "Sink",
        vec![binder(Variance::Contravariant)],
        vec![],
    );
    let sink_t = apply(&db, sink, &[t]);
    let writer = nominal(
        &mut db,
        "Writer",
        vec![binder(Variance::Invariant)],
        vec![sink_t],
    );
    let thing = nominal(&mut db, "Thing", vec![], vec![]);
    let holder = nominal(
        &mut db,
        "Holder",
        vec![binder(Variance::Invariant)],
        vec![thing],
    );
    let holder_t = apply(&db, holder, &[t]);
    let boxed = nominal(
        &mut db,
        "Box",
        vec![binder(Variance::Invariant)],
        vec![holder_t],
    );
    let top = nominal(&mut db, "Top", vec![], vec![]);
    let left = nominal(&mut db, "Left", vec![], vec![top]);
    let right = nominal(&mut db, "Right", vec![], vec![top]);
    let x = nominal(&mut db, "X", vec![], vec![left, right]);
    let y = nominal(&mut db, "Y", vec![], vec![right, left]);
    let lone = nominal(&mut db, "Lone", vec![], vec![]);
    let one = literal(&db, 1);
    let text = db.intern(Type::Literal(Literal::Str("a".into())));
    db.seal();
    let s = Solver::new(&db);
    let common = |types: &[TypeId]| s.common_supertype(union(&db, types));

    assert_eq!(common(&[a, b]), Some(base));
    assert_eq!(common(&[sub, b]), Some(base));
    assert_eq!(common(&[one, text]), Some(comparable));
    assert_eq!(common(&[one, int]), Some(int));
    // A covariant argument joins
    assert_eq!(
        common(&[apply(&db, array, &[a]), apply(&db, array, &[lone])]),
        Some(apply(&db, iter, &[union(&db, &[a, lone])]))
    );
    // A contravariant argument takes the lowest
    assert_eq!(
        common(&[apply(&db, writer, &[a]), apply(&db, writer, &[sub])]),
        Some(apply(&db, sink, &[sub]))
    );
    // An invariant mismatch skips the ancestor
    assert_eq!(
        common(&[apply(&db, boxed, &[a]), apply(&db, boxed, &[b])]),
        Some(thing)
    );
    // A diamond takes the first least ancestor in the first member's MRO
    assert_eq!(common(&[x, y]), Some(left));
    assert_eq!(common(&[y, x]), Some(left));
    // Sharing only `Value` gives nothing
    assert_eq!(common(&[a, lone]), None);
    // A type other than a union is its own
    assert_eq!(s.common_supertype(a), Some(a));
}

#[test]
fn widening_goes_to_a_common_supertype_then_unknown() {
    let mut db = Database::new();
    let t = reference(&db, 0, 0);
    let int = int(&mut db);
    let nil = nominal(&mut db, "Nil", vec![], vec![]);
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let iter_t = apply(&db, iter, &[t]);
    let array = nominal(
        &mut db,
        "Array",
        vec![binder(Variance::Invariant)],
        vec![iter_t],
    );
    let one = literal(&db, 1);
    db.seal();
    let s = Solver::new(&db);

    // `x = [x]` in a loop, from an array: every member is an `Iter`
    let mut widening = Widening::default();
    let mut state = apply(&db, array, &[nil]);
    for step in 1..=WIDENING_LIMIT {
        state = widening.join(&s, state, apply(&db, array, &[state]));
        assert!(matches!(db.ty(state), Type::Union(_)), "step {step}");
    }
    state = widening.join(&s, state, apply(&db, array, &[state]));
    assert!(
        matches!(db.ty(state), Type::Apply { base, .. } if *base == iter),
        "{:?}",
        db.ty(state)
    );
    for _ in 0..=WIDENING_LIMIT {
        state = widening.join(&s, state, apply(&db, array, &[state]));
    }
    assert_eq!(state, db.unknown());
    state = widening.join(&s, state, apply(&db, array, &[state]));
    assert_eq!(state, db.unknown());

    // From `nil`, the members share only `Value`
    let mut widening = Widening::default();
    let mut state = nil;
    for _ in 0..=WIDENING_LIMIT {
        state = widening.join(&s, state, apply(&db, array, &[state]));
    }
    assert_eq!(state, db.unknown());

    // A join that stops growing never widens
    let mut widening = Widening::default();
    let mut state = one;
    for _ in 0..3 * WIDENING_LIMIT {
        state = widening.join(&s, state, int);
    }
    assert_eq!(state, int);
}

#[test]
fn a_fresh_literal_relates_as_its_regular_twin() {
    let mut db = Database::new();
    let int = int(&mut db);
    let (one, two) = (literal(&db, 1), literal(&db, 2));
    let one_two = union(&db, &[one, two]);
    db.seal();
    let (fresh_one, fresh_three) = (fresh(&db, 1), fresh(&db, 3));
    for (actual, expected) in [
        (fresh_one, one),
        (one, fresh_one),
        (fresh_one, int),
        (fresh_one, one_two),
    ] {
        assert_eq!(check(&db, actual, expected).status, Status::Proven);
    }
    assert!(contradiction(
        &check(&db, fresh_three, two),
        Contradiction::DistinctLiterals
    ));
    assert!(contradiction(
        &check(&db, fresh_three, one_two),
        Contradiction::Outside
    ));
    // A join keeps the regular twin, whatever the order
    let s = Solver::new(&db);
    assert_eq!(s.lub(fresh_one, one), one);
    assert_eq!(s.lub(one, fresh_one), one);
    assert_eq!(s.lub(fresh_one, fresh_one), fresh_one);
}
