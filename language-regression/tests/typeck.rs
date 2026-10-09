#![deny(warnings)]

//! Type-checking tests over sets of units.
//!
//! Each case is a directory: `main.dol` is a script, and any other `x.y.dol` is
//! module `x.y`. The case's units, and any stubs its settings select, are compiled
//! and checked together. Nothing runs.
//!
//! Diagnostics are expected as `# error:` blocks in the file they point into, as in
//! the language regression tests, and `DOLANG_TEST_UPDATE` rewrites them. What the
//! checker concluded is asserted with annotations (see
//! [`dolang_private_test::annotate`]) whose payload is a judgment and its value:
//!
//! ```text
//! let p @ Pair = nil
//! #       ^~~~: ref a.Pair
//! ```
//!
//! Only annotated spans are asserted.
//!
//! Each case is then checked again with every module supplied by its typelib (see
//! [`typeck::typelib`]), as a library the interpreter bundles is.
//! Everything reported for the script must be the same, and what is reported for the
//! modules must be what was before, less what came of checking their bodies.
//!
//! # Settings
//!
//! A `.test` file in a case or in a group of cases holds `key: value` lines. A
//! deeper file overrides a shallower one key by key.
//!
//! | Key | Meaning | Default |
//! | --- | ------- | ------- |
//! | `stubs` | Modules to add from `tests/typeck/stub/` | none |
//! | `repo-stubs` | Modules to add from the repository's native module stubs | none |
//! | `prelude` | `default` for the default prelude, `none` for an empty one | `default` |
//! | `pipes` | The types `strand`'s pipe placeholders stand for, sender then receiver, each `module.Item` | `strand.Sender strand.Receiver` |
//! | `validated` | `true` or `false`: whether every well-formedness check must pass | not asserted |
//! | `skip` | `miri` to skip under Miri | none |

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use dolang::compile::{Config, ErrorKind, Mode, Span, UnitId, typeck};
use dolang_private_test::{Directive, annotate};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/typeck");
const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

#[derive(Default)]
struct Settings {
    stubs: Vec<String>,
    repo_stubs: Vec<String>,
    prelude_none: bool,
    /// Each nominee's module and item, sender first
    pipes: Option<[(String, String); 2]>,
    validated: Option<bool>,
}

impl Settings {
    /// Merge the `.test` files from the root of the fixtures down to `case`.
    fn load(case: &Path) -> Self {
        let root = Path::new(ROOT);
        let case = Path::new(env!("CARGO_MANIFEST_DIR")).join(case);
        let relative = case
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("{} is not under {ROOT}", case.display()));
        let mut settings = Settings::default();
        let mut dir = root.to_owned();
        settings.read(&dir);
        for component in relative.components() {
            dir.push(component);
            settings.read(&dir);
        }
        settings
    }

    fn read(&mut self, dir: &Path) {
        let path = dir.join(".test");
        let Ok(content) = fs::read_to_string(&path) else {
            return;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                panic!("{}: expected `key: value`, found `{line}`", path.display());
            };
            let value = value.trim();
            let names = || value.split_whitespace().map(str::to_owned).collect();
            match key.trim() {
                "stubs" => self.stubs = names(),
                "repo-stubs" => self.repo_stubs = names(),
                "prelude" => {
                    self.prelude_none = match value {
                        "default" => false,
                        "none" => true,
                        _ => panic!("{}: unknown prelude `{value}`", path.display()),
                    }
                }
                "pipes" => {
                    let nominee = |name: &str| {
                        let (module, item) = name.rsplit_once('.').unwrap_or_else(|| {
                            panic!("{}: expected `module.Item`, found `{name}`", path.display())
                        });
                        (module.to_owned(), item.to_owned())
                    };
                    let [sender, receiver] = value.split_whitespace().collect::<Vec<_>>()[..]
                    else {
                        panic!("{}: expected a sender and a receiver", path.display());
                    };
                    self.pipes = Some([nominee(sender), nominee(receiver)]);
                }
                "validated" => {
                    self.validated = Some(match value {
                        "true" => true,
                        "false" => false,
                        _ => panic!("{}: expected `true` or `false`", path.display()),
                    })
                }
                "skip" if value == "miri" => {}
                key => panic!("{}: unknown setting `{key}: {value}`", path.display()),
            }
        }
    }
}

