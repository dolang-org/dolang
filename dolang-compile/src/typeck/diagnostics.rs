use crate::{Config, ErrorKind, Mode, Unit, typeck};
use std::{path::Path, thread};

fn config(mode: Mode<'_>) -> Config<'_> {
    let mut config = Config::new();
    config.mode(mode).typecheck(true);
    config
}

#[test]
fn builder_assigns_ids_in_order_and_rejects_duplicate_modules() {
    let path = Path::new("same.dol");
    let first = config(Mode::Module {
        name: "first".into(),
    })
    .unit(path, "");
    let second = config(Mode::Module {
        name: "second".into(),
    })
    .unit(path, "");
    let script = config(Mode::Script).unit(path, "");
    // Checking needs resolved types, not the document index
    assert!(script.nodes().next().is_none());
    let mut checker = typeck::Builder::new();
    let ids = [&first, &second, &script].map(|unit| checker.unit(unit).unwrap());
    assert_eq!(ids.map(|id| id.index()), [0, 1, 2]);
    assert!(matches!(
        checker.unit(&first).map_err(|error| error.kind()),
        Err(ErrorKind::DuplicateModule)
    ));
    // Scripts have no module name to collide on
    checker.unit(&script).unwrap();
}

#[test]
fn builder_rejects_units_without_resolved_types() {
    let path = Path::new("unit.dol");
    let failed = config(Mode::Script).unit(path, "let =\n");
    let unchecked = Config::new().unit(path, "");
    let mut checker = typeck::Builder::new();
    assert!(matches!(
        checker.unit(&failed).map_err(|error| error.kind()),
        Err(ErrorKind::Fail)
    ));
    assert!(matches!(
        checker.unit(&unchecked).map_err(|error| error.kind()),
        Err(ErrorKind::Unresolved)
    ));
}

#[test]
fn check_excludes_unit_diagnostics() {
    let unit = config(Mode::Script).unit(Path::new("unit.dol"), "let x @ Missing = 1\n");
    assert!(unit.diagnostics().next().is_some());
    let mut checker = typeck::Builder::new();
    checker.unit(&unit).unwrap();
    assert!(checker.check().diagnostics().next().is_none());
}

#[test]
fn oversized_binder_group_is_diagnosed_and_seals() {
    let binders: Vec<_> = (0..=usize::from(u16::MAX) + 1)
        .map(|n| format!("T{n}"))
        .collect();
    let source = format!(
        "pub class Big[{}]\npub let big @ Big[] = nil\n",
        binders.join(", ")
    );
    let unit = config(Mode::Module { name: "m".into() }).unit(Path::new("m.dol"), source);
    let mut checker = typeck::Builder::new();
    checker.unit(&unit).unwrap();
    let check = checker.check();
    let messages: Vec<_> = check
        .diagnostics()
        .map(|diag| diag.message().to_string())
        .collect();
    assert_eq!(
        messages,
        ["declaration has more than 65536 binders, counting those it captures"]
    );
    check.smoke();
}

#[test]
fn judgments_do_not_depend_on_the_order_units_are_added() {
    let geo = config(Mode::Module { name: "geo".into() }).unit(
        Path::new("geo.dol"),
        "pub class Box[T]\n  pub field item @ T = nil\npub def get[T] b @ Box[T] -> T\n  b.item\n",
    );
    let user = config(Mode::Module {
        name: "user".into(),
    })
    .unit(
        Path::new("user.dol"),
        "import geo:\n  - Box\npub class Crate[T]: Box[T]\n  pub field extra @ Box[T] = nil\n",
    );
    let judge = |units: [(&'static str, &crate::Unit<'_>); 2]| {
        let mut checker = typeck::Builder::new();
        checker.analysis(true);
        let ids = units.map(|(_, unit)| checker.unit(unit).unwrap());
        let check = checker.check();
        check.smoke();
        // Keyed by the unit, whatever ID it was given
        let mut judgments: Vec<_> = units
            .iter()
            .zip(ids)
            .flat_map(|(&(name, _), id)| {
                check.judgments(id).into_iter().map(move |judgment| {
                    (
                        name,
                        judgment.name,
                        judgment.span.start().byte_offset(),
                        judgment.span.end().byte_offset(),
                        judgment.value,
                    )
                })
            })
            .collect();
        judgments.sort();
        judgments
    };
    let forward = judge([("geo", &geo), ("user", &user)]);
    assert!(!forward.is_empty());
    assert_eq!(forward, judge([("user", &user), ("geo", &geo)]));
}

#[test]
fn results_do_not_depend_on_threads() {
    let module = |name: &str, source: &'static str| {
        config(Mode::Module {
            name: name.to_owned().into(),
        })
        .unit(Path::new(&format!("{name}.dol")), source)
    };
    let script =
        |path: &str, source: &'static str| config(Mode::Script).unit(Path::new(path), source);
    let units = [
        module(
            "geo",
            "pub class Box[T]\n  pub field item @ T = nil\npub def get[T] b @ Box[T] -> T\n  b.item\n",
        ),
        module(
            "user",
            "import geo:\n  - Box\n  - get\nimport split:\n  - Num\npub class Crate[T]: Box[T]\n  pub field extra @ Box[T] = nil\npub class Bounded[T @ Num]\npub let bad @ Bounded[Crate[Num]] = nil\npub def unbox b @ Box[Num] -> Crate[Num]\n  get $b\n",
        ),
        module(
            "split",
            "pub class Num\npub class Split[*Ts, U @ Num]\n  pub def items _self -> Split[...Ts, U]\n    nil\npub def split[*Xs] x @ Split[...Xs, Num]\n  x\n",
        ),
        script(
            "first.dol",
            "import geo:\n  - Box\nimport split:\n  - Num\nlet b @ Box[Num] = (Box())\nlet n @ Box[Num] = b.item\n",
        ),
        script(
            "second.dol",
            "import user:\n  - Crate\n  - Bounded\nlet c = (Crate())\nlet x @ Bounded[Crate[Crate[Int]]] = nil\n",
        ),
        // Runs of its own: early, late, and nested in another
        script(
            "runs.dol",
            "let greeting = :hi:\ngreeting = :hello:\ndef show() -> Int\n  greeting\ndef outer[T] x @ T\n  def inner() -> Int\n    x\n  inner\ndef early a @ Int -> Str\n  a\n",
        ),
    ];
    // A pool of `threads` threads, else the global pool
    let check = |threads: Option<usize>| {
        let mut checker = typeck::Builder::new();
        checker.analysis(true);
        if let Some(threads) = threads {
            checker.threads(threads.try_into().unwrap());
        }
        let ids = units.each_ref().map(|unit| checker.unit(unit).unwrap());
        let check = checker.check();
        check.smoke();
        let diagnostics: Vec<String> = check
            .diagnostics()
            .map(|diag| {
                let annotations: Vec<_> = diag
                    .annotations()
                    .map(|annotation| (annotation.span(), annotation.message().to_string()))
                    .collect();
                let notes: Vec<_> = diag
                    .notes()
                    .map(|note| note.message().to_string())
                    .collect();
                format!(
                    "{:?} {:?} {} {annotations:?} {notes:?}",
                    diag.severity(),
                    diag.span(),
                    diag.message()
                )
            })
            .collect();
        let judgments: Vec<Vec<String>> = ids
            .iter()
            .map(|&id| {
                check
                    .judgments(id)
                    .into_iter()
                    .map(|judgment| {
                        format!("{} {:?} {}", judgment.name, judgment.span, judgment.value)
                    })
                    .collect()
            })
            .collect();
        (
            diagnostics,
            check.validated(),
            format!("{:?}", check.undecided()),
            judgments,
        )
    };
    let sequential = check(Some(1));
    // The fixture exercises errors, undecided checks and judgments in every unit
    assert!(!sequential.0.is_empty());
    assert!(!sequential.1);
    assert!(sequential.3.iter().all(|judgments| !judgments.is_empty()));
    assert_eq!(sequential, check(Some(4)));
    assert_eq!(sequential, check(None));
}

