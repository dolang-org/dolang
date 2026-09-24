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