/// A file to compile as one unit
struct Source {
    path: PathBuf,
    /// The name diagnostics render it by
    file: String,
    content: Vec<u8>,
    /// The module name, or `None` for the script
    module: Option<String>,
    /// Whether the file belongs to the case, rather than being a stub it selected
    case: bool,
}

impl Source {
    fn read(path: PathBuf, file: String, module: Option<String>, case: bool) -> Self {
        let content = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Source {
            path,
            file,
            content,
            module,
            case,
        }
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.content)
            .unwrap_or_else(|e| panic!("{}: not UTF-8: {e}", self.path.display()))
    }
}

/// The case's own files, sorted, then the stubs its settings select.
fn sources(case: &Path, settings: &Settings) -> Vec<Source> {
    let mut paths: Vec<PathBuf> = fs::read_dir(case)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension() == Some("dol".as_ref()))
        .collect();
    paths.sort();

    let mut sources = Vec::new();
    for path in paths {
        let file = path.file_name().unwrap().to_str().unwrap().to_owned();
        let stem = file.strip_suffix(".dol").unwrap();
        let module = (stem != "main").then(|| stem.to_owned());
        sources.push(Source::read(path, file, module, true));
    }
    for name in &settings.stubs {
        let path = Path::new(ROOT).join("stub").join(format!("{name}.dol"));
        assert!(path.exists(), "no stub `{name}` at {}", path.display());
        sources.push(Source::read(
            path,
            format!("stub/{name}.dol"),
            Some(name.clone()),
            false,
        ));
    }
    for name in &settings.repo_stubs {
        let (path, file) = repo_stub(name);
        sources.push(Source::read(path, file, Some(name.clone()), false));
    }

    let mut seen = HashMap::new();
    for source in &sources {
        if let Some(module) = &source.module
            && let Some(other) = seen.insert(module.as_str(), &source.file)
        {
            panic!(
                "module `{module}` is provided by both {other} and {}",
                source.file
            );
        }
    }
    sources
}

/// Find a native module's stub: module `x.y` is `x/y.dol` in a stub directory.
fn repo_stub(name: &str) -> (PathBuf, String) {
    let relative = format!("{}.dol", name.replace('.', "/"));
    let mut dirs = vec!["dolang".to_owned()];
    for entry in fs::read_dir(WORKSPACE).unwrap() {
        let entry = entry.unwrap().file_name().into_string().unwrap();
        if entry.starts_with("dolang-ext-") {
            dirs.push(entry);
        }
    }
    let mut found: Vec<_> = dirs
        .into_iter()
        .map(|dir| format!("{dir}/stub/{relative}"))
        .filter(|file| Path::new(WORKSPACE).join(file).exists())
        .collect();
    match found.len() {
        0 => panic!("no repository stub for module `{name}`"),
        1 => {
            let file = found.pop().unwrap();
            (Path::new(WORKSPACE).join(&file), file)
        }
        _ => panic!("several repository stubs for module `{name}`: {found:?}"),
    }
}

