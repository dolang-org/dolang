use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[cfg(windows)]
use wax::{
    Glob,
    walk::Entry as _,
};

#[derive(Debug)]
pub(crate) struct Cli {
    pub(crate) path: Option<PathBuf>,
    pub(crate) module: bool,
    pub(crate) args: Vec<String>,
    pub(crate) check: bool,
    pub(crate) compile: Option<PathBuf>,
    pub(crate) strict: bool,
}

#[derive(Debug)]
pub(crate) enum ParseOutcome {
    Run(Cli),
    Help(String),
    Error(String),
}

pub(crate) fn parse_from(args: impl IntoIterator<Item = OsString>) -> ParseOutcome {
    let mut args = args.into_iter();
    let program = program_name(args.next().as_deref());

    let mut path = None;
    let mut module = false;
    let mut check = false;
    let mut compile = None;
    let mut strict = false;
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
                Some("-m" | "--module") => {
                    if check {
                        return ParseOutcome::Error(conflict_error(
                            &program, "--check", "--module",
                        ));
                    }
                    module = true;
                    continue;
                }
                Some("--check") => {
                    if module {
                        return ParseOutcome::Error(conflict_error(
                            &program, "--check", "--module",
                        ));
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
                    compile = Some(PathBuf::from(output));
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

    if module && path.is_none() {
        return ParseOutcome::Error(missing_target_error(&program, "--module"));
    }

    let args = match expand_trailing_args(trailing) {
        Ok(args) => args,
        Err(message) => return ParseOutcome::Error(argument_error(&program, message)),
    };

    ParseOutcome::Run(Cli {
        path,
        module,
        args,
        check,
        compile,
        strict,
    })
}

fn program_name(program: Option<&OsStr>) -> String {
    program
        .and_then(|program| Path::new(program).file_name())
        .and_then(OsStr::to_str)
        .filter(|program| !program.is_empty())
        .unwrap_or("dolang-shell")
        .to_owned()
}

fn help(program: &str) -> String {
    format!(
        "\
Usage: {program} [OPTIONS] [PATH] [ARGS]...

Arguments:
  [PATH]     Script path (or module name if -m is used)
  [ARGS]...  Script arguments (appear in `shell.args`)

Options:
  -m, --module            Import and run path as a module's main function
      --check             Check syntax without executing
      --compile <OUTPUT>  Compile to bytecode file
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
    use std::{ffi::OsString, path::PathBuf};

    use super::{Cli, ParseOutcome, parse_from};

    #[test]
    fn shell_help_before_target_exits() {
        let outcome = parse_from(args(["dolang-shell", "--help"]));
        let ParseOutcome::Help(help) = outcome else {
            panic!("expected shell help");
        };
        assert!(help.contains("Usage: dolang-shell"));
    }

    #[test]
    fn help_after_script_target_is_forwarded() {
        let cli = parse_ok(["dolang-shell", "file.dol", "--help"]);
        assert_eq!(cli.path, Some(PathBuf::from("file.dol")));
        assert!(!cli.module);
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn help_after_module_target_is_forwarded() {
        let cli = parse_ok(["dolang-shell", "-m", "pkg.tool", "--help"]);
        assert_eq!(cli.path, Some(PathBuf::from("pkg.tool")));
        assert!(cli.module);
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn shell_flags_before_target_still_apply() {
        let cli = parse_ok(["dolang-shell", "--check", "--strict", "file.dol", "--help"]);
        assert!(cli.check);
        assert!(cli.strict);
        assert_eq!(cli.path, Some(PathBuf::from("file.dol")));
        assert_eq!(cli.args, ["--help"]);
    }

    #[test]
    fn unknown_option_before_target_errors() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang-shell", "--wat"])) else {
            panic!("expected error");
        };
        assert!(error.contains("unexpected argument '--wat'"));
    }

    #[test]
    fn unknown_option_after_target_is_forwarded() {
        let cli = parse_ok(["dolang-shell", "file.dol", "--wat"]);
        assert_eq!(cli.args, ["--wat"]);
    }

    #[test]
    fn module_requires_target() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang-shell", "-m"])) else {
            panic!("expected error");
        };
        assert!(error.contains("expected a target after '--module'"));
    }

    #[test]
    fn compile_requires_output() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang-shell", "--compile"])) else {
            panic!("expected error");
        };
        assert!(error.contains("a value is required for '--compile <OUTPUT>'"));
    }

    #[test]
    fn check_and_module_conflict() {
        let ParseOutcome::Error(error) = parse_from(args(["dolang-shell", "--check", "-m", "mod"]))
        else {
            panic!("expected error");
        };
        assert!(error.contains("cannot be used with '--module'"));
    }

    #[test]
    fn option_terminator_forces_next_token_to_be_target() {
        let cli = parse_ok(["dolang-shell", "--", "--help", "--check"]);
        assert_eq!(cli.path, Some(PathBuf::from("--help")));
        assert_eq!(cli.args, ["--check"]);
    }

    #[test]
    fn compile_and_check_preserve_current_precedence() {
        let cli = parse_ok([
            "dolang-shell",
            "--compile",
            "out.dolc",
            "--check",
            "file.dol",
        ]);
        assert!(cli.check);
        assert_eq!(cli.compile, Some(PathBuf::from("out.dolc")));
    }

    fn parse_ok(argv: impl IntoIterator<Item = &'static str>) -> Cli {
        let ParseOutcome::Run(cli) = parse_from(os_args(argv)) else {
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
