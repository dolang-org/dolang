use super::*;
use crate::typeck::solver::unpack::{Tail, Unpacked};

/// A world with `Unpack[S]` and a few classes to put in schemas
struct World {
    db: Database,
    unpack: TypeId,
    int: TypeId,
    str: TypeId,
    bool: TypeId,
}

impl World {
    fn new() -> Self {
        let mut db = Database::new();
        let binder = Binder {
            variance: Variance::Covariant,
            ..bounded(Kind::Schema, Binding::Positional, None)
        };
        let unpack = nominal(&mut db, "Unpack", vec![binder], vec![]);
        let int = int(&mut db);
        let sym = nominal(&mut db, "Sym", vec![], vec![]);
        db.set_intrinsic(Intrinsic::Sym, sym);
        let str = nominal(&mut db, "Str", vec![], vec![]);
        let bool = nominal(&mut db, "Bool", vec![], vec![]);
        Self {
            db,
            unpack,
            int,
            str,
            bool,
        }
    }

    /// A class whose only supertype is `Unpack[{items}]`
    fn class(&mut self, items: Vec<SchemaItem>) -> TypeId {
        let schema = self::items(&self.db, items);
        let supertype = apply(&self.db, self.unpack, &[schema]);
        nominal(&mut self.db, "C", vec![], vec![supertype])
    }

    fn key(&self, name: &str) -> TypeId {
        let symbol = self.db.intern_symbol(name);
        self.db.intern(Type::Literal(Literal::Sym(symbol)))
    }

    /// The database, sealed once the first query needs it, so that every class
    /// must be made before
    fn sealed(&mut self) -> &Database {
        if !self.db.is_sealed() {
            self.db.seal();
        }
        &self.db
    }

    fn union(&mut self, types: &[TypeId]) -> TypeId {
        Solver::new(self.sealed()).lub(types[0], types[1])
    }

    fn walk(&mut self, ty: TypeId, pattern: &PatternShape) -> Option<Unpacked> {
        let Type::Decl(unpack) = *self.db.ty(self.unpack) else {
            unreachable!()
        };
        Solver::new(self.sealed()).unpack_pattern(ty, unpack, pattern)
    }
}

/// A pattern of positional slots, by whether each is defaulted, and keyed slots
fn pattern(positional: &[bool], keyed: &[(TypeId, bool)], rest: bool) -> PatternShape {
    PatternShape {
        positional: positional.to_vec(),
        keyed: keyed.to_vec(),
        positional_rest: rest,
        keyed_rest: rest,
    }
}

fn tail(positional: Vec<SchemaItem>, keyed: Vec<SchemaItem>) -> Option<Tail> {
    Some(Tail { positional, keyed })
}

#[test]
fn an_optional_item_splits_the_possibilities() {
    let mut w = World::new();
    let (int, str, bool) = (w.int, w.str, w.bool);
    let c = w.class(vec![
        positional(Multiplicity::Required, int),
        positional(Multiplicity::Optional, str),
        positional(Multiplicity::Repeated, bool),
    ]);
    let walked = w.walk(c, &pattern(&[false, false], &[], false)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![int, w.union(&[str, bool])]);
    // The rest takes what's left after the first item, whether the optional
    // item took it or not
    let walked = w.walk(c, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.slots, vec![int]);
    let left = vec![
        positional(Multiplicity::Optional, str),
        positional(Multiplicity::Repeated, bool),
    ];
    assert_eq!(walked.tail, tail(left, vec![]));
    let walked = w.walk(c, &pattern(&[false, false], &[], true)).unwrap();
    let left = vec![positional(Multiplicity::Repeated, bool)];
    assert_eq!(walked.tail, tail(left, vec![]));
}