fn run(case: &Path) {
    let settings = Settings::load(case);
    let sources = sources(case, &settings);

    let units: Vec<_> = sources
        .iter()
        .map(|source| {
            let mut config = Config::new();
            config.typecheck(true);
            if settings.prelude_none {
                config.prelude().clear();
            }
            if let Some(name) = &source.module {
                config.mode(Mode::Module {
                    name: name.as_str().into(),
                });
            }
            config.unit(&source.path, source.text())
        })
        .collect();

    let mut checker = typeck::Builder::new();
    if let Some([(sender_module, sender), (receiver_module, receiver)]) = &settings.pipes {
        checker.pipes((sender_module, sender), (receiver_module, receiver));
    }
    // The source each checked unit came from
    let mut checked: HashMap<UnitId, usize> = HashMap::new();
    let mut ids: Vec<Option<UnitId>> = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        match checker.unit(unit) {
            Ok(id) => {
                checked.insert(id, index);
                ids.push(Some(id));
            }
            Err(error) if matches!(error.kind(), ErrorKind::Fail) => ids.push(None),
            Err(error) => panic!("{}: {error}", sources[index].file),
        }
    }
    let check = checker.check();
    // The sealed database must never make the solver panic, whatever errors it holds
    check.smoke();

    let mut diags: Vec<Vec<_>> = units
        .iter()
        .map(|unit| unit.diagnostics().collect())
        .collect();
    for diag in check.diagnostics() {
        let unit = diag
            .span()
            .unit()
            .expect("checker diagnostics name their unit");
        diags[checked[&unit]].push(diag.clone());
    }

    let mut failures = String::new();
    if let Some(expected) = settings.validated
        && check.validated() != expected
    {
        let _ = write!(
            failures,
            "\nexpected the check {}validated",
            if expected { "" } else { "not to be " }
        );
    }
    for (index, source) in sources.iter().enumerate() {
        let diags = &diags[index];
        if !source.case {
            for diag in diags {
                let display =
                    dolang_private_test::render_diag_display(&source.file, source.text(), diag);
                let _ = write!(failures, "\nunexpected diagnostic in a stub:\n{display}");
            }
            continue;
        }
        let mut directives = dolang_private_test::directives(&source.content);
        if dolang_private_test::match_diagnostics(
            &source.path,
            &source.content,
            &source.file,
            diags,
            &mut directives,
        ) {
            let _ = write!(failures, "\n{}: unexpected diagnostics", source.file);
        }
        for directive in &directives {
            if let Directive::DiagBlock {
                rendered: block, ..
            } = directive
            {
                let _ = write!(
                    failures,
                    "\n{}: missing diagnostic block:\n{block}",
                    source.file
                );
            }
        }
        judge(source, ids[index], &check, &mut failures);
    }
    from_typelibs(&sources, &units, &settings, &check, &ids, &mut failures);

    if !failures.is_empty() {
        panic!("{}:{failures}", case.display());
    }
}

