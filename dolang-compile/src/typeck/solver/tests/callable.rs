//! Callable values below function types

use super::{
    members::{Class, special},
    *,
};
use crate::typeck::r#type::Scope;

/// Bare `Func`, registered as the intrinsic
fn bare_func(db: &mut Database) -> TypeId {
    let func = nominal(db, "Func", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Func, func);
    func
}

/// `Func[S, R, :In, :Out]`, whose `(call)` is `(self, ...S <In >Out) -> R`,
/// registered as the intrinsic
fn generic_func(db: &mut Database) -> TypeId {
    let mut params = binder(Variance::Contravariant);
    params.kind = Kind::Schema;
    let channel = |db: &Database, name| Binder {
        binding: Binding::Keyword(db.intern_symbol(name)),
        ..binder(Variance::Contravariant)
    };
    let binders = vec![
        params,
        binder(Variance::Covariant),
        channel(db, "In"),
        channel(db, "Out"),
    ];
    let mut func = Class::new(db, "Func", binders.clone());
    let (id, _, source) = reserve(db, DeclKind::Function, "call");
    let s = db.intern(Type::Bound {
        reference: BoundRef::new(0, 0),
        kind: Kind::Schema,
    });
    let call = db.intern(Type::Function(Function {
        params: items(
            db,
            vec![
                positional(Multiplicity::Required, func.receiver(db)),
                include(Multiplicity::Required, s),
            ],
        ),
        result: reference(db, 0, 1),
        input: Some(reference(db, 0, 2)),
        output: Some(reference(db, 0, 3)),
    }));
    let ty = quantified(db, binders, call);
    populate(db, id, source, ty, vec![]);
    func.method(special(db, "call"), id, Scope::Instance);
    let func = func.finish(db, vec![]);
    db.set_intrinsic(Intrinsic::Func, func);
    func
}

/// `Type[T]`, registered as the intrinsic
fn class_type(db: &mut Database) -> TypeId {
    let ty = nominal(db, "Type", vec![binder(Variance::Covariant)], vec![]);
    db.set_intrinsic(Intrinsic::Type, ty);
    ty
}

/// A class `name` with `supers` whose instance `(call)` takes `params` and gives
/// `result`
fn calls(
    db: &mut Database,
    name: &str,
    params: &[TypeId],
    result: TypeId,
    supers: Vec<TypeId>,
) -> TypeId {
    let mut class = Class::new(db, name, vec![]);
    let call = class.function(db, vec![], params, result);
    class.method(special(db, "call"), call, Scope::Instance);
    class.finish(db, supers)
}

fn residual(outcome: &Outcome, what: &'static str) -> bool {
    outcome.status == Status::Unresolved && has(outcome, Residual::Unsupported(what).into())
}

const UNFIT: &str = "a callable none of whose signatures fits";

#[test]
fn an_instance_reaching_func_is_called_through_its_call() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let func = bare_func(&mut db);
    let adder = calls(&mut db, "Adder", &[int], str, vec![func]);
    let fits = function(&db, &[int], str);
    let wider = function(&db, &[int], db.top());
    let result = function(&db, &[int], int);
    let params = function(&db, &[str], str);
    db.seal();
    assert_eq!(check(&db, adder, fits).status, Status::Proven);
    assert_eq!(check(&db, adder, wider).status, Status::Proven);
    // A subclass may narrow the result or widen the parameters
    assert!(residual(&check(&db, adder, result), UNFIT));
    assert!(residual(&check(&db, adder, params), UNFIT));
}

#[test]
fn a_class_that_doesnt_reach_func_is_residual() {
    let mut db = Database::new();
    let int = int(&mut db);
    bare_func(&mut db);
    let plain = calls(&mut db, "Plain", &[int], int, vec![]);
    let without = Class::new(&mut db, "Without", vec![]).finish(&mut db, vec![]);
    let f = function(&db, &[int], int);
    db.seal();
    // A subclass may reach `Func`
    for class in [plain, without] {
        assert!(residual(
            &check(&db, class, f),
            "a class that doesn't reach `Func`"
        ));
    }
}

