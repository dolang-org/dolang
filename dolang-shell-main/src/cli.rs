use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[cfg(windows)]
use wax::{Glob, walk::Entry as _};

#[derive(Debug)]
pub(crate) struct Cli {
    pub(crate) path: Option<PathBuf>,
    pub(crate) module_paths: Vec<PathBuf>,
    pub(crate) prelude: Vec<PreludeImport>,
    pub(crate) main: bool,
    pub(crate) args: Vec<String>,
    pub(crate) check: bool,
    pub(crate) compile: Option<PathBuf>,
    pub(crate) strict: bool,
    pub(crate) cache: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreludeImport {
    Module {
        module: String,
        bind: Option<String>,
    },
    Item {
        module: String,
        item: String,
        bind: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) enum ParseOutcome {
    Run(Cli),
    Help(String),
    Error(String),
}

pub(crate) fn parse_from(
    args: impl IntoIterator<Item = OsString>,
    implicit_main: Option<String>,
) -> ParseOutcome {
    let mut args = args.into_iter();
    let program = args.next();
    let program = program_name(program.as_deref());

    if let Some(implicit_main) = implicit_main {
        let args = match args.map(into_string).collect::<Result<Vec<_>, _>>() {
            Ok(args) => args,
            Err(message) => return ParseOutcome::Error(argument_error(&program, message)),
        };
        return ParseOutcome::Run(Cli {
            path: Some(PathBuf::from(implicit_main)),
            module_paths: Vec::new(),
            prelude: Vec::new(),
            main: true,
            args,
            check: false,
            compile: None,
            strict: false,
            cache: true,
        });
    }

    let mut path = None;
    let mut module_paths = Vec::new();
    let mut prelude = Vec::new();
    let mut main = false;
    let mut check = false;
    let mut compile = None;
    let mut strict = false;
    let mut cache = true;
    let mut trailing = Vec::new();
    let mut stop_options = false;

    while let Some(arg) = args.next() {
        if path.is_some() {
            trailing.push(arg);
            continue;
        }

        if !stop_options {
            match arg.to_str() {
                Some("--") => {
                    stop_options = true;
                    continue;
                }
                Some("-h" | "--help") => return ParseOutcome::Help(help(&program)),
                Some("-m" | "--main") => {
                    if check {
                        return ParseOutcome::Error(conflict_error(&program, "--check", "--main"));
                    }
                    if compile.is_some() {
                        return ParseOutcome::Error(conflict_error(
                            &program,
                            "--compile",
                            "--main",
                        ));
                    }
                    main = true;
                    continue;
                }
                Some("--check") => {
                    if main {
                        return ParseOutcome::Error(conflict_error(&program, "--check", "--main"));
                    }
                    check = true;
                    continue;
                }
                Some("--compile") => {
                    let Some(output) = args.next() else {
                        return ParseOutcome::Error(missing_value_error(
                            &program,
                            "--compile <OUTPUT>",
                        ));
                    };
                    if main {
                        return ParseOutcome::Error(conflict_error(
                            &program,
                            "--compile",
                            "--main",
                        ));
                    }
                    compile = Some(PathBuf::from(output));
                    continue;
                }
                Some("--module-path") => {
                    let Some(module_path) = args.next() else {
                        return ParseOutcome::Error(missing_value_error(
                            &program,
                            "--module-path <PATH>",
                        ));
                    };
                    module_paths.push(PathBuf::from(module_path));
                    continue;
                }
                Some("--import") => {
                    let Some(value) = args.next() else {
                        return ParseOutcome::Error(missing_value_error(
                            &program,
                            "--import <MODULE[=NAME]>",
                        ));
                    };
                    let value = match into_string(value) {
                        Ok(value) => value,
                        Err(message) => {
                            return ParseOutcome::Error(argument_error(&program, message));
                        }
                    };
                    let (module, bind) = split_alias(value);
                    prelude.push(PreludeImport::Module { module, bind });
                    continue;
                }
                Some("--import-item") => {
                    let Some(value) = args.next() else {
                        return ParseOutcome::Error(missing_value_error(
                            &program,
                            "--import-item <MODULE.ITEM[=NAME]>",
                        ));
                    };
                    let value = match into_string(value) {
                        Ok(value) => value,
                        Err(message) => {
                            return ParseOutcome::Error(argument_error(&program, message));
                        }
                    };
                    let (qualified, bind) = split_alias(value);
                    let Some((module, item)) = qualified.rsplit_once('.') else {
                        return ParseOutcome::Error(argument_error(
                            &program,
                            "--import-item requires MODULE.ITEM[=NAME]",
                        ));
                    };
                    prelude.push(PreludeImport::Item {
                        module: module.to_owned(),
                        item: item.to_owned(),
                        bind,
                    });
                    continue;
                }
                Some("--no-cache") => {
                    cache = false;
                    continue;
                }
                Some("--strict") => {
                    strict = true;
                    continue;
                }
                _ => {}
            }

            if is_option_like(&arg) {
                return ParseOutcome::Error(unexpected_argument_error(&program, &arg));
            }
        }

        path = Some(PathBuf::from(arg));
    }

    if main && path.is_none() {
        return ParseOutcome::Error(missing_target_error(&program, "--main"));
    }

    let args = match expand_trailing_args(trailing) {
        Ok(args) => args,
        Err(message) => return ParseOutcome::Error(argument_error(&program, message)),
    };

    ParseOutcome::Run(Cli {
        path,
        module_paths,
        prelude,
        main,
        args,
        check,
        compile,
        strict,
        cache,
    })
}

fn split_alias(value: String) -> (String, Option<String>) {
    match value.split_once('=') {
        Some((value, bind)) => (value.to_owned(), Some(bind.to_owned())),
        None => (value, None),
    }
}

fn program_name(program: Option<&OsStr>) -> String {
    program
        .and_then(|program| Path::new(program).file_name())
        .and_then(OsStr::to_str)
        .filter(|program| !program.is_empty())
        .unwrap_or("dolang")
        .to_owned()
}

pub(crate) fn infer_implicit_entrypoint(
    program: Option<&OsStr>,
    has_entrypoint: impl Fn(&str) -> bool,
) -> Option<String> {
    let stem = Path::new(program?).file_stem()?.to_str()?;
    let stem = stem.strip_prefix("dolang-").unwrap_or(stem);
    if stem == "dolang" || stem.is_empty() {
        return None;
    }
    has_entrypoint(stem).then(|| stem.to_owned())
}

fn help(program: &str) -> String {
    format!(
        "\
Usage: {program} [OPTIONS] [PATH] [ARGS]...

Arguments:
  [PATH]     Script path (or bundled entrypoint name if -m is used)
  [ARGS]...  Script arguments (appear in `shell.args`)

Options:
  -m, --main              Run a bundled main entrypoint
      --check             Check syntax without executing
      --compile <OUTPUT>  Compile to bytecode file
      --module-path <PATH>  Add a module search path
      --import <MODULE[=NAME]>  Add a module to the prelude
      --import-item <MODULE.ITEM[=NAME]>  Add a module item to the prelude
      --no-cache          Disable the bytecode cache
      --strict            Treat warnings as errors
  -h, --help              Print help"
    )
}

fn usage(program: &str) -> String {
    format!("Usage: {program} [OPTIONS] [PATH] [ARGS]...")
}

fn conflict_error(program: &str, left: &str, right: &str) -> String {
    format!(
        "error: the argument '{left}' cannot be used with '{right}'\n\n{}\n\nFor more information, try '--help'.",
        usage(program)
    )
}

fn missing_value_error(program: &str, argument: &str) -> String {
    format!(
        "error: a value is required for '{argument}' but none was supplied\n\n{}\n\nFor more information, try '--help'.",
        usage(program)
    )
}

fn missing_target_error(program: &str, flag: &str) -> String {
    format!(
        "error: expected a target after '{flag}'\n\n{}\n\nFor more information, try '--help'.",
        usage(program)
    )
}

fn unexpected_argument_error(program: &str, arg: &OsStr) -> String {
    format!(
        "error: unexpected argument '{}' found\n\n{}\n\nFor more information, try '--help'.",
        arg.to_string_lossy(),
        usage(program)
    )
}

fn argument_error(program: &str, message: impl AsRef<str>) -> String {
    format!(
        "error: {}\n\n{}\n\nFor more information, try '--help'.",
        message.as_ref(),
        usage(program)
    )
}

fn is_option_like(arg: &OsStr) -> bool {
    arg.to_str()
        .is_some_and(|arg| arg.starts_with('-') && arg != "-")
}

fn expand_trailing_args(args: Vec<OsString>) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        expand_trailing_args_windows(args)
    }

