//! Recursive types, related coinductively: a judgment revisited past a guarded
//! step is assumed while it's proven.

use super::*;

fn union(db: &Database, members: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        members.iter().copied().map(UnionMember::Type).collect(),
    ))
}

/// `Int`, `Base[+T]`, and `Box[+T]: Base[T]`
fn classes(db: &mut Database) -> (TypeId, TypeId, TypeId) {
    let int = nominal(db, "Int", vec![], vec![]);
    let base = nominal(db, "Base", vec![binder(Variance::Covariant)], vec![]);
    let supertype = apply(db, base, &[reference(db, 0, 0)]);
    let boxed = nominal(
        db,
        "Box",
        vec![binder(Variance::Covariant)],
        vec![supertype],
    );
    (int, base, boxed)
}

/// An alias whose body `body` builds from the alias itself
fn recursive(
    db: &mut Database,
    name: &str,
    body: impl FnOnce(&Database, TypeId) -> TypeId,
) -> TypeId {
    let (id, ty, source) = reserve(db, DeclKind::Alias, name);
    let body = body(db, ty);
    populate(db, id, source, body, vec![]);
    ty
}

#[test]
fn recursive_unions_are_proven_cheaply() {
    // Proving Box[A] <: Base[B] needs A <: B, which probes Box[A] <: Base[B]
    // again, past the argument
    let mut db = Database::new();
    let (int, base, boxed) = classes(&mut db);
    let a = recursive(&mut db, "A", |db, a| {
        union(db, &[int, apply(db, boxed, &[a])])
    });
    let b = recursive(&mut db, "B", |db, b| {
        union(db, &[int, apply(db, base, &[b])])
    });
    db.seal();
    let gas = Gas::new(5_000);
    let mut s = Solver::new(&db, &gas);
    s.constrain(s.closed(a), s.closed(b), Provenance::default());
    let outcome = s.solve().remove(0);
    assert_eq!(outcome.status, Status::Proven, "{:?}", outcome.diagnostics);
}

#[test]
fn recursion_through_arguments_is_proven() {
    let mut db = Database::new();
    let (_, base, boxed) = classes(&mut db);
    let a = recursive(&mut db, "A", |db, a| apply(db, boxed, &[a]));
    let b = recursive(&mut db, "B", |db, b| apply(db, base, &[b]));
    db.seal();
    assert_eq!(check(&db, a, b).status, Status::Proven);
    assert_ne!(check(&db, b, a).status, Status::Proven);
}

#[test]
fn recursion_through_functions_is_proven() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let r = recursive(&mut db, "R", |db, r| function(db, &[], r));
    let s = recursive(&mut db, "S", |db, s| function(db, &[], s));
    // Through a parameter list's item, contravariantly
    let p = recursive(&mut db, "P", |db, p| function(db, &[p], int));
    let q = recursive(&mut db, "Q", |db, q| function(db, &[q], int));
    db.seal();
    assert_eq!(check(&db, r, s).status, Status::Proven);
    assert_eq!(check(&db, p, q).status, Status::Proven);
}

#[test]
fn mutual_recursion_is_proven() {
    let mut db = Database::new();
    let (int, base, boxed) = classes(&mut db);
    let (aid, a, asrc) = reserve(&mut db, DeclKind::Alias, "A");
    let (cid, c, csrc) = reserve(&mut db, DeclKind::Alias, "C");
    let body = union(&db, &[int, apply(&db, boxed, &[c])]);
    populate(&mut db, aid, asrc, body, vec![]);
    let body = apply(&db, boxed, &[a]);
    populate(&mut db, cid, csrc, body, vec![]);
    let b = recursive(&mut db, "B", |db, b| {
        union(db, &[int, apply(db, base, &[b])])
    });
    db.seal();
    assert_eq!(check(&db, a, b).status, Status::Proven);
    assert_eq!(check(&db, c, b).status, Status::Proven);
}

#[test]
fn instantiations_of_a_generic_recursive_alias_are_related() {
    // List[T] = Nil | Cons[T, List[T]], with Int: Num
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let nil = nominal(&mut db, "Nil", vec![], vec![]);
    let covariant = || binder(Variance::Covariant);
    let cons = nominal(&mut db, "Cons", vec![covariant(), covariant()], vec![]);
    let list = recursive(&mut db, "List", |db, list| {
        let t = reference(db, 0, 0);
        let cell = apply(db, cons, &[t, apply(db, list, &[t])]);
        quantified(db, vec![covariant()], union(db, &[nil, cell]))
    });
    let ints = apply(&db, list, &[int]);
    let nums = apply(&db, list, &[num]);
    db.seal();
    assert_eq!(check(&db, ints, nums).status, Status::Proven);
    assert_ne!(check(&db, nums, ints).status, Status::Proven);
}

#[test]
fn contradictions_under_an_assumption_contradict() {
    // A = Box[Pair[A, Str]] and B = Base[Pair[B, Int]]: assuming A <: B leaves
    // Str <: Int
    let mut db = Database::new();
    let (int, base, boxed) = classes(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let covariant = || binder(Variance::Covariant);
    let pair = nominal(&mut db, "Pair", vec![covariant(), covariant()], vec![]);
    let a = recursive(&mut db, "A", |db, a| {
        apply(db, boxed, &[apply(db, pair, &[a, str])])
    });
    let b = recursive(&mut db, "B", |db, b| {
        apply(db, base, &[apply(db, pair, &[b, int])])
    });
    db.seal();
    let outcome = check(&db, a, b);
    assert_eq!(outcome.status, Status::Contradicted);
    // Reported where Str <: Int is, past the arguments
    let contradiction = outcome
        .diagnostics
        .iter()
        .find(|d| matches!(d.issue, Issue::Contradiction(_)))
        .unwrap();
    assert!(contradiction.path.len() > 2, "{:?}", contradiction.path);
    assert!(!has(&outcome, Residual::Recursive.into()));
}

#[test]
fn unrelated_recursive_aliases_are_not_proven() {
    let mut db = Database::new();
    let (int, _, boxed) = classes(&mut db);
    let other = nominal(&mut db, "Other", vec![binder(Variance::Covariant)], vec![]);
    let a = recursive(&mut db, "A", |db, a| {
        union(db, &[int, apply(db, boxed, &[a])])
    });
    let c = recursive(&mut db, "C", |db, c| {
        union(db, &[int, apply(db, other, &[c])])
    });
    db.seal();
    assert_ne!(check(&db, a, c).status, Status::Proven);
    assert_ne!(check(&db, c, a).status, Status::Proven);
}

#[test]
fn an_included_schema_doesnt_guard() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let inner = schema(&db, &[int]);
    let outer = items(&db, vec![include(Multiplicity::Required, inner)]);
    db.seal();
    let gas = Gas::default();
    let s = Solver::new(&db, &gas);
    let subtype = |actual, expected| Relation::Subtype {
        actual: s.closed(actual),
        expected: s.closed(expected),
        fill: Fill::Contents,
    };
    let schemas = subtype(outer, outer);
    assert!(!s.guards(&Step::Item(0), schemas, subtype(inner, inner)));
    assert!(s.guards(&Step::Item(0), schemas, subtype(int, int)));
    assert!(s.guards(&Step::Key(0), schemas, subtype(int, int)));
    let argument = Step::Argument {
        index: 0,
        reversed: false,
    };
    assert!(s.guards(&argument, subtype(int, int), schemas));
    assert!(!s.guards(&Step::UnionMember(0), subtype(int, int), subtype(int, int)));
}
