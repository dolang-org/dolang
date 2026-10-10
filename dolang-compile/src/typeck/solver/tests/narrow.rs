use super::*;
use crate::typeck::{cfg::Relation, solver::narrow::Target};

fn union(db: &Database, types: &[TypeId]) -> TypeId {
    db.intern(Type::Union(
        types.iter().copied().map(UnionMember::Type).collect(),
    ))
}

fn decl(db: &Database, ty: TypeId) -> DeclId {
    let Type::Decl(id) = *db.ty(ty) else {
        panic!("not a declaration")
    };
    id
}

fn lit(db: &Database, literal: Literal) -> TypeId {
    db.intern(Type::Literal(literal))
}

fn intrinsic(db: &mut Database, name: &str, intrinsic: Intrinsic) -> TypeId {
    let ty = nominal(db, name, vec![], vec![]);
    db.set_intrinsic(intrinsic, ty);
    ty
}

/// A world of classes and literals
struct World {
    db: Database,
    nil: TypeId,
    bool: TypeId,
    int: TypeId,
    str: TypeId,
    base: TypeId,
    sub: TypeId,
    subsub: TypeId,
    other: TypeId,
    /// `Int`'s subclass
    my_int: TypeId,
    /// A class whose supertype isn't nominal, so its ancestors are unknown
    odd: TypeId,
}

impl World {
    fn new() -> Self {
        let mut db = Database::new();
        let nil = intrinsic(&mut db, "Nil", Intrinsic::Nil);
        let bool = intrinsic(&mut db, "Bool", Intrinsic::Bool);
        let int = intrinsic(&mut db, "Int", Intrinsic::Int);
        let str = intrinsic(&mut db, "Str", Intrinsic::Str);
        let base = nominal(&mut db, "Base", vec![], vec![]);
        let sub = nominal(&mut db, "Sub", vec![], vec![base]);
        let subsub = nominal(&mut db, "SubSub", vec![], vec![sub]);
        let other = nominal(&mut db, "Other", vec![], vec![]);
        let my_int = nominal(&mut db, "MyInt", vec![], vec![int]);
        let func = function(&db, &[], int);
        let odd = nominal(&mut db, "Odd", vec![], vec![func]);
        Self {
            db,
            nil,
            bool,
            int,
            str,
            base,
            sub,
            subsub,
            other,
            my_int,
            odd,
        }
    }

    fn int_lit(&self, n: i128) -> TypeId {
        literal(&self.db, n)
    }

    fn nil_lit(&self) -> TypeId {
        lit(&self.db, Literal::Nil)
    }

    fn bool_lit(&self, value: bool) -> TypeId {
        lit(&self.db, Literal::Bool(value))
    }
}

fn class(s: &Solver<'_>, ty: TypeId, relation: Relation, negated: bool, target: TypeId) -> TypeId {
    s.narrow(ty, relation, negated, Target::Class(decl(s.db, target)))
}

fn value(s: &Solver<'_>, ty: TypeId, negated: bool, target: TypeId) -> TypeId {
    s.narrow(ty, Relation::Exact, negated, Target::Literal(target))
}

#[test]
fn generic_union_alias_preserves_narrowed_arguments() {
    let mut db = Database::new();
    let int = intrinsic(&mut db, "Int", Intrinsic::Int);
    let nil = intrinsic(&mut db, "Nil", Intrinsic::Nil);
    let wrapper = nominal(
        &mut db,
        "Wrapper",
        vec![binder(Variance::Covariant)],
        vec![],
    );
    let wrapped = apply(&db, wrapper, &[reference(&db, 0, 0)]);
    let body = union(&db, &[nil, wrapped]);
    let body = quantified(&db, vec![binder(Variance::Covariant)], body);
    let segment = alias(&mut db, "Segment", body);
    let segment = apply(&db, segment, &[int]);
    let expected = apply(&db, wrapper, &[int]);
    db.seal();
    let s = Solver::new(&db);
    assert_eq!(
        class(&s, segment, Relation::Upper, false, wrapper),
        expected
    );
    assert_eq!(class(&s, segment, Relation::Upper, true, wrapper), nil);
}