    #[cfg(not(windows))]
    {
        args.into_iter()
            .map(into_string)
            .collect::<Result<Vec<_>, _>>()
    }
}

fn into_string(arg: OsString) -> Result<String, String> {
    arg.into_string()
        .map_err(|arg| format!("argument '{}' is not valid UTF-8", arg.to_string_lossy()))
}

#[cfg(windows)]
fn expand_trailing_args_windows(args: Vec<OsString>) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to determine current directory: {err}"))?;
    let mut expanded = Vec::new();

    for arg in args {
        let pattern = into_string(arg)?;
        if !looks_like_glob(&pattern) {
            expanded.push(pattern);
            continue;
        }

        let glob = match Glob::new(&pattern) {
            Ok(glob) => glob,
            Err(_) => {
                expanded.push(pattern);
                continue;
            }
        };

        let (prefix, glob) = glob.partition();
        let mut matches = Vec::new();

        if let Some(glob) = glob {
            for entry in glob.walk(cwd.join(&prefix)) {
                let entry = entry.map_err(|err| format!("failed to expand '{pattern}': {err}"))?;
                matches.push(prefix.join(entry.root_relative_paths().1));
            }
        } else if cwd.join(&prefix).exists() {
            matches.push(prefix);
        }

        if matches.is_empty() {
            expanded.push(pattern);
            continue;
        }

        expanded.extend(
            matches
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
    }

    Ok(expanded)
}

