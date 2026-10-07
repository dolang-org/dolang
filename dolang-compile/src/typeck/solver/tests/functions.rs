use super::*;

/// Check a call of `callee` with `args`, expecting a result below `result`
fn call(db: &Database, callee: TypeId, args: &[CallArgument], result: TypeId) -> Outcome {
    let mut s = Solver::new(db);
    let expected = s.call(args, s.closed(result), None, None);
    s.constrain(s.closed(callee), expected, Provenance::default());
    s.solve().remove(0)
}

#[test]
fn fixed_function_variance_and_arity() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let sub = nominal(&mut db, "Sub", vec![], vec![base]);
    let broad_input = function(&db, &[base], sub);
    let narrow_input = function(&db, &[sub], base);
    let nullary = function(&db, &[], sub);
    db.seal();
    assert_eq!(check(&db, broad_input, narrow_input).status, Status::Proven);
    assert_eq!(
        check(&db, narrow_input, broad_input).status,
        Status::Contradicted
    );
    assert_eq!(
        check(&db, broad_input, nullary).status,
        Status::Contradicted
    );
}

#[test]
fn ambient_channels_are_related_by_variance() {
    let mut db = Database::new();
    let one = literal(&db, 1);
    let two = literal(&db, 2);
    let make = |input, output, result| {
        db.intern(Type::Function(Function {
            params: schema(&db, &[]),
            result,
            input,
            output,
        }))
    };
    let a = make(Some(one), Some(two), one);
    let b = make(Some(one), Some(two), db.top());
    let c = make(Some(two), Some(two), db.top());
    let d = make(None, Some(two), db.top());
    db.seal();
    assert_eq!(check(&db, a, b).status, Status::Proven);
    assert!(contradiction(
        &check(&db, a, c),
        Contradiction::DistinctLiterals
    ));
    // An omitted channel is gradual
    assert_eq!(check(&db, a, d).status, Status::Proven);
}

#[test]
fn functions_have_intrinsic_nominal_supertype() {
    let mut db = Database::new();
    let base = nominal(&mut db, "Base", vec![], vec![]);
    let func = nominal(&mut db, "Func", vec![], vec![base]);
    let unrelated = nominal(&mut db, "Other", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Func, func);
    let plain = function(&db, &[db.top()], db.top());
    let wrapped = alias(&mut db, "Callable", plain);
    let r = reference(&db, 0, 0);
    let generic = quantified(
        &db,
        vec![binder(Variance::Covariant)],
        function(&db, &[r], r),
    );
    db.seal();
    for ty in [plain, wrapped, generic] {
        assert_eq!(check(&db, ty, func).status, Status::Proven);
        assert_eq!(check(&db, ty, base).status, Status::Proven);
        assert_eq!(check(&db, ty, unrelated).status, Status::Contradicted);
    }
    // A nominal Func carries no signature from which to prove this direction.
    assert_eq!(check(&db, func, plain).status, Status::Unresolved);
}

#[test]
fn missing_func_intrinsic_is_residual() {
    let mut db = Database::new();
    let nominal = nominal(&mut db, "Func", vec![], vec![]);
    let function = function(&db, &[], db.top());
    db.seal();
    let result = check(&db, function, nominal);
    assert_eq!(result.status, Status::Unresolved);
    assert!(has(
        &result,
        Residual::MissingIntrinsic(Intrinsic::Func).into()
    ));
}

#[test]
fn omitted_channels_are_used_through_their_default_bounds() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let sink = nominal(
        &mut db,
        "Sink",
        vec![binder(Variance::Contravariant)],
        vec![],
    );
    let [iter_unknown, sink_unknown] = [iter, sink].map(|c| apply(&db, c, &[db.unknown()]));
    let [iter_int, sink_int] = [iter, sink].map(|c| apply(&db, c, &[int]));
    let [input, output] = [0, 1].map(|slot| reference(&db, 0, slot));
    let body = db.intern(Type::Function(Function {
        params: schema(&db, &[]),
        result: db.top(),
        input: Some(input),
        output: Some(output),
    }));
    let f = generic(
        &mut db,
        vec![
            bounded(Kind::Type, Binding::Implicit, Some(iter_unknown)),
            bounded(Kind::Type, Binding::Implicit, Some(sink_unknown)),
        ],
        body,
    );
    db.seal();
    let relate = |a, b| {
        let mut s = Solver::new(&db);
        let env = s.rigid_environment(f);
        s.constrain(s.view(a, env), s.view(b, env), Provenance::default());
        let status = s.solve()[0].status;
        let root = s.obligation(s.roots[0].obligation);
        let implicit = root
            .active
            .borrow()
            .iter()
            .any(|(_, step)| *step == Step::ImplicitBound);
        (status, implicit)
    };
    // Forwarding a channel is identity, so it never consults the bound
    for channel in [input, output] {
        assert_eq!(relate(channel, channel), (Status::Proven, false));
    }
    // Using its elements goes through the bound, labeled for strictness
    assert_eq!(relate(input, iter_int), (Status::Proven, true));
    assert_eq!(relate(output, sink_int), (Status::Proven, true));
    assert_eq!(relate(input, sink_int).0, Status::Contradicted);
}

