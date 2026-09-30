use super::*;
use crate::typeck::{
    solver::member::{Found, FoundKind, Lookup},
    r#type::{Member, MemberKey, Scope},
};

/// A class whose members are added before it is populated
struct Class {
    id: DeclId,
    ty: TypeId,
    source: DeclSource,
    binders: Vec<Binder>,
    members: Vec<(MemberKey, Member)>,
}

impl Class {
    fn new(db: &mut Database, name: &str, binders: Vec<Binder>) -> Self {
        let (id, ty, source) = reserve(db, DeclKind::Class, name);
        Self {
            id,
            ty,
            source,
            binders,
            members: Vec::new(),
        }
    }

    /// The class applied to its own group, as a method's receiver is
    fn receiver(&self, db: &Database) -> TypeId {
        if self.binders.is_empty() {
            return self.ty;
        }
        let args: Vec<_> = (0..self.binders.len())
            .map(|slot| reference(db, 0, slot))
            .collect();
        apply(db, self.ty, &args)
    }

    fn field(&mut self, key: MemberKey, ty: TypeId, scope: Scope) {
        self.members.push((
            key,
            Member::Field {
                ty,
                scope,
                public: !key.private,
            },
        ));
    }

    /// A method's function, lifted over the class's binders and then its `own`. Its
    /// types refer to the class's binders first, and it takes the receiver first.
    fn function(
        &self,
        db: &mut Database,
        own: Vec<Binder>,
        params: &[TypeId],
        result: TypeId,
    ) -> DeclId {
        let (id, _, source) = reserve(db, DeclKind::Function, "method");
        let params: Vec<_> = [self.receiver(db)]
            .into_iter()
            .chain(params.iter().copied())
            .collect();
        let binders = self.binders.iter().cloned().chain(own).collect();
        let ty = quantified(db, binders, function(db, &params, result));
        populate(db, id, source, ty, vec![]);
        id
    }

    fn method(&mut self, key: MemberKey, decl: DeclId, scope: Scope) {
        self.members.push((
            key,
            Member::Method {
                decl,
                scope,
                public: !key.private,
            },
        ));
    }

    fn finish(self, db: &mut Database, supers: Vec<TypeId>) -> TypeId {
        let binders = (0..self.binders.len())
            .map(|_| BinderSource {
                name: self.source.name.unwrap(),
                span: self.source.span,
                bound: None,
                default: None,
                origin: BinderOrigin::Written,
            })
            .collect();
        let ty = quantified(db, self.binders, self.ty);
        db.populate(
            self.id,
            Declaration {
                source: self.source,
                ty,
                binders,
                supertypes: supers.into(),
                members: self.members.into(),
            },
        );
        self.ty
    }
}

fn key(db: &Database, name: &str) -> MemberKey {
    MemberKey {
        name: db.intern_symbol(name),
        special: false,
        private: false,
    }
}

fn private(db: &Database, name: &str) -> MemberKey {
    MemberKey {
        private: true,
        ..key(db, name)
    }
}

fn special(db: &Database, name: &str) -> MemberKey {
    MemberKey {
        special: true,
        ..key(db, name)
    }
}

fn found(lookup: Result<Lookup, Issue>) -> Found {
    match lookup {
        Ok(Lookup::Found(found)) => found,
        other => panic!("not found: {other:?}"),
    }
}

fn missing(lookup: Result<Lookup, Issue>) -> bool {
    matches!(lookup, Ok(Lookup::Missing))
}

/// Whether a term is proved equivalent to a type
fn same_type(s: &mut Solver<'_>, a: Term, b: TypeId) -> bool {
    let b = s.closed(b);
    s.constrain(a, b, Provenance::default());
    s.constrain(b, a, Provenance::default());
    s.solve()
        .iter()
        .all(|outcome| outcome.status == Status::Proven)
}