#[cfg(windows)]
fn looks_like_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        path::PathBuf,
    };

    use super::{Cli, ParseOutcome, PreludeImport, infer_implicit_entrypoint, parse_from};

    #[test]
    fn shell_help_before_target_exits() {
        let outcome = parse_from(args(["dolang", "--help"]), None);
        let ParseOutcome::Help(help) = outcome else {
            panic!("expected shell help");
        };
        assert!(help.contains("Usage: dolang"));
    }

    #[test]
    fn help_after_script_target_is_forwarded() {
        let cli = parse_ok(["dolang", "file.dol", "--help"]);
        assert_eq!(cli.path, Some(PathBuf::from("file.dol")));
        assert!(!cli.main);
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn help_after_main_target_is_forwarded() {
        let cli = parse_ok(["dolang", "-m", "pkg.tool", "--help"]);
        assert_eq!(cli.path, Some(PathBuf::from("pkg.tool")));
        assert!(cli.main);
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn shell_flags_before_target_still_apply() {
        let cli = parse_ok(["dolang", "--check", "--strict", "file.dol", "--help"]);
        assert!(cli.check);
        assert!(cli.strict);
        assert_eq!(cli.path, Some(PathBuf::from("file.dol")));
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn unknown_option_before_target_errors() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang", "--wat"]), None) else {
            panic!("expected error");
        };
        assert!(error.contains("unexpected argument '--wat'"));
    }

    #[test]
    fn unknown_option_after_target_is_forwarded() {
        let cli = parse_ok(["dolang", "file.dol", "--wat"]);
        assert_eq!(cli.args, ["--wat"]);
    }

    #[test]
    fn main_requires_target() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang", "-m"]), None) else {
            panic!("expected error");
        };
        assert!(error.contains("expected a target after '--main'"));
    }

    #[test]
    fn compile_requires_output() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang", "--compile"]), None) else {
            panic!("expected error");
        };
        assert!(error.contains("a value is required for '--compile <OUTPUT>'"));
    }

    #[test]
    fn module_path_requires_path() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang", "--module-path"]), None) else {
            panic!("expected error");
        };
        assert!(error.contains("a value is required for '--module-path <PATH>'"));
    }

    #[test]
    fn module_paths_preserve_command_line_order() {
        let cli = parse_ok([
            "dolang",
            "--module-path",
            "first",
            "--module-path",
            "second",
            "file.dol",
        ]);
        assert_eq!(
            cli.module_paths,
            [PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn prelude_imports_preserve_kind_alias_and_order() {
        let cli = parse_ok([
            "dolang",
            "--import",
            "fs",
            "--import",
            "shell=sh",
            "--import-item",
            "fs.open",
            "--import-item",
            "fs.write=writefile",
            "file.dol",
        ]);
        assert_eq!(
            cli.prelude,
            [
                PreludeImport::Module {
                    module: "fs".to_owned(),
                    bind: None,
                },
                PreludeImport::Module {
                    module: "shell".to_owned(),
                    bind: Some("sh".to_owned()),
                },
                PreludeImport::Item {
                    module: "fs".to_owned(),
                    item: "open".to_owned(),
                    bind: None,
                },
                PreludeImport::Item {
                    module: "fs".to_owned(),
                    item: "write".to_owned(),
                    bind: Some("writefile".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn import_item_uses_last_dot_as_separator() {
        let cli = parse_ok(["dolang", "--import-item", "foo.bar.item", "file.dol"]);
        assert_eq!(
            cli.prelude,
            [PreludeImport::Item {
                module: "foo.bar".to_owned(),
                item: "item".to_owned(),
                bind: None,
            }]
        );
    }

    #[test]
    fn import_item_requires_qualified_name() {
        let ParseOutcome::Error(error) =
            parse_from(args(["dolang", "--import-item", "item", "file.dol"]), None)
        else {
            panic!("expected error");
        };
        assert!(error.contains("--import-item requires MODULE.ITEM[=NAME]"));
    }

    #[test]
    fn no_cache_disables_cache() {
        let cli = parse_ok(["dolang", "--no-cache", "file.dol"]);
        assert!(!cli.cache);
    }

    #[test]
    fn check_and_main_conflict() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang", "--check", "-m", "mod"]), None)
        else {
            panic!("expected error");
        };
        assert!(error.contains("cannot be used with '--main'"));
    }

    #[test]
    fn compile_and_main_conflict() {
        let ParseOutcome::Error(error) = parse_from(
            args(["dolang", "--compile", "out.dolc", "--main", "test"]),
            None,
        ) else {
            panic!("expected error");
        };
        assert!(error.contains("cannot be used with '--main'"));
    }

    #[test]
    fn implicit_entrypoint_disables_shell_flag_parsing() {
        let cli = parse_ok_with(
            ["dolang-test", "--help", "--strict", "file.dol"],
            Some("test".to_owned()),
        );
        assert_eq!(cli.path, Some(PathBuf::from("test")));
        assert!(cli.main);
        assert_eq!(cli.args, ["--help", "--strict", "file.dol"]);
        assert!(!cli.strict);
        assert!(!cli.check);
    }

    #[test]
    fn infer_implicit_entrypoint_from_dolang_prefix() {
        let inferred =
            infer_implicit_entrypoint(Some(OsStr::new("/tmp/dolang-test")), |name| name == "test");
        assert_eq!(inferred.as_deref(), Some("test"));
    }

    #[test]
    fn infer_implicit_entrypoint_from_plain_stem() {
        let inferred = infer_implicit_entrypoint(Some(OsStr::new("test")), |name| name == "test");
        assert_eq!(inferred.as_deref(), Some("test"));
    }

    #[test]
    fn dolang_stem_does_not_trigger_implicit_entrypoint() {
        let inferred = infer_implicit_entrypoint(Some(OsStr::new("dolang")), |_| true);
        assert!(inferred.is_none());
    }

    #[test]
    fn option_terminator_forces_next_token_to_be_target() {
        let cli = parse_ok(["dolang", "--", "--help", "--check"]);
        assert_eq!(cli.path, Some(PathBuf::from("--help")));
        assert_eq!(cli.args, ["--check"]);
    }

    #[test]
    fn compile_and_check_preserve_current_precedence() {
        let cli = parse_ok(["dolang", "--compile", "out.dolc", "--check", "file.dol"]);
        assert!(cli.check);
        assert_eq!(cli.compile, Some(PathBuf::from("out.dolc")));
    }

    fn parse_ok(argv: impl IntoIterator<Item = &'static str>) -> Cli {
        parse_ok_with(argv, None)
    }

    fn parse_ok_with(
        argv: impl IntoIterator<Item = &'static str>,
        implicit_main: Option<String>,
    ) -> Cli {
        let ParseOutcome::Run(cli) = parse_from(os_args(argv), implicit_main) else {
            panic!("expected parsed command");
        };
        cli
    }

    fn args(args: impl IntoIterator<Item = &'static str>) -> Vec<OsString> {
        os_args(args)
    }

    fn os_args(args: impl IntoIterator<Item = &'static str>) -> Vec<OsString> {
        args.into_iter().map(OsString::from).collect()
    }
}