/// Check the case again with every module supplied by its typelib, and compare what
/// that reports with the check from source.
fn from_typelibs(
    sources: &[Source],
    units: &[dolang::compile::Unit<'_>],
    settings: &Settings,
    check: &typeck::Check,
    ids: &[Option<UnitId>],
    failures: &mut String,
) {
    // A unit that failed to compile is left out of both checks
    let written: Vec<Option<Vec<u8>>> = (units.iter().enumerate())
        .map(|(index, unit)| {
            let module = ids[index].is_some() && sources[index].module.is_some();
            module.then(|| typeck::typelib(unit).unwrap())
        })
        .collect();
    let typelibs: Vec<Option<typeck::Typelib>> = (written.iter())
        .map(|bytes| {
            bytes
                .as_ref()
                .map(|bytes| typeck::Typelib::read(bytes).unwrap())
        })
        .collect();
    let mut checker = typeck::Builder::new();
    if let Some([(sender_module, sender), (receiver_module, receiver)]) = &settings.pipes {
        checker.pipes((sender_module, sender), (receiver_module, receiver));
    }
    for (index, unit) in units.iter().enumerate() {
        if ids[index].is_none() {
            continue;
        }
        let id = match &typelibs[index] {
            Some(typelib) => checker.typelib(typelib),
            None => checker.unit(unit),
        };
        assert_eq!(Some(id.unwrap()), ids[index], "units are added in order");
    }
    let other = checker.check();
    other.smoke();

    let script = |unit: Option<UnitId>| {
        let unit = unit.expect("checker locations name their unit");
        let index = ids.iter().position(|id| *id == Some(unit)).unwrap();
        sources[index].module.is_none()
    };
    // What is reported in the script, then in the modules
    let diagnostics = |check: &typeck::Check| -> [Vec<String>; 2] {
        let mut split = [Vec::new(), Vec::new()];
        for diag in check.diagnostics() {
            let mut out = format!("{:?} {:?} {}", diag.severity(), diag.span(), diag.message());
            for annotation in diag.annotations() {
                let _ = write!(out, "; {:?} {}", annotation.span(), annotation.message());
            }
            for note in diag.notes() {
                let _ = write!(out, "; note: {}", note.message());
            }
            split[usize::from(!script(diag.span().unit()))].push(out);
        }
        split
    };
    let undecided = |check: &typeck::Check| -> [Vec<String>; 2] {
        let mut split = [Vec::new(), Vec::new()];
        for (reason, span) in check.undecided() {
            split[usize::from(!script(span.unit()))].push(format!("{reason} {span:?}"));
        }
        split
    };
    let compare = |what: &str,
                   failures: &mut String,
                   source: [Vec<String>; 2],
                   typelib: [Vec<String>; 2]| {
        let [source_script, source_modules] = source;
        let [typelib_script, typelib_modules] = typelib;
        let mut rest = source_modules.iter();
        let subsequence = (typelib_modules.iter()).all(|found| rest.any(|item| item == found));
        if source_script != typelib_script || !subsequence {
            let _ = write!(
                failures,
                "\n{what} differ when modules are typelibs:\n  from source: {source_script:#?} {source_modules:#?}\n  from typelibs: {typelib_script:#?} {typelib_modules:#?}"
            );
        }
    };
    compare(
        "diagnostics",
        failures,
        diagnostics(check),
        diagnostics(&other),
    );
    compare(
        "undecided checks",
        failures,
        undecided(check),
        undecided(&other),
    );
    for (index, source) in sources.iter().enumerate() {
        let Some(id) = ids[index].filter(|_| source.module.is_none()) else {
            continue;
        };
        let judgments = |check: &typeck::Check| -> Vec<String> {
            (check.judgments(id).into_iter())
                .map(|judgment| format!("{} {:?} {}", judgment.name, judgment.span, judgment.value))
                .collect()
        };
        compare(
            &format!("{} judgments", source.file),
            failures,
            [judgments(check), Vec::new()],
            [judgments(&other), Vec::new()],
        );
    }
}

/// Check a case file's judgment annotations.
fn judge(source: &Source, id: Option<UnitId>, check: &typeck::Check, failures: &mut String) {
    let lines: Vec<&str> = source.text().split('\n').collect();
    let annotations = annotate::parse(&source.path, &lines);
    if annotations.is_empty() {
        return;
    }
    let Some(id) = id else {
        panic!(
            "{}: the unit failed to compile, so its annotations cannot hold",
            source.file
        );
    };
    let judgments = check.judgments(id);

    for annotation in &annotations {
        let (name, value) = annotation
            .payload
            .split_once(char::is_whitespace)
            .map_or((annotation.payload.as_str(), ""), |(name, value)| {
                (name, value.trim())
            });
        if !typeck::JUDGMENTS.contains(&name) {
            panic!(
                "{}:{}: unknown judgment `{name}`",
                source.file,
                annotation.line + 1
            );
        }
        let target = annotate::target_line(&annotations, annotation, &lines);
        let at = |span: &Span| {
            span.start().line_offset() as usize == target
                && span.end().line_offset() as usize == target
        };
        let found: Vec<&str> = judgments
            .iter()
            .filter(|judgment| {
                judgment.name == name
                    && at(&judgment.span)
                    && judgment.span.start().column_offset() as usize == annotation.start_col
                    && judgment.span.end().column_offset() as usize == annotation.end_col
            })
            .map(|judgment| judgment.value.as_str())
            .collect();
        let report = match found.as_slice() {
            [found] if *found == value => continue,
            [found] => format!("expected {name} {value}, found {name} {found}"),
            [] => {
                let mut report = format!("no {name} judgment spans these columns exactly");
                for judgment in judgments.iter().filter(|judgment| at(&judgment.span)) {
                    let _ = write!(
                        report,
                        "\n      {} {} at columns {}-{}",
                        judgment.name,
                        judgment.value,
                        judgment.span.start().column_offset() + 1,
                        judgment.span.end().column_offset()
                    );
                }
                report
            }
            several => format!("{} {name} judgments span these columns", several.len()),
        };
        let _ = write!(
            failures,
            "\n{}:{}: {report}\n{}",
            source.file,
            annotation.line + 1,
            annotate::excerpt(annotation, target, &lines)
        );
    }
}

include!(concat!(env!("OUT_DIR"), "/generated_typeck_tests.rs"));