#[test]
fn imports_lists_statement_and_prelude_modules() {
    let source = "import zeta\nimport alpha.beta:\n  - Item\ndef f()\n  import zeta: z\n  import @gamma\n  spawn f\n";
    let mut config = config(Mode::Script);
    config.document(true);
    let unit = config.unit(Path::new("unit.dol"), source);
    assert_eq!(unit.imports(), ["alpha.beta", "gamma", "strand", "zeta"]);
    // The list comes from the document index
    let unit = Config::new().unit(Path::new("unit.dol"), source);
    assert!(unit.imports().is_empty());
}

#[test]
fn unit_source_round_trips() {
    let path = Path::new("unit.dol");
    let source = "# ünïcödé\nlet x = \"π\"\n";
    let unit = Config::new().unit(path, source);
    assert!(std::ptr::eq(unit.source(), source));
    let unit: Unit<'static> = Config::new().unit(path, source.to_owned());
    assert_eq!(unit.source(), source);
}

#[test]
fn repl_reassigns_only_prelude_items() {
    let refused = |source: &str| {
        let mut config = config(Mode::Repl);
        config
            .prelude()
            .import_module("std")
            .import_items("env")
            .item("count")
            .commit();
        let unit = config.unit(Path::new("repl.dol"), source);
        unit.diagnostics()
            .any(|diag| diag.message().to_string() == "imported bindings cannot be reassigned")
    };
    // The REPL threads its environment through the prelude's items
    assert!(!refused("count = 1\n"));
    assert!(refused("std = 1\n"));
    assert!(refused("import json\njson = 1\n"));
}

