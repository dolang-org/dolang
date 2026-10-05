use super::*;
use crate::{RestKind, typeck::solver::unpack::Unpacked};

/// A world with `Spread[S]`, `Unpack[S, :Rest @ Spread[S] = Spread[S]]`, and a few classes to
/// put in schemas
struct World {
    db: Database,
    spread: TypeId,
    unpack: TypeId,
    int: TypeId,
    str: TypeId,
    bool: TypeId,
}

/// The reference to binder `slot` of the group being declared
fn bound(db: &Database, slot: usize, kind: Kind) -> TypeId {
    db.intern(Type::Bound {
        reference: BoundRef::new(0, slot),
        kind,
    })
}

impl World {
    fn new() -> Self {
        let mut db = Database::new();
        let schema = Binder {
            variance: Variance::Covariant,
            ..bounded(Kind::Schema, Binding::Positional, None)
        };
        let spread = nominal(&mut db, "Spread", vec![schema.clone()], vec![]);
        let (id, unpack, source) = reserve(&mut db, DeclKind::Class, "Unpack");
        let s = bound(&db, 0, Kind::Schema);
        let rest = Binder {
            kind: Kind::Type,
            binding: Binding::Keyword(db.intern_symbol("Rest")),
            bound: Some(apply(&db, spread, &[s])),
            default: Some(apply(&db, spread, &[s])),
            variance: Variance::Covariant,
        };
        let body = quantified(&db, vec![schema, rest], unpack);
        let supertype = apply(&db, spread, &[s]);
        populate(&mut db, id, source, body, vec![supertype]);
        let int = int(&mut db);
        let sym = nominal(&mut db, "Sym", vec![], vec![]);
        db.set_intrinsic(Intrinsic::Sym, sym);
        let str = nominal(&mut db, "Str", vec![], vec![]);
        let bool = nominal(&mut db, "Bool", vec![], vec![]);
        Self {
            db,
            spread,
            unpack,
            int,
            str,
            bool,
        }
    }

    fn unpack_decl(&self) -> DeclId {
        let Type::Decl(unpack) = *self.db.ty(self.unpack) else {
            unreachable!()
        };
        unpack
    }

    /// `Unpack[{items}]`, with its default `Rest`
    fn unpack_of(&self, items: Vec<SchemaItem>) -> TypeId {
        let schema = self::items(&self.db, items);
        let unpack = self.unpack_decl();
        self.db.apply_defaults(unpack, &[schema]).unwrap()
    }

    /// `Spread[{items}]`, a rest by default
    fn spread_of(&self, items: Vec<SchemaItem>) -> TypeId {
        let schema = self::items(&self.db, items);
        apply(&self.db, self.spread, &[schema])
    }

    /// A class whose only supertype is `Unpack[{items}]`
    fn class(&mut self, items: Vec<SchemaItem>) -> TypeId {
        let supertype = self.unpack_of(items);
        nominal(&mut self.db, "C", vec![], vec![supertype])
    }

    /// A class with `binders` whose only supertype is `Unpack[{items}, Rest: rest]`,
    /// where `rest` is given the class itself
    fn rest_class(
        &mut self,
        binders: Vec<Binder>,
        items: Vec<SchemaItem>,
        rest: impl FnOnce(&Database, TypeId) -> TypeId,
    ) -> TypeId {
        let (id, class, source) = reserve(&mut self.db, DeclKind::Class, "R");
        let schema = self::items(&self.db, items);
        let rest = rest(&self.db, class);
        let supertype = apply(&self.db, self.unpack, &[schema, rest]);
        let body = quantified(&self.db, binders, class);
        populate(&mut self.db, id, source, body, vec![supertype]);
        class
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
        let unpack = self.unpack_decl();
        Solver::new(self.sealed()).unpack_pattern(ty, unpack, pattern)
    }
}