#[test]
fn members_are_found_in_mro_order_with_their_class_arguments() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    // class Base[T]: pub field value @ T; pub def get self -> T
    let mut base = Class::new(&mut db, "Base", vec![binder(Variance::Invariant)]);
    base.field(key(&db, "value"), t, Scope::Instance);
    let get = base.function(&mut db, vec![], &[], t);
    base.method(key(&db, "get"), get, Scope::Instance);
    let base = base.finish(&mut db, vec![]);
    // class Sub: Base[Int]
    let sub = Class::new(&mut db, "Sub", vec![]);
    let sub_id = sub.id;
    let base_int = apply(&db, base, &[int]);
    let sub = sub.finish(&mut db, vec![base_int]);
    // A diamond: Left and Right both declare `x`; Left's own ancestor comes first
    let mut top = Class::new(&mut db, "Top", vec![]);
    top.field(key(&db, "x"), int, Scope::Instance);
    let top = top.finish(&mut db, vec![]);
    let left = Class::new(&mut db, "Left", vec![]).finish(&mut db, vec![top]);
    let mut right = Class::new(&mut db, "Right", vec![]);
    right.field(key(&db, "x"), str, Scope::Instance);
    let right = right.finish(&mut db, vec![]);
    let diamond = Class::new(&mut db, "Diamond", vec![]).finish(&mut db, vec![left, right]);
    // An override in a subclass wins
    let mut over = Class::new(&mut db, "Over", vec![]);
    over.field(key(&db, "value"), int, Scope::Instance);
    let over_id = over.id;
    let over = over.finish(&mut db, vec![base_int]);
    db.seal();

    let mut s = Solver::new(&db);
    let value = found(s.member(s.closed(sub), key(&db, "value")));
    assert!(value.public);
    let FoundKind::Field(ty) = value.kind else {
        panic!("a field")
    };
    assert!(same_type(&mut s, ty, int));

    // The method is applied to `Int` and still takes its receiver
    let get = found(s.member(s.closed(sub), key(&db, "get")));
    let FoundKind::Method(signatures) = get.kind else {
        panic!("a method")
    };
    let (true, Some(signature)) = (signatures.overloads.is_empty(), signatures.implementation)
    else {
        panic!("one signature")
    };
    let expected = function(&db, &[base_int], int);
    assert!(same_type(&mut s, signature, expected));
    let r = s.infer();
    let call = s.call(&[CallArgument::Positional(s.closed(sub))], r, None, None);
    s.constrain(signature, call, Provenance::default());
    assert!(
        s.solve()
            .iter()
            .all(|outcome| outcome.status != Status::Contradicted)
    );
    let Term::Infer(r) = r else { unreachable!() };
    assert_eq!(s.default(r), Ok(int));

    let x = found(s.member(s.closed(diamond), key(&db, "x")));
    let FoundKind::Field(ty) = x.kind else {
        panic!("a field")
    };
    assert!(same_type(&mut s, ty, int));
    assert_ne!(x.class, sub_id);

    assert_eq!(
        found(s.member(s.closed(over), key(&db, "value"))).class,
        over_id
    );
    assert!(missing(s.member(s.closed(sub), key(&db, "absent"))));
}

#[test]
fn private_members_are_their_class_own() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    // class Config: field port @ Int; #[getter] pub def port self -> Str
    let mut config = Class::new(&mut db, "Config", vec![]);
    let config_id = config.id;
    config.field(private(&db, "port"), int, Scope::Instance);
    let getter = config.function(&mut db, vec![], &[], str);
    config.members.push((
        key(&db, "port"),
        Member::Property {
            getter: Some(getter),
            setter: None,
            scope: Scope::Instance,
            public: true,
        },
    ));
    let config = config.finish(&mut db, vec![]);
    let sub = Class::new(&mut db, "Sub", vec![]).finish(&mut db, vec![config]);
    let other = nominal(&mut db, "Other", vec![], vec![]);
    db.seal();

    let mut s = Solver::new(&db);
    let port = found(s.member(s.closed(sub), key(&db, "port")));
    let FoundKind::Property {
        getter: Some(getter),
        setter: None,
    } = port.kind
    else {
        panic!("a getter")
    };
    let expected = function(&db, &[config], str);
    assert!(same_type(
        &mut s,
        getter.implementation.expect("a getter"),
        expected
    ));

    let field = found(s.private_member(s.closed(sub), config_id, private(&db, "port")));
    assert!(!field.public);
    let FoundKind::Field(ty) = field.kind else {
        panic!("a field")
    };
    assert!(same_type(&mut s, ty, int));
    assert!(missing(s.private_member(
        s.closed(other),
        config_id,
        private(&db, "port")
    )));
}

