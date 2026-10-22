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
//! | `stubs` | Modules to add from `tests/typeck/stub/`; `std` is added whenever the case has no other | none |
//! | `repo-stubs` | Modules to add from the repository's native module stubs | none |
//! | `prelude` | `default` for the default prelude, `none` for an empty one | `default` |
//! | `pipes` | The types `strand`'s pipe placeholders stand for, sender then receiver, each `module.Item` | `strand.Sender strand.Receiver` |
//! | `validated` | `true` or `false`: whether the check reports no errors | not asserted |
//! | `bench` | `false`, `true` (source), or a list of `source`, `typelib`, `compile` | `false` |
//! | `skip` | `miri` to skip under Miri | none |

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use dolang::compile::{Config, ErrorKind, Mode, Severity, Span, UnitId, typeck};
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
                "bench" => {} // Validated by the case generator.
                key => panic!("{}: unknown setting `{key}: {value}`", path.display()),
            }
        }
    }
}

/// A file to compile as one unit
pub struct Source {
    pub path: PathBuf,
    /// The name diagnostics render it by
    pub file: String,
    content: Vec<u8>,
    /// The module name, or `None` for the script
    pub module: Option<String>,
    /// Whether the file belongs to the case, rather than being a stub it selected
    pub case: bool,
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

    pub fn text(&self) -> &str {
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
    // A repository stub's imports are met by their own repository stubs, as the
    // shell's check finds them. `std` is the fixtures' own.
    let mut queue = settings.repo_stubs.clone();
    let mut next = 0;
    while let Some(name) = queue.get(next).cloned() {
        next += 1;
        if sources
            .iter()
            .any(|source| source.module.as_ref() == Some(&name))
        {
            continue;
        }
        let (path, file) = repo_stub(&name);
        let source = Source::read(path, file, Some(name), false);
        let mut config = Config::new();
        config.document(true);
        let unit = config.unit(&source.path, source.text());
        queue.extend(
            (unit.imports().into_iter())
                .filter(|module| *module != "std" && find_repo_stub(module).is_some())
                .map(str::to_owned),
        );
        sources.push(source);
    }
    // The checker designates classes in `std` that every case relies on
    if !sources
        .iter()
        .any(|source| source.module.as_deref() == Some("std"))
    {
        let path = Path::new(ROOT).join("stub").join("std.dol");
        sources.push(Source::read(
            path,
            "stub/std.dol".to_owned(),
            Some("std".to_owned()),
            false,
        ));
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
    find_repo_stub(name).unwrap_or_else(|| panic!("no repository stub for module `{name}`"))
}

/// Find a native module's stub, if it has one.
fn find_repo_stub(name: &str) -> Option<(PathBuf, String)> {
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
        0 => None,
        1 => {
            let file = found.pop().unwrap();
            Some((Path::new(WORKSPACE).join(&file), file))
        }
        _ => panic!("several repository stubs for module `{name}`: {found:?}"),
    }
}

/// Loaded inputs shared by correctness tests and benchmarks.
pub struct Case {
    path: PathBuf,
    settings: Settings,
    pub sources: Vec<Source>,
}

impl Case {
    pub fn load(path: &Path) -> Self {
        let settings = Settings::load(path);
        let sources = sources(path, &settings);
        Self {
            path: path.to_owned(),
            settings,
            sources,
        }
    }

    pub fn config<'a>(&self, source: &'a Source, document: bool) -> Config<'a> {
        let mut config = Config::new();
        config.typecheck(true).document(document).recover(document);
        if self.settings.prelude_none {
            config.prelude().clear();
        }
        if let Some(name) = &source.module {
            config.mode(Mode::Module {
                name: name.as_str().into(),
            });
        }
        config
    }

    pub fn compile(&self) -> Vec<dolang::compile::Unit<'_>> {
        self.sources
            .iter()
            .map(|source| self.config(source, false).unit(&source.path, source.text()))
            .collect()
    }

    pub fn builder(&self) -> typeck::Builder<'_, '_> {
        let mut builder = typeck::Builder::new();
        if let Some([(sender_module, sender), (receiver_module, receiver)]) = &self.settings.pipes {
            builder.pipes((sender_module, sender), (receiver_module, receiver));
        }
        builder
    }
}

pub fn run(case: &Path) {
    let case = Case::load(case);
    validate(&case);
}

/// Validate prepared inputs without changing benchmark timing boundaries.
pub fn validate(case: &Case) {
    let settings = &case.settings;
    let sources = &case.sources;
    let units = case.compile();

    let mut checker = case.builder();
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
    from_typelibs(sources, &units, settings, &check, &ids, &mut failures);

    if !failures.is_empty() {
        panic!("{}:{failures}", case.path.display());
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
    let mut written: Vec<Option<Vec<u8>>> = Vec::new();
    let mut exportable = true;
    for (index, unit) in units.iter().enumerate() {
        if !(ids[index].is_some() && sources[index].module.is_some()) {
            written.push(None);
            continue;
        }
        match typeck::typelib(unit) {
            Ok(bytes) => written.push(Some(bytes)),
            // A module whose interface names a private declaration has no typelib,
            // and its check from source says why
            Err(error) if matches!(error.kind(), ErrorKind::NotExportable) => {
                let reported = check.diagnostics().any(|diag| {
                    let message = diag.message().to_string();
                    diag.severity() == Severity::Error
                        && diag.span().unit() == ids[index]
                        && message.starts_with("private ")
                        && message.contains(" in the public interface of ")
                });
                if !reported {
                    let _ = write!(
                        failures,
                        "\n{}: no typelib, but no private declaration in its interface \
                         is reported",
                        sources[index].file
                    );
                }
                exportable = false;
                written.push(None);
            }
            Err(error) => panic!("{}: {error}", sources[index].file),
        }
    }
    // The check from typelibs is compared only when every module has one
    if !exportable {
        return;
    }
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
    // What is reported in the script, then in the modules. A typelib keeps neither
    // a reference's span nor how it is written, so what is reported in a module is
    // compared without spans or quoted names.
    let diagnostics = |check: &typeck::Check| -> [Vec<String>; 2] {
        let mut split = [Vec::new(), Vec::new()];
        for diag in check.diagnostics() {
            let module = !script(diag.span().unit());
            let span = |span| match module {
                true => String::new(),
                false => format!("{span:?} "),
            };
            let text = |message: &str| match module {
                true => (message.split('`').enumerate())
                    .map(|(index, part)| if index % 2 == 1 { "_" } else { part })
                    .collect::<Vec<_>>()
                    .join("`"),
                false => message.to_owned(),
            };
            let mut out = format!(
                "{:?} {}{}",
                diag.severity(),
                span(diag.span()),
                text(&diag.message().to_string())
            );
            for annotation in diag.annotations() {
                let _ = write!(
                    out,
                    "; {}{}",
                    span(annotation.span()),
                    text(&annotation.message().to_string())
                );
            }
            for note in diag.notes() {
                let _ = write!(out, "; note: {}", text(&note.message().to_string()));
            }
            split[usize::from(module)].push(out);
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