#[test]
fn counts_that_cant_match_are_impossible() {
    let mut w = World::new();
    let int = w.int;
    let one = w.class(vec![positional(Multiplicity::Required, int)]);
    let two = w.class(vec![
        positional(Multiplicity::Required, int),
        positional(Multiplicity::Required, int),
    ]);
    let ints = w.class(vec![positional(Multiplicity::Repeated, int)]);
    let walked = w.walk(one, &pattern(&[false, false], &[], false)).unwrap();
    assert!(!walked.possible);
    let walked = w.walk(two, &pattern(&[false], &[], false)).unwrap();
    assert!(!walked.possible);
    let walked = w.walk(two, &pattern(&[false], &[], true)).unwrap();
    assert!(walked.possible);
    let left = vec![positional(Multiplicity::Required, int)];
    assert_eq!(walked.tail, tail(left, vec![]));
    // A repeated item may hold any number
    let walked = w.walk(ints, &pattern(&[false, false], &[], false)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![int, int]);
}

#[test]
fn defaulted_slots_may_be_left_unfilled() {
    let mut w = World::new();
    let int = w.int;
    let one = w.class(vec![positional(Multiplicity::Required, int)]);
    let none = w.class(vec![]);
    let walked = w.walk(one, &pattern(&[false, true], &[], false)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![int, w.db.bottom()]);
    let walked = w.walk(none, &pattern(&[false, true], &[], false)).unwrap();
    assert!(!walked.possible);
}

#[test]
fn literal_keys_are_taken_and_domains_kept() {
    let mut w = World::new();
    let (int, str, bool) = (w.int, w.str, w.bool);
    let (a, b, z) = (w.key("a"), w.key("b"), w.key("z"));
    let c = w.class(vec![
        keyed(Multiplicity::Required, a, int),
        keyed(Multiplicity::Optional, b, str),
        keyed(Multiplicity::Repeated, str, bool),
    ]);
    let walked = w.walk(c, &pattern(&[], &[(a, false)], true)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![int]);
    let left = vec![
        keyed(Multiplicity::Optional, b, str),
        keyed(Multiplicity::Repeated, str, bool),
    ];
    assert_eq!(walked.tail, tail(vec![], left));
    // `a` is left over, with no rest to take it
    let walked = w.walk(c, &pattern(&[], &[(b, false)], false)).unwrap();
    assert!(!walked.possible);
    // No item has the key `z`, unless it's defaulted
    let walked = w
        .walk(c, &pattern(&[], &[(a, false), (z, false)], true))
        .unwrap();
    assert!(!walked.possible);
    let walked = w
        .walk(c, &pattern(&[], &[(a, false), (z, true)], true))
        .unwrap();
    assert_eq!(walked.slots, vec![int, w.db.bottom()]);
}

#[test]
fn unions_join_their_members_walks() {
    let mut w = World::new();
    let (int, str, bool) = (w.int, w.str, w.bool);
    let first = w.class(vec![
        positional(Multiplicity::Required, int),
        positional(Multiplicity::Required, str),
    ]);
    let second = w.class(vec![positional(Multiplicity::Required, bool)]);
    let both = w.union(&[first, second]);
    let walked = w.walk(both, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.slots, vec![w.union(&[int, bool])]);
    let left = vec![positional(Multiplicity::Repeated, str)];
    assert_eq!(walked.tail, tail(left, vec![]));
    // A member the pattern can't match adds nothing
    let walked = w.walk(both, &pattern(&[false], &[], false)).unwrap();
    assert_eq!(walked.slots, vec![bool]);
}

#[test]
fn unknown_gives_unknown() {
    let mut w = World::new();
    let int = w.int;
    let c = w.class(vec![positional(Multiplicity::Required, int)]);
    let unknown_schema = w.db.unknown_schema();
    let open = w.class(vec![
        positional(Multiplicity::Required, int),
        include(Multiplicity::Required, unknown_schema),
    ]);
    let unknown = w.db.unknown();
    let either = w.db.intern(Type::Union(
        vec![UnionMember::Type(c), UnionMember::Type(unknown)].into(),
    ));
    let walked = w.walk(either, &pattern(&[false], &[], true)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![unknown]);
    assert_eq!(walked.tail, None);
    // An included dynamic schema is a repeated `Unknown`
    let walked = w.walk(open, &pattern(&[false, false], &[], false)).unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![int, unknown]);
}

#[test]
fn a_member_not_reaching_unpack_is_undecided() {
    let mut w = World::new();
    let int = w.int;
    assert_eq!(w.walk(int, &pattern(&[false], &[], false)), None);
}
