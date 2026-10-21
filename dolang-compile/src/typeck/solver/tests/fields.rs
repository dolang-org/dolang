use super::{
    members::{Class, key, private, special},
    *,
};
use crate::typeck::r#type::{FieldAccess, Scope};

fn sym(db: &Database, name: &str) -> TypeId {
    db.intern(Type::Literal(Literal::Sym(db.intern_symbol(name))))
}

/// `Sym`, which a schema's open tail is keyed by
fn sym_class(db: &mut Database) -> TypeId {
    let sym = nominal(db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    sym
}

fn fields(db: &Database, target: TypeId, access: FieldAccess) -> TypeId {
    db.intern(Type::Fields { target, access })
}

/// The schema a closed member schema stands for
fn listed(db: &Database, target: TypeId, access: FieldAccess) -> Result<TypeId, Issue> {
    let gas = Gas::default();
    Solver::new(db, &gas).fields(target, access)
}

fn required(db: &Database, name: &str, value: TypeId) -> SchemaItem {
    keyed(Multiplicity::Required, sym(db, name), value)
}

fn optional(db: &Database, name: &str, value: TypeId) -> SchemaItem {
    keyed(Multiplicity::Optional, sym(db, name), value)
}

fn tail(key: TypeId, value: TypeId) -> SchemaItem {
    keyed(Multiplicity::Repeated, key, value)
}

#[test]
fn members_list_public_instance_fields_and_properties() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    // final class Point: pub field x @ Int; field secret; a class field; a
    // method; a getter-only and a setter-only property; a special method
    let mut point = Class::new(&mut db, "Point", vec![]).final_class();
    point.field(key(&db, "x"), int, Scope::Instance);
    point.field(private(&db, "secret"), int, Scope::Instance);
    point.field(key(&db, "count"), int, Scope::Class);
    let method = point.function(&mut db, vec![], &[], int);
    point.method(key(&db, "norm"), method, Scope::Instance);
    let init = point.function(&mut db, vec![], &[], int);
    point.method(special(&db, "init"), init, Scope::Instance);
    let getter = point.function(&mut db, vec![], &[], str);
    point.property(key(&db, "label"), Some(getter), None);
    let nil = db.intern(Type::Literal(Literal::Nil));
    let setter = point.function(&mut db, vec![], &[str], nil);
    point.property(key(&db, "sink"), None, Some(setter));
    let point = point.finish(&mut db, vec![]);
    db.seal();

    let read = items(
        &db,
        vec![required(&db, "x", int), required(&db, "label", str)],
    );
    assert_eq!(listed(&db, point, FieldAccess::Read), Ok(read));
    let write = items(
        &db,
        vec![optional(&db, "x", int), optional(&db, "sink", str)],
    );
    assert_eq!(listed(&db, point, FieldAccess::Write), Ok(write));
}

#[test]
fn open_classes_admit_further_keys() {
    let mut db = Database::new();
    let int = int(&mut db);
    let symbol = sym_class(&mut db);
    let mut open = Class::new(&mut db, "Open", vec![]);
    open.field(key(&db, "x"), int, Scope::Instance);
    let open = open.finish(&mut db, vec![]);
    // A decorated member's value is unknown, so it opens a final class's too
    let mut decorated = Class::new(&mut db, "Decorated", vec![]).final_class();
    let method = decorated.function(&mut db, vec![], &[], int);
    decorated.decorated(key(&db, "wrapped"), method);
    let decorated = decorated.finish(&mut db, vec![]);
    db.seal();

    let (top, bottom) = (db.top(), db.bottom());
    let read = items(&db, vec![required(&db, "x", int), tail(symbol, top)]);
    assert_eq!(listed(&db, open, FieldAccess::Read), Ok(read));
    let write = items(&db, vec![optional(&db, "x", int), tail(symbol, bottom)]);
    assert_eq!(listed(&db, open, FieldAccess::Write), Ok(write));
    assert_eq!(
        listed(&db, decorated, FieldAccess::Read),
        Ok(items(&db, vec![tail(symbol, top)]))
    );
}