/// A pattern of positional slots, by whether each is defaulted, keyed slots, and
/// a `...` rest if `rest`
fn pattern(positional: &[bool], keyed: &[(TypeId, bool)], rest: bool) -> PatternShape {
    PatternShape {
        positional: positional.to_vec(),
        keyed: keyed.to_vec(),
        rests: if rest { vec![RestKind::Mixed] } else { vec![] },
    }
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
    let left = w.spread_of(vec![
        positional(Multiplicity::Optional, str),
        positional(Multiplicity::Repeated, bool),
    ]);
    assert_eq!(walked.rests, vec![left]);
    let walked = w.walk(c, &pattern(&[false, false], &[], true)).unwrap();
    let left = w.spread_of(vec![positional(Multiplicity::Repeated, bool)]);
    assert_eq!(walked.rests, vec![left]);
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
    let left = w.spread_of(vec![positional(Multiplicity::Required, int)]);
    assert_eq!(walked.rests, vec![left]);
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
    let left = w.spread_of(vec![
        keyed(Multiplicity::Optional, b, str),
        keyed(Multiplicity::Repeated, str, bool),
    ]);
    assert_eq!(walked.rests, vec![left]);
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
fn rests_take_their_own_lanes() {
    let mut w = World::new();
    let (int, str) = (w.int, w.str);
    let a = w.key("a");
    let c = w.class(vec![
        positional(Multiplicity::Required, int),
        keyed(Multiplicity::Required, a, str),
    ]);
    let split = PatternShape {
        positional: vec![],
        keyed: vec![],
        rests: vec![RestKind::Pos, RestKind::Key],
    };
    let walked = w.walk(c, &split).unwrap();
    let positional = w.spread_of(vec![positional(Multiplicity::Required, int)]);
    let keyed = w.spread_of(vec![keyed(Multiplicity::Required, a, str)]);
    assert_eq!(walked.rests, vec![positional, keyed]);
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
    let left = w.spread_of(vec![positional(Multiplicity::Repeated, str)]);
    assert_eq!(walked.rests, vec![left]);
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
    assert_eq!(walked.rests, vec![unknown]);
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

#[test]
fn an_unknown_unpack_schema_stays_unknown() {
    let mut w = World::new();
    let schema = w.db.unknown_schema();
    let supertype = w.db.apply_defaults(w.unpack_decl(), &[schema]).unwrap();
    let c = nominal(&mut w.db, "Dynamic", vec![], vec![supertype]);
    let rest = apply(&w.db, w.spread, &[schema]);
    let unknown = w.db.unknown();
    let key = w.key("name");
    let walked = w
        .walk(c, &pattern(&[false], &[(key, false)], true))
        .unwrap();
    assert!(walked.possible);
    assert_eq!(walked.slots, vec![unknown, unknown]);
    assert_eq!(walked.rests, vec![rest]);
    let split = PatternShape {
        positional: vec![],
        keyed: vec![],
        rests: vec![RestKind::Pos, RestKind::Key],
    };
    assert_eq!(w.walk(c, &split).unwrap().rests, vec![rest, rest]);
}

#[test]
fn a_closed_rest_carries_over() {
    let mut w = World::new();
    let int = w.int;
    let ints = vec![positional(Multiplicity::Repeated, int)];
    // `R: Unpack[{*Int}, Rest: R]`
    let r = w.rest_class(vec![], ints.clone(), |_, class| class);
    let walked = w.walk(r, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.rests, vec![r]);
}

#[test]
fn a_generic_rest_is_solved_again_from_the_tail() {
    let mut w = World::new();
    let (int, str, bool) = (w.int, w.str, w.bool);
    // `R[T]: Unpack[{*T}, Rest: R[T]]`
    let t = binder(Variance::Covariant);
    let ts = vec![positional(Multiplicity::Repeated, reference(&w.db, 0, 0))];
    let r = w.rest_class(vec![t], ts, |db, class| {
        apply(db, class, &[reference(db, 0, 0)])
    });
    let (a, b) = (reference(&w.db, 0, 0), reference(&w.db, 0, 1));
    let pair = w.rest_class(
        vec![binder(Variance::Covariant), binder(Variance::Covariant)],
        vec![
            positional(Multiplicity::Required, a),
            positional(Multiplicity::Required, b),
        ],
        |db, class| apply(db, class, &[a, b]),
    );
    let ints = apply(&w.db, r, &[int]);
    let walked = w.walk(ints, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.rests, vec![ints]);
    // `Pair[A, B]: Unpack[{A, B}, Rest: Pair[A, B]]` can't match a one-item tail
    let applied = apply(&w.db, pair, &[str, bool]);
    let walked = w.walk(applied, &pattern(&[false], &[], true)).unwrap();
    let left = w.spread_of(vec![positional(Multiplicity::Required, bool)]);
    assert_eq!(walked.rests, vec![left]);
}

#[test]
fn a_rest_outside_its_bound_falls_back() {
    let mut w = World::new();
    let int = w.int;
    let ints = vec![positional(Multiplicity::Repeated, int)];
    // `R: Unpack[{*Int}, Rest: Int]`
    let r = w.rest_class(vec![], ints.clone(), |_, _| int);
    let walked = w.walk(r, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.rests, vec![w.spread_of(ints)]);
}

#[test]
fn an_unpack_type_keeps_its_rest_only_for_its_own_tail() {
    let mut w = World::new();
    let (int, str) = (w.int, w.str);
    let ints = vec![positional(Multiplicity::Repeated, int)];
    let rest = w.class(ints.clone());
    let schema = items(&w.db, ints.clone());
    let homogeneous = apply(&w.db, w.unpack, &[schema, rest]);
    let walked = w.walk(homogeneous, &pattern(&[false], &[], true)).unwrap();
    assert_eq!(walked.rests, vec![rest]);
    let pair = vec![
        positional(Multiplicity::Required, int),
        positional(Multiplicity::Required, str),
    ];
    let schema = items(&w.db, pair);
    let fixed = apply(&w.db, w.unpack, &[schema, rest]);
    let walked = w.walk(fixed, &pattern(&[false], &[], true)).unwrap();
    let left = w.spread_of(vec![positional(Multiplicity::Required, str)]);
    assert_eq!(walked.rests, vec![left]);
}