#[test]
fn func_arguments_describe_the_function() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let func = generic_func(&mut db);
    let unknown = db.unknown();
    let unknown_schema = db.unknown_schema();
    // `Handler: Func[{Int}, Str]` without a `(call)` of its own
    let described = apply(&db, func, &[schema(&db, &[int]), str, unknown, unknown]);
    let handler = Class::new(&mut db, "Handler", vec![]).finish(&mut db, vec![described]);
    // `Echo: Func` declares its own
    let bare = apply(&db, func, &[unknown_schema, unknown, unknown, unknown]);
    let echo = calls(&mut db, "Echo", &[str], str, vec![bare]);
    let [int_str, int_int, str_str] =
        [(int, str), (int, int), (str, str)].map(|(param, result)| function(&db, &[param], result));
    db.seal();
    assert_eq!(check(&db, handler, int_str).status, Status::Proven);
    assert!(residual(&check(&db, handler, int_int), UNFIT));
    assert_eq!(check(&db, echo, str_str).status, Status::Proven);
    assert!(residual(&check(&db, echo, int_str), UNFIT));
}

#[test]
fn a_generic_call_is_instantiated() {
    let mut db = Database::new();
    let int = int(&mut db);
    let func = bare_func(&mut db);
    // `(call)[T] self x @ T -> T`
    let mut identity = Class::new(&mut db, "Identity", vec![]);
    let t = reference(&db, 0, 0);
    let call = identity.function(&mut db, vec![binder(Variance::Invariant)], &[t], t);
    identity.method(special(&db, "call"), call, Scope::Instance);
    let identity = identity.finish(&mut db, vec![func]);
    db.seal();

    let mut s = Solver::new(&db);
    let r = s.infer();
    let expected = s.call(&[CallArgument::Positional(s.closed(int))], r, None, None);
    s.constrain(s.closed(identity), expected, Provenance::default());
    s.solve();
    let outcomes = default_all(&mut s);
    assert!(
        outcomes.iter().all(|o| o.status == Status::Proven),
        "{outcomes:?}"
    );
    assert_eq!(s.solution(variable_id(r)), Some(int));
}

#[test]
fn a_class_object_is_called_through_init() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let class = class_type(&mut db);
    bare_func(&mut db);
    // `Point` with `(init) self x @ Int`
    let mut point = Class::new(&mut db, "Point", vec![]);
    let top = db.top();
    let init = point.function(&mut db, vec![], &[int], top);
    point.method(special(&db, "init"), init, Scope::Instance);
    let point = point.finish(&mut db, vec![]);
    // `Empty` without one
    let empty = Class::new(&mut db, "Empty", vec![]).finish(&mut db, vec![]);
    let [point_object, empty_object] = [point, empty].map(|c| apply(&db, class, &[c]));
    let constructs = function(&db, &[int], point);
    let wrong = function(&db, &[str], point);
    let nullary = function(&db, &[], empty);
    db.seal();
    assert_eq!(check(&db, point_object, constructs).status, Status::Proven);
    assert!(residual(&check(&db, point_object, wrong), UNFIT));
    assert_eq!(check(&db, empty_object, nullary).status, Status::Proven);
}

#[test]
fn a_class_level_call_takes_precedence() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let class = class_type(&mut db);
    bare_func(&mut db);
    let mut factory = Class::new(&mut db, "Factory", vec![]);
    let call = factory.function(&mut db, vec![], &[int], str);
    factory.method(special(&db, "call"), call, Scope::Class);
    let top = db.top();
    let init = factory.function(&mut db, vec![], &[str], top);
    factory.method(special(&db, "init"), init, Scope::Instance);
    let factory = factory.finish(&mut db, vec![]);
    let object = apply(&db, class, &[factory]);
    let calls = function(&db, &[int], str);
    let constructs = function(&db, &[str], factory);
    db.seal();
    assert_eq!(check(&db, object, calls).status, Status::Proven);
    assert!(residual(&check(&db, object, constructs), UNFIT));
}