#[test]
fn members_are_found_in_mro_order_with_their_class_arguments() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // class Base[T]: pub field value @ T; pub field label @ Str
    let mut base = Class::new(&mut db, "Base", vec![binder(Variance::Invariant)]);
    base.field(key(&db, "value"), t, Scope::Instance);
    base.field(key(&db, "label"), str, Scope::Instance);
    let base = base.finish(&mut db, vec![]);
    let base_int = apply(&db, base, &[int]);
    // final class Sub: Base[Int], whose method `label` hides the field
    let mut sub = Class::new(&mut db, "Sub", vec![]).final_class();
    let method = sub.function(&mut db, vec![], &[], str);
    sub.method(key(&db, "label"), method, Scope::Instance);
    let sub = sub.finish(&mut db, vec![base_int]);
    db.seal();

    let read = items(&db, vec![required(&db, "value", int)]);
    assert_eq!(listed(&db, sub, FieldAccess::Read), Ok(read));
}

#[test]
fn unions_join_reads_and_meet_writes() {
    let mut db = Database::new();
    let symbol = sym_class(&mut db);
    let int = int(&mut db);
    let base = Class::new(&mut db, "Base", vec![]).finish(&mut db, vec![]);
    let sub = Class::new(&mut db, "Sub", vec![]).finish(&mut db, vec![base]);
    // final class A: pub field x @ Sub; pub field y @ Int
    let mut a = Class::new(&mut db, "A", vec![]).final_class();
    a.field(key(&db, "x"), sub, Scope::Instance);
    a.field(key(&db, "y"), int, Scope::Instance);
    let a = a.finish(&mut db, vec![]);
    // final class B: pub field x @ Base
    let mut b = Class::new(&mut db, "B", vec![]).final_class();
    b.field(key(&db, "x"), base, Scope::Instance);
    let b = b.finish(&mut db, vec![]);
    db.seal();

    let union = db.intern(Type::Union(
        vec![UnionMember::Type(a), UnionMember::Type(b)].into(),
    ));
    let read = items(&db, vec![required(&db, "x", base), optional(&db, "y", int)]);
    assert_eq!(listed(&db, union, FieldAccess::Read), Ok(read));
    // `y` isn't one both set, so it's admitted with no value fitting it
    let bottom = db.bottom();
    let write = items(&db, vec![optional(&db, "x", sub), tail(symbol, bottom)]);
    assert_eq!(listed(&db, union, FieldAccess::Write), Ok(write));
}

#[test]
fn abstract_targets_are_left_as_themselves() {
    let mut db = Database::new();
    let int = int(&mut db);
    let t = reference(&db, 0, 0);
    let decl = generic(&mut db, vec![binder(Variance::Invariant)], t);
    db.seal();

    let schema = items(&db, vec![required(&db, "x", int)]);
    let outcome = under(&db, decl, fields(&db, t, FieldAccess::Read), schema);
    assert_eq!(outcome.status, Status::Unresolved);
}

#[test]
fn member_schemas_relate_as_what_they_list() {
    let mut db = Database::new();
    let symbol = sym_class(&mut db);
    let int = int(&mut db);
    // class Base: pub field x @ Int; class Sub: Base, pub field y @ Int
    let mut base = Class::new(&mut db, "Base", vec![]);
    base.field(key(&db, "x"), int, Scope::Instance);
    let base = base.finish(&mut db, vec![]);
    let mut sub = Class::new(&mut db, "Sub", vec![]);
    sub.field(key(&db, "y"), int, Scope::Instance);
    let sub = sub.finish(&mut db, vec![base]);
    db.seal();

    let x = sym(&db, "x");
    let (read_base, read_sub) = (
        fields(&db, base, FieldAccess::Read),
        fields(&db, sub, FieldAccess::Read),
    );
    let (write_base, write_sub) = (
        fields(&db, base, FieldAccess::Write),
        fields(&db, sub, FieldAccess::Write),
    );
    let proven = |a, b| check(&db, a, b).status == Status::Proven;
    // A subclass reads more and sets the same
    assert!(proven(read_sub, read_base));
    assert!(!proven(read_base, read_sub));
    assert!(proven(write_base, write_sub));
    assert!(!proven(write_sub, write_base));
    // Items are selected from what's listed
    assert!(proven(selecting(&db, false, read_sub, x), int));
    assert!(proven(int, selecting(&db, true, write_sub, x)));
    let y = sym(&db, "y");
    assert!(!proven(int, selecting(&db, true, write_base, y)));
    // A tail admitting no value is no item a closed schema refuses
    let closed = items(&db, vec![optional(&db, "x", int)]);
    let bottom = db.bottom();
    let never = items(&db, vec![optional(&db, "x", int), tail(symbol, bottom)]);
    assert!(proven(never, closed));
    assert!(proven(write_base, closed));
}