#[test]
fn upper_bounds_keep_reaching_members() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let bottom = w.db.bottom();
    let upper = |ty| class(&s, ty, Relation::Upper, false, w.sub);

    assert_eq!(upper(w.sub), w.sub);
    assert_eq!(upper(w.subsub), w.subsub);
    // A supertype, an unrelated class, `Value` and `Unknown` become `C`
    assert_eq!(upper(w.base), w.sub);
    assert_eq!(upper(w.other), w.sub);
    assert_eq!(upper(w.db.top()), w.sub);
    assert_eq!(upper(w.db.unknown()), w.sub);
    // Unproven reach becomes `C` too
    assert_eq!(upper(w.odd), w.sub);
    // Literals of another class, and `Nil` and `Bool`, whose values are all
    // literals, are disjoint
    assert_eq!(upper(w.int_lit(1)), bottom);
    assert_eq!(upper(w.nil_lit()), bottom);
    assert_eq!(upper(w.nil), bottom);
    assert_eq!(upper(w.bool), bottom);
    // Another intrinsic's subclass may be a `C`
    assert_eq!(upper(w.int), w.sub);
    assert_eq!(upper(w.str), w.sub);
    // An intrinsic above `C` isn't disjoint from it
    assert_eq!(class(&s, w.int, Relation::Upper, false, w.my_int), w.my_int);
    assert_eq!(
        class(&s, w.int_lit(1), Relation::Upper, false, w.int),
        w.int_lit(1)
    );

    let mixed = union(&w.db, &[w.subsub, w.other, w.int, w.nil_lit()]);
    assert_eq!(upper(mixed), w.sub);
    let mixed = union(&w.db, &[w.subsub, w.nil, w.nil_lit()]);
    assert_eq!(upper(mixed), w.subsub);
    assert_eq!(upper(union(&w.db, &[w.nil, w.bool])), bottom);
}

#[test]
fn negated_upper_bounds_drop_reaching_members() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let bottom = w.db.bottom();
    let not = |ty, target| class(&s, ty, Relation::Upper, true, target);

    assert_eq!(not(w.sub, w.sub), bottom);
    assert_eq!(not(w.subsub, w.sub), bottom);
    // Not every value of a supertype is a `C`
    assert_eq!(not(w.base, w.sub), w.base);
    assert_eq!(not(w.other, w.sub), w.other);
    assert_eq!(not(w.odd, w.sub), w.odd);
    assert_eq!(not(w.db.top(), w.sub), w.db.top());
    assert_eq!(not(w.db.unknown(), w.sub), w.db.unknown());
    assert_eq!(not(w.int_lit(1), w.int), bottom);
    assert_eq!(not(w.int_lit(1), w.sub), w.int_lit(1));

    let mixed = union(&w.db, &[w.subsub, w.other, w.int_lit(1)]);
    assert_eq!(not(mixed, w.sub), union(&w.db, &[w.other, w.int_lit(1)]));
}

#[test]
fn exact_bounds_keep_only_the_class() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let bottom = w.db.bottom();
    let exact = |ty, target| class(&s, ty, Relation::Exact, false, target);

    assert_eq!(exact(w.sub, w.sub), w.sub);
    // A supertype, `Value` and `Unknown` become `C`
    assert_eq!(exact(w.base, w.sub), w.sub);
    assert_eq!(exact(w.db.top(), w.sub), w.sub);
    assert_eq!(exact(w.db.unknown(), w.sub), w.sub);
    // A subclass's or an unrelated class's instances don't have class `C`
    assert_eq!(exact(w.subsub, w.sub), bottom);
    assert_eq!(exact(w.other, w.sub), bottom);
    assert_eq!(exact(w.odd, w.sub), bottom);
    // Unless whether `C` reaches the member can't be proven
    assert_eq!(exact(w.base, w.odd), w.odd);
    // A literal's class is exactly its intrinsic
    assert_eq!(exact(w.int_lit(1), w.int), w.int_lit(1));
    assert_eq!(exact(w.int_lit(1), w.my_int), bottom);
    assert_eq!(exact(w.int, w.my_int), w.my_int);

    let mixed = union(&w.db, &[w.base, w.subsub, w.other]);
    assert_eq!(exact(mixed, w.sub), w.sub);
}

#[test]
fn negated_exact_bounds_drop_only_singletons() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let not = |ty, target| class(&s, ty, Relation::Exact, true, target);

    // A class may have subclasses
    assert_eq!(not(w.sub, w.sub), w.sub);
    assert_eq!(not(w.nil, w.nil), w.nil);
    assert_eq!(not(w.db.unknown(), w.sub), w.db.unknown());
    let mixed = union(&w.db, &[w.int_lit(1), w.str, w.sub]);
    assert_eq!(not(mixed, w.int), union(&w.db, &[w.str, w.sub]));
}