#[test]
fn owned_unit_is_shared_between_threads() {
    let path = Path::new("m.dol");
    let module = || Mode::Module {
        name: String::from("m").into(),
    };
    let messages = |unit: &Unit<'_>| {
        unit.diagnostics()
            .map(|diag| diag.message().to_string())
            .collect::<Vec<_>>()
    };
    let emit = |unit: Unit<'_>| {
        let mut out = Vec::new();
        unit.emit(&mut out).unwrap();
        out
    };

    let source = "let x @ Missing = 1\n";
    let expected = messages(&config(Mode::Module { name: "m".into() }).unit(path, source));
    assert!(!expected.is_empty());
    let unit: Unit<'static> = config(module()).unit(path, source.to_owned());
    let unit = thread::spawn(move || unit).join().unwrap();
    thread::scope(|scope| {
        let readers = [(); 2].map(|()| scope.spawn(|| messages(&unit)));
        for reader in readers {
            assert_eq!(reader.join().unwrap(), expected);
        }
    });

    let source = "pub let x = 1\n";
    let expected = emit(config(Mode::Module { name: "m".into() }).unit(path, source));
    let unit: Unit<'static> = config(module()).unit(path, source.to_owned());
    assert_eq!(thread::spawn(move || emit(unit)).join().unwrap(), expected);
}

#[test]
fn returns_cannot_exit_modules_or_repl() {
    let sources = [
        "return\n",
        "return 42\n",
        "if true\n  return 42\n",
        "while true\n  return\n",
        "for x = [1]\n  return x\n",
        "try\n  return\ncatch _\n  nil\n",
        "match 1\n  _ do return\n",
        "let _f = do return 42\n",
        "def call block\n  block()\ncall do return 42\n",
        "throw 1\nreturn\n",
    ];
    for mode in [Mode::Module { name: "m".into() }, Mode::Repl] {
        let expected = if mode == Mode::Repl {
            "return at top level of REPL"
        } else {
            "return at top level of module"
        };
        for source in sources {
            for typecheck in [false, true] {
                for recover in [false, true] {
                    let mut config = Config::new();
                    config
                        .mode(mode.clone())
                        .typecheck(typecheck)
                        .recover(recover);
                    let unit = config.unit(Path::new("return.dol"), source);
                    let diag = unit
                        .diagnostics()
                        .find(|diag| diag.message().to_string() == expected);
                    assert!(
                        diag.is_some(),
                        "{source:?}, typecheck={typecheck}, recover={recover}"
                    );
                    let span = diag.unwrap().span().span();
                    assert_eq!(
                        &source[span.start().byte_offset()..span.end().byte_offset()],
                        "return"
                    );
                    assert!(unit.emit(&mut Vec::new()).is_err());
                }
            }
        }
    }
}

#[test]
fn returns_remain_valid_in_scripts_and_declared_functions() {
    let sources = [
        "pub def f()\n  return 42\nf()\n",
        "def call block\n  block()\npub def f()\n  call do return 42\nf()\n",
        "pub class C\n  pub def f self\n    return 42\nC().f()\n",
        "def call block\n  block()\npub class C\n  pub field x = call do return 42\nC().x\n",
    ];
    for mode in [Mode::Script, Mode::Module { name: "m".into() }, Mode::Repl] {
        for source in sources {
            let mut config = Config::new();
            config.mode(mode.clone());
            let unit = config.unit(Path::new("functions.dol"), source);
            assert!(unit.emit(&mut Vec::new()).is_ok(), "{source:?}");
        }
    }
    for source in [
        "return\n",
        "return 42\n",
        "if true\n  return 42\n",
        "def call block\n  block()\ncall do return 42\n",
    ] {
        let unit = Config::new().unit(Path::new("script.dol"), source);
        assert!(unit.emit(&mut Vec::new()).is_ok(), "{source:?}");
    }
}