#[test]
fn class_objects_have_class_and_static_members_and_unbound_methods() {
    let mut db = Database::new();
    let int = int(&mut db);
    let ty = nominal(&mut db, "Type", vec![binder(Variance::Covariant)], vec![]);
    db.set_intrinsic(Intrinsic::Type, ty);
    let mut base = Class::new(&mut db, "Base", vec![]);
    base.field(key(&db, "count"), int, Scope::Class);
    base.field(key(&db, "made"), int, Scope::Static);
    base.field(key(&db, "size"), int, Scope::Instance);
    let run = base.function(&mut db, vec![], &[], int);
    base.method(key(&db, "run"), run, Scope::Instance);
    let base = base.finish(&mut db, vec![]);
    let sub = Class::new(&mut db, "Sub", vec![]).finish(&mut db, vec![base]);
    db.seal();

    let s = Solver::new(&db);
    let object = |class| s.closed(apply(&db, ty, &[class]));
    assert_eq!(
        found(s.member(object(sub), key(&db, "count"))).scope,
        Scope::Class
    );
    // A static member isn't inherited
    assert!(missing(s.member(object(sub), key(&db, "made"))));
    assert_eq!(
        found(s.member(object(base), key(&db, "made"))).scope,
        Scope::Static
    );
    // An instance method is reached unbound, but an instance field is not
    let run = found(s.member(object(sub), key(&db, "run")));
    assert!(matches!(run.kind, FoundKind::Method(_)));
    assert!(missing(s.member(object(sub), key(&db, "size"))));
    // Instances don't see the type object's members
    assert!(missing(s.member(s.closed(sub), key(&db, "count"))));
}

#[test]
fn a_missing_member_falls_back_to_get_and_set() {
    let mut db = Database::new();
    let int = int(&mut db);
    let mut dynamic = Class::new(&mut db, "Dynamic", vec![]);
    let get = dynamic.function(&mut db, vec![], &[int], int);
    dynamic.method(special(&db, "get"), get, Scope::Instance);
    dynamic.field(key(&db, "real"), int, Scope::Instance);
    let dynamic = dynamic.finish(&mut db, vec![]);
    db.seal();

    let s = Solver::new(&db);
    assert!(matches!(
        s.member(s.closed(dynamic), key(&db, "real")),
        Ok(Lookup::Found(_))
    ));
    assert!(matches!(
        s.member(s.closed(dynamic), key(&db, "anything")),
        Ok(Lookup::Fallback {
            get: Some(_),
            set: None
        })
    ));
    // A special method has no fallback
    assert!(missing(s.member(s.closed(dynamic), special(&db, "iter"))));
}