#[test]
fn literal_comparisons_strip_other_literals() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let bottom = w.db.bottom();
    let one = w.int_lit(1);
    let two = w.int_lit(2);
    let nil = w.nil_lit();

    // Only literals are known unequal; a class's `(==)` may equal anything
    let mixed = union(&w.db, &[one, two, nil, w.base]);
    assert_eq!(value(&s, mixed, false, one), union(&w.db, &[one, w.base]));
    // The join subsumes a literal under its class
    let mixed = union(&w.db, &[one, two, nil, w.int, w.base]);
    assert_eq!(value(&s, mixed, false, one), union(&w.db, &[w.int, w.base]));
    assert_eq!(
        value(&s, union(&w.db, &[w.base, nil]), false, nil),
        union(&w.db, &[w.base, nil])
    );
    assert_eq!(value(&s, w.bool, false, w.bool_lit(true)), w.bool);
    assert_eq!(value(&s, w.db.unknown(), false, one), w.db.unknown());
    assert_eq!(value(&s, w.db.top(), false, one), w.db.top());
    assert_eq!(value(&s, two, false, one), bottom);

    assert_eq!(
        value(&s, mixed, true, one),
        union(&w.db, &[nil, w.int, w.base])
    );
    assert_eq!(value(&s, union(&w.db, &[w.str, nil]), true, nil), w.str);
    assert_eq!(value(&s, union(&w.db, &[w.str, w.nil]), true, nil), w.str);
    assert_eq!(value(&s, one, true, one), bottom);
    assert_eq!(value(&s, w.db.unknown(), true, one), w.db.unknown());
}

#[test]
fn truthiness_narrows_nil_and_bool() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let truthy = |ty| {
        let ty = value(&s, ty, true, w.nil_lit());
        value(&s, ty, true, w.bool_lit(false))
    };

    assert_eq!(truthy(union(&w.db, &[w.str, w.nil])), w.str);
    assert_eq!(truthy(w.bool), w.bool_lit(true));
    assert_eq!(value(&s, w.bool, true, w.bool_lit(true)), w.bool_lit(false));
    assert_eq!(truthy(w.bool_lit(false)), w.db.bottom());
    assert_eq!(
        truthy(union(&w.db, &[w.base, w.bool, w.nil_lit()])),
        union(&w.db, &[w.base, w.bool_lit(true)])
    );
}

#[test]
fn generic_members_carry_arguments_down() {
    let mut db = Database::new();
    let int = int(&mut db);
    let t = reference(&db, 0, 0);
    let seq = nominal(&mut db, "Seq", vec![binder(Variance::Covariant)], vec![]);
    let seq_t = apply(&db, seq, &[t]);
    let array = nominal(
        &mut db,
        "Array",
        vec![binder(Variance::Covariant)],
        vec![seq_t],
    );
    let vec = nominal(
        &mut db,
        "Vec",
        vec![binder(Variance::Invariant)],
        vec![seq_t],
    );
    let cell = nominal(&mut db, "Cell", vec![binder(Variance::Invariant)], vec![]);
    let cell_t = apply(&db, cell, &[t]);
    let lazy = nominal(
        &mut db,
        "Lazy",
        vec![binder(Variance::Covariant)],
        vec![cell_t],
    );
    let other = nominal(&mut db, "Other", vec![], vec![]);
    db.seal();
    let mut s = Solver::new(&db);
    s.gradual();
    let unknown = db.unknown();
    let upper = |ty, target| class(&s, ty, Relation::Upper, false, target);

    let seq_int = apply(&db, seq, &[int]);
    let array_int = apply(&db, array, &[int]);
    assert_eq!(upper(seq_int, array), array_int);
    assert_eq!(class(&s, seq_int, Relation::Exact, false, array), array_int);
    // An invariant binder under a covariant one can't take its argument
    assert_eq!(upper(seq_int, vec), apply(&db, vec, &[unknown]));
    // An invariant argument carries into any binder
    let cell_int = apply(&db, cell, &[int]);
    assert_eq!(upper(cell_int, lazy), apply(&db, lazy, &[int]));
    // A member already reaching `C` is kept whole
    assert_eq!(upper(array_int, seq), array_int);
    assert_eq!(
        class(&s, array_int, Relation::Upper, true, seq),
        db.bottom()
    );
    // Nothing carries from an unrelated member
    assert_eq!(upper(other, array), apply(&db, array, &[unknown]));
    assert_eq!(upper(db.top(), array), apply(&db, array, &[unknown]));

    // A strict unit's solver gives an uncarried covariant binder its bound, and
    // keeps a member it can't narrow without an invariant binder's argument
    let strict = Solver::new(&db);
    let upper = |ty, target| class(&strict, ty, Relation::Upper, false, target);
    let array_value = apply(&db, array, &[db.top()]);
    assert_eq!(upper(seq_int, array), array_int);
    assert_eq!(upper(seq_int, vec), seq_int);
    assert_eq!(upper(other, array), array_value);
    assert_eq!(upper(db.top(), array), array_value);
    assert_eq!(upper(db.top(), vec), db.top());
    assert_eq!(upper(unknown, array), apply(&db, array, &[unknown]));
}

