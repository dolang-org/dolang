use dolang_compile::{Config, ErrorKind, Mode, typeck};
use std::path::Path;

fn config(mode: Mode<'_>) -> Config<'_> {
    let mut config = Config::new();
    config.mode(mode).typecheck(true);
    config
}

#[test]
fn builder_assigns_ids_in_order_and_rejects_duplicate_modules() {
    let path = Path::new("same.dol");
    let first = config(Mode::Module { name: "first" }).unit(path, b"");
    let second = config(Mode::Module { name: "second" }).unit(path, b"");
    let script = config(Mode::Script).unit(path, b"");
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
    let failed = config(Mode::Script).unit(path, b"let =\n");
    let unchecked = Config::new().unit(path, b"");
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
    let unit = config(Mode::Script).unit(Path::new("unit.dol"), b"let x @ Missing = 1\n");
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
    let unit = config(Mode::Module { name: "m" }).unit(Path::new("m.dol"), source.as_bytes());
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
    let geo = config(Mode::Module { name: "geo" }).unit(
        Path::new("geo.dol"),
        b"pub class Box[T]\n  pub field item @ T = nil\npub def get[T] b @ Box[T] -> T\n  b.item\n",
    );
    let user = config(Mode::Module { name: "user" }).unit(
        Path::new("user.dol"),
        b"import geo:\n  - Box\npub class Crate[T]: Box[T]\n  pub field extra @ Box[T] = nil\n",
    );
    let judge = |units: [(&'static str, &dolang_compile::Unit<'_>); 2]| {
        let mut checker = typeck::Builder::new();
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