#[test]
fn receivers_are_walked_to_a_class_or_left_undecided() {
    let mut db = Database::new();
    // A literal's members are its backing class's
    let mut int = Class::new(&mut db, "Int", vec![]);
    int.field(key(&db, "abs"), int.ty, Scope::Instance);
    let int = int.finish(&mut db, vec![]);
    db.set_intrinsic(Intrinsic::Int, int);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let mut shape = Class::new(&mut db, "Shape", vec![]);
    shape.field(key(&db, "area"), int, Scope::Instance);
    let shape = shape.finish(&mut db, vec![]);
    let one = literal(&db, 1);
    let unknown = db.unknown();
    let dynamic = Class::new(&mut db, "Dynamic", vec![]).finish(&mut db, vec![unknown]);
    let union = db.intern(Type::Union(
        vec![UnionMember::Type(shape), UnionMember::Type(str)].into(),
    ));
    // f[T @ Shape] x @ T
    let t = reference(&db, 0, 0);
    let body = function(&db, &[t], t);
    let f = generic(
        &mut db,
        vec![bounded(Kind::Type, Binding::Positional, Some(shape))],
        body,
    );
    db.seal();

    let mut s = Solver::new(&db);
    let area = key(&db, "area");
    let environment = s.rigid_environment(f);
    let rigid = s.view(t, environment);
    assert!(matches!(s.member(rigid, area), Ok(Lookup::Found(_))));
    let var = s.infer();
    assert_eq!(
        s.member(var, area).err(),
        Some(Issue::Residual(Residual::Inference))
    );
    assert!(matches!(
        s.member(s.closed(db.unknown()), area),
        Ok(Lookup::Dynamic)
    ));
    assert!(matches!(
        s.member(s.closed(dynamic), area),
        Ok(Lookup::Dynamic)
    ));
    assert!(missing(s.member(s.closed(db.top()), area)));
    assert_eq!(
        s.member(s.closed(union), area).err(),
        Some(Issue::Residual(Residual::Unsupported(
            "a member of a union receiver"
        )))
    );
    assert!(matches!(
        s.member(s.closed(one), key(&db, "abs")),
        Ok(Lookup::Found(_))
    ));
    assert!(missing(s.member(s.closed(one), area)));
}

#[test]
fn every_signature_of_an_overloaded_method_is_applied() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let t = reference(&db, 0, 0);
    let mut boxed = Class::new(&mut db, "Box", vec![binder(Variance::Invariant)]);
    let pick = boxed.function(&mut db, vec![], &[int], t);
    let overload = boxed.function(&mut db, vec![], &[str], str);
    db.set_overloads(pick, vec![overload]);
    boxed.method(key(&db, "pick"), pick, Scope::Instance);
    // map[U] self f @ (T -> U) -> U
    let u = reference(&db, 0, 1);
    let mapper = function(&db, &[t], u);
    let map = boxed.function(&mut db, vec![binder(Variance::Invariant)], &[mapper], u);
    boxed.method(key(&db, "map"), map, Scope::Instance);
    let boxed = boxed.finish(&mut db, vec![]);
    let box_int = apply(&db, boxed, &[int]);
    let show = function(&db, &[int], str);
    db.seal();

    let mut s = Solver::new(&db);
    let pick = found(s.member(s.closed(box_int), key(&db, "pick")));
    let FoundKind::Method(signatures) = pick.kind else {
        panic!("a method")
    };
    let [overload] = signatures.overloads[..] else {
        panic!("one overload")
    };
    let implementation = signatures.implementation.expect("an implementation");
    let expected = function(&db, &[box_int, int], int);
    assert!(same_type(&mut s, implementation, expected));
    let expected = function(&db, &[box_int, str], str);
    assert!(same_type(&mut s, overload, expected));

    // The method's own binder is still instantiated at the call
    let map = found(s.member(s.closed(box_int), key(&db, "map")));
    let FoundKind::Method(signatures) = map.kind else {
        panic!("a method")
    };
    let r = s.infer();
    let args = [
        CallArgument::Positional(s.closed(box_int)),
        CallArgument::Positional(s.closed(show)),
    ];
    let call = s.call(&args, r, None, None);
    let map = signatures.implementation.expect("an implementation");
    s.constrain(map, call, Provenance::default());
    s.solve();
    let outcomes = default_all(&mut s);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.status == Status::Proven)
    );
    let Term::Infer(r) = r else { unreachable!() };
    assert_eq!(s.solution(r), Some(str));
}