#[test]
fn type_only_prelude_bindings_have_no_runtime_effect() {
    for mode in [Mode::Script, Mode::Module { name: "m".into() }, Mode::Repl] {
        for document in [false, true] {
            for typecheck in [false, true] {
                let mut config = Config::new();
                config
                    .mode(mode.clone())
                    .document(document)
                    .typecheck(typecheck);
                config
                    .prelude()
                    .import_items("missing_at_runtime")
                    .type_item("Unused")
                    .type_items(["Input"])
                    .type_item_with_name("Output", "Result")
                    .commit();
                let unit = config.unit(
                    Path::new("m.dol"),
                    "let answer @ Input = 42\nlet result @ Result = answer\nresult\n",
                );
                assert!(unit.diagnostics().next().is_none());
                if document {
                    let items: Vec<_> = unit
                        .nodes()
                        .filter_map(|(_, node)| match node.kind() {
                            crate::Kind::PreludeItem {
                                module: "missing_at_runtime",
                                item,
                                type_only,
                                ..
                            } => Some((item, type_only)),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(items, [("Input", true), ("Output", true)]);
                }
                let mut bytes = Vec::new();
                unit.emit(&mut bytes).unwrap();
                // A missing module name must not enter emitted bytecode.
                assert!(
                    !bytes
                        .windows(b"missing_at_runtime".len())
                        .any(|window| window == b"missing_at_runtime")
                );
            }
        }
    }
}

#[test]
fn type_only_prelude_values_are_unbound() {
    for typecheck in [false, true] {
        for name in ["Never", "Seq", "BaseArray", "BaseDict"] {
            let mut config = Config::new();
            config.typecheck(typecheck);
            let unit = config.unit(Path::new("m.dol"), name.to_owned());
            assert!(
                unit.diagnostics()
                    .any(|diag| diag.message().to_string().contains("unbound")),
                "{name}"
            );
        }
    }
}

#[test]
fn type_only_prelude_shadowing() {
    let mut config = Config::new();
    config.document(true).typecheck(true);
    config
        .prelude()
        .import_items("custom")
        .type_item("Name")
        .commit();
    for source in [
        "let Name = Int\nlet value @ Name = 1\nvalue\n",
        "@let Name = Int\nlet value @ Name = 1\nvalue\n",
        "import local:\n  - @Name\nlet value @ Name = 1\nvalue\n",
        "def identity[Name] value@Name -> Name\n  value\nidentity 1\n",
    ] {
        let unit = config.unit(Path::new("m.dol"), source);
        assert!(
            !unit
                .diagnostics()
                .any(|diag| diag.message().to_string().contains("unbound"))
        );
        assert!(!unit.nodes().any(|(_, node)| matches!(
            node.kind(),
            crate::Kind::PreludeItem {
                module: "custom",
                ..
            }
        )));
    }
}

#[test]
fn later_prelude_items_win_in_types_only() {
    for last_type_only in [false, true] {
        let mut config = Config::new();
        config.document(true);
        let items = config.prelude().import_items("custom");
        let items = if last_type_only {
            items
                .item_with_name("Runtime", "Name")
                .type_item_with_name("TypeOnly", "Name")
        } else {
            items
                .type_item_with_name("TypeOnly", "Name")
                .item_with_name("Runtime", "Name")
        };
        items.commit();
        let unit = config.unit(Path::new("m.dol"), "let value @ Name = 1\nName\nvalue\n");
        for (_, node) in unit.nodes() {
            if let crate::Kind::NameType {
                target: Some(target),
                ..
            } = node.kind()
            {
                assert!(
                    matches!(unit.node(target).unwrap().kind(), crate::Kind::PreludeItem { type_only, .. } if type_only == last_type_only)
                );
            }
        }
        let mut bytes = Vec::new();
        unit.emit(&mut bytes).unwrap();
        assert!(
            !bytes
                .windows(b"TypeOnly".len())
                .any(|part| part == b"TypeOnly")
        );
    }
}

#[test]
fn module_prelude_bindings_follow_type_only_configuration_order() {
    for last_type_only in [false, true] {
        let mut config = Config::new();
        config.document(true);
        let prelude = config.prelude();
        if last_type_only {
            prelude
                .import_module_with_name("module", "Name")
                .import_items("types")
                .type_item_with_name("Item", "Name")
                .commit();
        } else {
            prelude
                .import_items("types")
                .type_item_with_name("Item", "Name")
                .commit()
                .import_module_with_name("module", "Name");
        }
        let unit = config.unit(Path::new("m.dol"), "let value @ Name = nil\nvalue\n");
        assert!(unit.diagnostics().next().is_none());
        let (_, node) = unit
            .nodes()
            .find(|(_, node)| matches!(node.kind(), crate::Kind::NameType { .. }))
            .unwrap();
        let crate::Kind::NameType {
            target: Some(target),
            ..
        } = node.kind()
        else {
            panic!("missing target")
        };
        assert_eq!(
            matches!(
                unit.node(target).unwrap().kind(),
                crate::Kind::PreludeItem {
                    type_only: true,
                    ..
                }
            ),
            last_type_only
        );
    }
}