#[test]
fn calls_bind_arguments_as_the_runtime_does() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let sym = nominal(&mut db, "Sym", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Sym, sym);
    let nil = nominal(&mut db, "Nil", vec![], vec![]);
    db.set_intrinsic(Intrinsic::Nil, nil);
    let [k, opt, z] = ["k", "opt", "z"].map(|name| db.intern_symbol(name));
    let key = |db: &Database, name| db.intern(Type::Literal(Literal::Sym(name)));
    // (Int, ?Int, *Str, k: Int, ?opt: Int, **Int) -> Str
    let params = items(
        &db,
        vec![
            positional(Req, int),
            positional(Opt, int),
            positional(Rep, str),
            keyed(Req, key(&db, k), int),
            keyed(Opt, key(&db, opt), int),
            keyed(Rep, sym, int),
        ],
    );
    let f = db.intern(Type::Function(Function {
        params,
        result: str,
        input: None,
        output: None,
    }));
    // (Int, Int) -> Str
    let pair = function(&db, &[int, int], str);
    let two_ints = schema(&db, &[int, int]);
    let ints = items(&db, vec![positional(Rep, int)]);
    let nil_value = db.intern(Type::Literal(Literal::Nil));
    let int_type = int;
    db.seal();
    let s = Solver::new(&db);
    let [int, str, nil_value] = [int, str, nil_value].map(|ty| s.closed(ty));
    use CallArgument::{Keyword, Positional, Spread};
    for args in [
        vec![Positional(int), Keyword(k, int)],
        vec![Keyword(k, int), Positional(int), Positional(int)],
        vec![
            Positional(int),
            Positional(int),
            Positional(str),
            Keyword(k, int),
        ],
        vec![
            Positional(int),
            Keyword(k, int),
            Keyword(opt, int),
            Keyword(z, int),
        ],
        vec![Keyword(k, int), Spread(s.closed(two_ints))],
        vec![Spread(s.closed(db.unknown_schema()))],
    ] {
        assert_eq!(
            call(&db, f, &args, db.top()).status,
            Status::Proven,
            "{args:?}"
        );
    }
    let fails = |args: &[CallArgument], contradiction: Contradiction| {
        let outcome = call(&db, f, args, db.top());
        assert!(
            self::contradiction(&outcome, contradiction),
            "{args:?}: {outcome:?}"
        );
    };
    fails(&[Positional(int)], Contradiction::Missing(3));
    fails(&[Keyword(k, int)], Contradiction::Missing(0));
    // The second positional argument is the optional parameter's, even as `nil`
    fails(
        &[Positional(int), Positional(str), Keyword(k, int)],
        Contradiction::UnrelatedNominals,
    );
    fails(
        &[Positional(int), Positional(nil_value), Keyword(k, int)],
        Contradiction::UnrelatedNominals,
    );
    // A repeated keyword goes to the keyed rest (the runtime's #832)
    assert_eq!(
        call(
            &db,
            f,
            &[Positional(int), Keyword(k, int), Keyword(k, int)],
            db.top()
        )
        .status,
        Status::Proven
    );
    fails(
        &[Positional(int), Keyword(k, int), Keyword(k, str)],
        Contradiction::UnrelatedNominals,
    );
    fails(
        &[Positional(int), Keyword(k, int), Keyword(z, str)],
        Contradiction::UnrelatedNominals,
    );
    fails(
        &[Positional(int), Keyword(k, str)],
        Contradiction::UnrelatedNominals,
    );
    let fixed = |args: &[CallArgument], contradiction: Contradiction| {
        let outcome = call(&db, pair, args, db.top());
        assert!(
            self::contradiction(&outcome, contradiction),
            "{args:?}: {outcome:?}"
        );
    };
    fixed(
        &[Positional(int), Positional(int), Positional(int)],
        Contradiction::Excess(2),
    );
    fixed(
        &[Positional(int), Positional(int), Keyword(k, int)],
        Contradiction::Excess(2),
    );
    // An array spread may hold too few items
    fixed(&[Spread(s.closed(ints))], Contradiction::Missing(0));
    // The result is below what the call expects
    assert!(contradiction(
        &call(&db, pair, &[Positional(int), Positional(int)], int_type),
        Contradiction::UnrelatedNominals
    ));
}

