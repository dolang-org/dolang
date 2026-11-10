use super::{ESCAPED, Unquotable, quoted};
use crate::typeck::r#type::{
    Binder, Binding, BoundRef, Database, DeclId, Element, Function, Kind, Multiplicity, SchemaItem,
    Type, TypeId, Variance,
};

fn everything(_: DeclId) -> bool {
    true
}

fn quote(db: &Database, ty: TypeId) -> Option<Unquotable> {
    quoted(db, &everything, ty)
}

/// `[T] (T) -> T`
fn identity(db: &Database) -> TypeId {
    let t = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Type,
    });
    let params = db.intern(Type::Schema(
        vec![SchemaItem {
            multiplicity: Multiplicity::Required,
            element: Element::Positional(t),
        }]
        .into(),
    ));
    let body = db.intern(Type::Function(Function {
        params,
        result: t,
        input: None,
        output: None,
    }));
    db.intern(Type::Quantified {
        binders: vec![Binder {
            kind: Kind::Type,
            binding: Binding::Positional,
            bound: None,
            default: None,
            variance: Variance::Invariant,
        }]
        .into(),
        body,
    })
}

/// A function giving `ty`, which a type expression writes around it
fn nest(db: &Database, ty: TypeId) -> TypeId {
    db.intern(Type::Function(Function {
        params: db.intern(Type::Schema(Vec::new().into())),
        result: ty,
        input: None,
        output: None,
    }))
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn a_generic_function_is_quoted_at_the_top() {
    let db = Database::new();
    assert_eq!(quote(&db, identity(&db)), None);
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn a_generic_function_is_quoted_as_a_functions_result() {
    let db = Database::new();
    let nested = nest(&db, identity(&db));
    assert_eq!(quote(&db, nested), None);
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn a_class_object_is_not_quoted_inside_another_type() {
    let mut db = Database::new();
    let class = db.allocate();
    let meta = db.intern(Type::Meta(class));
    let nested = nest(&db, meta);
    assert_eq!(
        quote(&db, nested),
        Some(Unquotable::Form("a class object inside another type"))
    );
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn an_escaped_rigid_is_not_quoted() {
    let mut db = Database::new();
    let decl = db.allocate();
    let rigid = db.intern(Type::Rigid {
        decl,
        slot: 0,
        kind: Kind::Type,
    });
    assert_eq!(quote(&db, rigid), Some(ESCAPED));
    assert_eq!(quote(&db, nest(&db, rigid)), Some(ESCAPED));
}

#[test]
#[cfg_attr(miri, ignore = "type checker is safe code and too slow under Miri")]
fn unknown_is_quoted() {
    let db = Database::new();
    assert_eq!(quote(&db, db.unknown()), None);
}