#[test]
fn covariant_schema_binders_take_their_bound() {
    let mut db = Database::new();
    let int = int(&mut db);
    intrinsic(&mut db, "Sym", Intrinsic::Sym);
    let schema_binder = |variance, binding, bound| Binder {
        variance,
        ..bounded(Kind::Schema, binding, bound)
    };
    let closed = schema(&db, &[int]);
    let mut declare = |name, binder| nominal(&mut db, name, vec![binder], vec![]);
    let bounded_class = declare(
        "Bounded",
        schema_binder(Variance::Covariant, Binding::Positional, Some(closed)),
    );
    let open_class = declare(
        "Open",
        schema_binder(Variance::Covariant, Binding::Positional, None),
    );
    let keyed_class = declare(
        "Keyed",
        schema_binder(Variance::Covariant, Binding::Rest(Rest::Keyed), None),
    );
    let contra_class = declare(
        "Contra",
        schema_binder(Variance::Contravariant, Binding::Positional, None),
    );
    let invariant_class = declare(
        "Invariant",
        schema_binder(Variance::Invariant, Binding::Positional, Some(closed)),
    );
    db.seal();
    let s = Solver::new(&db);
    let top = db.top();
    let upper = |target| class(&s, top, Relation::Upper, false, target);

    assert_eq!(upper(bounded_class), apply(&db, bounded_class, &[closed]));
    let open = db.rest_shape(Rest::All);
    assert_eq!(upper(open_class), apply(&db, open_class, &[open]));
    let keyed = db.rest_shape(Rest::Keyed);
    assert_eq!(upper(keyed_class), apply(&db, keyed_class, &[keyed]));
    // There's no bottom schema, and an invariant binder has no sound argument
    assert_eq!(upper(contra_class), top);
    assert_eq!(upper(invariant_class), top);
}

#[test]
fn packs_are_kept() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let pack = w.db.intern(Type::Union(
        vec![
            UnionMember::Type(w.sub),
            UnionMember::Expand(w.db.unknown_schema()),
        ]
        .into(),
    ));
    assert_eq!(
        class(&s, pack, Relation::Upper, true, w.sub),
        w.db.intern(Type::Union(
            vec![UnionMember::Expand(w.db.unknown_schema())].into(),
        ))
    );
}

/// Every witness of the input that satisfies the relation is in the result. A
/// witness stands for a value: a literal, or an instance of exactly a class.
#[test]
fn narrowing_is_sound() {
    let mut w = World::new();
    w.db.seal();
    let s = Solver::new(&w.db);
    let db = &w.db;
    let literals = [
        w.int_lit(1),
        w.int_lit(2),
        w.nil_lit(),
        w.bool_lit(true),
        w.bool_lit(false),
        lit(db, Literal::Str("a".into())),
    ];
    // Classes with instances that aren't literals
    let instances = [w.base, w.sub, w.subsub, w.other, w.my_int, w.odd];
    let exact_class = |witness: TypeId| match db.ty(witness) {
        Type::Literal(literal) => db.intrinsic(literal.intrinsic()).unwrap(),
        _ => witness,
    };
    let below = |a, b| check(db, a, b).status == Status::Proven;
    let inputs = [
        union(db, &[w.base, w.other, w.int, w.str, w.nil, w.bool]),
        union(db, &[w.subsub, w.my_int, w.odd, literals[0], literals[2]]),
        union(db, &[w.bool, w.nil_lit(), w.int_lit(2)]),
        db.top(),
    ];
    let classes = [w.base, w.sub, w.int, w.my_int, w.nil, w.bool, w.odd];

    for input in inputs {
        let witnesses = literals
            .iter()
            .chain(&instances)
            .copied()
            .filter(|&witness| below(witness, input));
        for witness in witnesses {
            let exact = exact_class(witness);
            for target in classes {
                for (relation, holds) in [
                    (Relation::Upper, below(exact, target)),
                    (Relation::Exact, exact == target),
                ] {
                    for negated in [false, true] {
                        if holds == negated {
                            continue;
                        }
                        let result = class(&s, input, relation, negated, target);
                        assert!(
                            below(witness, result),
                            "{witness:?} lost from {input:?} by {relation:?} {negated} {target:?}"
                        );
                    }
                }
            }
            for &target in &literals {
                let is_literal = matches!(db.ty(witness), Type::Literal(_));
                for negated in [false, true] {
                    // A class instance's `(==)` may say anything
                    if is_literal && (witness == target) == negated {
                        continue;
                    }
                    let result = value(&s, input, negated, target);
                    assert!(
                        below(witness, result),
                        "{witness:?} lost from {input:?} by == {negated} {target:?}"
                    );
                }
            }
        }
    }
}