#[test]
fn a_call_result_variable_is_bounded_by_the_callee_result() {
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let str = nominal(&mut db, "Str", vec![], vec![]);
    let f = function(&db, &[int], str);
    db.seal();
    let mut s = Solver::new(&db);
    let result = s.infer();
    let expected = s.call(
        &[CallArgument::Positional(s.closed(int))],
        result,
        None,
        None,
    );
    s.constrain(s.closed(f), expected, Provenance::default());
    assert_eq!(s.solve()[0].status, Status::Unresolved);
    let lower: Vec<_> = s.bounds(variable_id(result)).lower().collect();
    assert_eq!(lower, vec![s.closed(str)]);
}

#[test]
fn parameter_lists_are_related_contravariantly() {
    use Multiplicity::{Optional as Opt, Repeated as Rep, Required as Req};
    let mut db = Database::new();
    let int = nominal(&mut db, "Int", vec![], vec![]);
    let [a, b] = ["a", "b"].map(|k| db.intern(Type::Literal(Literal::Sym(db.intern_symbol(k)))));
    let f = |db: &Database, params| {
        db.intern(Type::Function(Function {
            params,
            result: int,
            input: None,
            output: None,
        }))
    };
    let one = f(&db, items(&db, vec![positional(Req, int)]));
    let maybe_two = f(
        &db,
        items(&db, vec![positional(Req, int), positional(Opt, int)]),
    );
    let two = f(
        &db,
        items(&db, vec![positional(Req, int), positional(Req, int)]),
    );
    let any = f(&db, items(&db, vec![positional(Rep, int)]));
    let ab = f(
        &db,
        items(&db, vec![keyed(Req, a, int), keyed(Req, b, int)]),
    );
    let ba = f(
        &db,
        items(&db, vec![keyed(Req, b, int), keyed(Req, a, int)]),
    );
    db.seal();
    for (x, y) in [(maybe_two, one), (any, two), (any, one), (ab, ba)] {
        assert_eq!(check(&db, x, y).status, Status::Proven, "{x:?} <: {y:?}");
    }
    // A function that takes one argument can't be called with two
    assert!(contradiction(
        &check(&db, one, maybe_two),
        Contradiction::Excess(1)
    ));
    assert!(contradiction(
        &check(&db, two, any),
        Contradiction::Missing(0)
    ));
}

#[test]
fn omitted_channels_are_gradual() {
    let mut db = Database::new();
    let num = nominal(&mut db, "Num", vec![], vec![]);
    let int = nominal(&mut db, "Int", vec![], vec![num]);
    let iter = nominal(&mut db, "Iter", vec![binder(Variance::Covariant)], vec![]);
    let sink = nominal(
        &mut db,
        "Sink",
        vec![binder(Variance::Contravariant)],
        vec![],
    );
    let [iter_num, iter_int] = [num, int].map(|t| apply(&db, iter, &[t]));
    let [sink_num, sink_int] = [num, int].map(|t| apply(&db, sink, &[t]));
    let f = |db: &Database, input, output| {
        db.intern(Type::Function(Function {
            params: schema(db, &[]),
            result: int,
            input,
            output,
        }))
    };
    let reads_nums = f(&db, Some(iter_num), None);
    let reads_ints = f(&db, Some(iter_int), None);
    let writes_nums = f(&db, None, Some(sink_num));
    let writes_ints = f(&db, None, Some(sink_int));
    let reads_int = f(&db, Some(int), None);
    let plain = f(&db, None, None);
    db.seal();
    // A function that writes only `Int`s can be given a sink of `Num`s
    for (x, y) in [
        (reads_nums, reads_ints),
        (writes_ints, writes_nums),
        (plain, reads_ints),
        (reads_ints, plain),
        (reads_int, plain),
        (plain, plain),
    ] {
        assert_eq!(check(&db, x, y).status, Status::Proven, "{x:?} <: {y:?}");
    }
    for (x, y) in [(reads_ints, reads_nums), (writes_nums, writes_ints)] {
        assert_eq!(
            check(&db, x, y).status,
            Status::Contradicted,
            "{x:?} <: {y:?}"
        );
    }
}