#[test]
fn a_generic_class_object_infers_its_arguments() {
    let mut db = Database::new();
    let int = int(&mut db);
    let class = class_type(&mut db);
    bare_func(&mut db);
    // `Box[T]` with `(init) self x @ T`
    let mut boxed = Class::new(&mut db, "Box", vec![binder(Variance::Invariant)]);
    let t = reference(&db, 0, 0);
    let top = db.top();
    let init = boxed.function(&mut db, vec![], &[t], top);
    boxed.method(special(&db, "init"), init, Scope::Instance);
    let boxed_id = boxed.id;
    let boxed = boxed.finish(&mut db, vec![]);
    // `[T] Type[Box[T]]`, as flow gives a generic class's object
    let object = quantified(
        &db,
        vec![binder(Variance::Invariant)],
        apply(&db, class, &[apply(&db, boxed, &[t])]),
    );
    let box_int = apply(&db, boxed, &[int]);
    let constructs = function(&db, &[int], box_int);
    db.seal();
    assert_eq!(check(&db, object, constructs).status, Status::Proven);

    let mut s = Solver::new(&db);
    let r = s.infer();
    let expected = s.call(&[CallArgument::Positional(s.closed(int))], r, None, None);
    s.constrain(s.closed(object), expected, Provenance::default());
    s.solve();
    let outcomes = default_all(&mut s);
    assert!(
        outcomes.iter().all(|o| o.status == Status::Proven),
        "{outcomes:?}"
    );
    let solution = s.solution(variable_id(r)).expect("a solution");
    assert_eq!(s.exposed_nominal(solution), Some((boxed_id, vec![int])));
}

#[test]
fn trials_choose_among_overloaded_calls() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let func = bare_func(&mut db);
    let mut pick = Class::new(&mut db, "Pick", vec![]);
    let top = db.top();
    let call = pick.function(&mut db, vec![], &[top], top);
    let on_int = pick.function(&mut db, vec![], &[int], int);
    let on_str = pick.function(&mut db, vec![], &[str], str);
    db.set_overloads(call, vec![on_int, on_str]);
    pick.method(special(&db, "call"), call, Scope::Instance);
    let pick = pick.finish(&mut db, vec![func]);
    let [int_int, str_str, int_str] =
        [(int, int), (str, str), (int, str)].map(|(param, result)| function(&db, &[param], result));
    db.seal();
    assert_eq!(check(&db, pick, int_int).status, Status::Proven);
    assert_eq!(check(&db, pick, str_str).status, Status::Proven);
    // The implementation isn't an alternative
    assert!(residual(&check(&db, pick, int_str), UNFIT));

    // Both fit a function of variables
    let mut s = Solver::new(&db);
    let (x, r) = (s.infer(), s.infer());
    let expected = s.call(&[CallArgument::Positional(x)], r, None, None);
    s.constrain(s.closed(pick), expected, Provenance::default());
    let outcome = s.solve().remove(0);
    assert_eq!(outcome.status, Status::Unresolved);
    assert!(has(&outcome, Residual::Ambiguous.into()));
}

#[test]
fn a_union_of_callables_is_below_what_each_is() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let func = bare_func(&mut db);
    let a = calls(&mut db, "A", &[int], str, vec![func]);
    let top = db.top();
    let b = calls(&mut db, "B", &[top], str, vec![func]);
    let either = db.intern(Type::Union(
        vec![UnionMember::Type(a), UnionMember::Type(b)].into(),
    ));
    let f = function(&db, &[int], str);
    db.seal();
    assert_eq!(check(&db, either, f).status, Status::Proven);
}

#[test]
fn a_chosen_signature_never_contradicts() {
    let mut db = Database::new();
    let int = int(&mut db);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let func = bare_func(&mut db);
    let adder = calls(&mut db, "Adder", &[int], str, vec![func]);
    db.seal();

    // The signature is chosen before its parameter is known
    let mut s = Solver::new(&db);
    let (x, r) = (s.infer(), s.infer());
    let expected = s.call(&[CallArgument::Positional(x)], r, None, None);
    s.constrain(s.closed(adder), expected, Provenance::default());
    s.solve();
    s.constrain(s.closed(str), x, Provenance::default());
    let outcome = s.solve().remove(0);
    assert!(residual(&outcome, UNFIT), "{outcome:?}");
}
