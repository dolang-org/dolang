#![deny(warnings)]

use std::{error::Error, sync::Arc};

use tokio::runtime::Builder;

use dolang::{
    compile,
    extension::VmExt,
    runtime::{
        self, Error as RuntimeError, Output,
        error::ErrorKind,
        strand::Redirect,
        unpack,
        value::{Empty, Nil, Root},
        vm,
    },
};

use dolang_ext_shell::{Exec, Exit};

use crate::{
    batch::Action,
    cli::{Cli, ParseOutcome},
    interactive::{DYNAMIC_PRELUDE, DynamicPrelude},
    terminal_state::TerminalRestoreGuard,
};

mod batch;
mod cli;
mod interactive;
mod load;
mod terminal_state;

pub trait Config: Send + Sync + 'static {
    fn bundled_module(&self, name: &str) -> Option<&'static [u8]> {
        let _ = name;
        None
    }

    fn bundled_entrypoint(&self, name: &str) -> Option<&'static [u8]> {
        let _ = name;
        None
    }

    fn default_entrypoint(&self) -> Option<&str> {
        None
    }
}

/// Unpack the single name argument of a bundled resource lookup.
fn bundled_name<'v, 's>(
    strand: &mut runtime::Strand<'v, 's>,
    args: runtime::Args<'v, '_>,
) -> runtime::Result<'v, 's, String> {
    let ([name], []) = unpack!(strand, args, 1, 0)?;
    let name = name
        .as_str(strand)
        .ok_or_else(|| RuntimeError::type_error(strand, "name must be a Str"))?;
    Ok(name.to_string())
}

fn get_action(cli: &Cli) -> Action {
    if cli.check {
        Action::Check
    } else if let Some(output) = &cli.compile {
        Action::Compile(output.clone())
    } else {
        Action::Run
    }
}

#[cfg(unix)]
async fn interrupt_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(windows)]
async fn interrupt_signal() -> std::io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut ctrl_c = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;
    tokio::select! {
        _ = ctrl_c.recv() => Ok(()),
        _ = ctrl_break.recv() => Ok(()),
    }
}

/// Run a `dolang`-compatible CLI and return its process exit code.
///
/// Custom binaries can call this after linking any additional extensions they
/// want to register via `dolang::extension!`.
pub fn main(config: impl Config) -> i32 {
    // Spawn a thread with a larger stack to avoid stack overflow in debug
    // builds, where deep call stacks of uninlined frames can exceed the
    // default stack size (particularly on Windows).
    const STACK_SIZE: usize = 8 * 1024 * 1024;
    let config = Arc::new(config);

    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let terminal_restore = TerminalRestoreGuard::capture_if_terminal();
            let outcome = run(config);
            drop(terminal_restore);
            finish(outcome)
        })
        .expect("failed to spawn main thread")
        .join()
        .expect("main thread panicked")
}

enum Outcome {
    Exit(i32),
    Exec(std::process::Command),
}

fn finish(outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Exit(code) => code,
        Outcome::Exec(mut command) => launch(&mut command),
    }
}

#[cfg(unix)]
fn launch(command: &mut std::process::Command) -> i32 {
    use std::os::unix::process::CommandExt as _;
    eprintln!("failed to execute replacement process: {}", command.exec());
    1
}

#[cfg(windows)]
fn launch(command: &mut std::process::Command) -> i32 {
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("failed to execute replacement process: {error}");
            1
        }
    }
}

fn run(config: Arc<dyn Config>) -> Outcome {
    let argv: Vec<_> = std::env::args_os().collect();
    let implicit_main =
        cli::infer_implicit_entrypoint(argv.first().map(|arg| arg.as_os_str()), |name| {
            config.bundled_entrypoint(name).is_some()
        });
    let mut cli = match cli::parse_from(argv, implicit_main) {
        ParseOutcome::Run(cli) => cli,
        ParseOutcome::Help(help) => {
            println!("{help}");
            return Outcome::Exit(0);
        }
        ParseOutcome::Error(error) => {
            eprintln!("{error}");
            return Outcome::Exit(2);
        }
    };
    let action = get_action(&cli);

    let rt = Builder::new_current_thread().enable_all().build().unwrap();

    rt.block_on(async move {
        vm::Builder::build(async move |builder| {
            for ext in builder.extensions() {
                ext.apply(builder).unwrap();
            }

            if cli.path.is_none() && !cli.main {
                let dynamic_prelude = builder.register_type::<DynamicPrelude>();
                let mut root = Root::new(builder);
                Output::set(builder, &mut root, Empty::Dict);
                builder.module_object(DYNAMIC_PRELUDE, &dynamic_prelude, DynamicPrelude { root });
            }

            let compile_prelude = cli.prelude.clone();
            let importer_config = config.clone();
            let batch_config = config.clone();
            let module_config = config.clone();
            let entrypoint_config = config.clone();
            let backtrace_sym = builder.sym("backtrace");

            builder
                .module("_shell")
                .function("render_error", async move |strand, args, out| {
                    let ([error], [backtrace]) = unpack!(strand, args, 1, 0, backtrace_sym = None)?;
                    dolang_ext_shell::render_error(strand, &error, backtrace.as_deref(), out)
                })
                .function("compile_script", async move |strand, args, out| {
                    let ([path], []) = unpack!(strand, args, 1, 0)?;
                    let path = dolang_ext_shell::as_path(strand, &path).ok_or_else(|| {
                        RuntimeError::type_error(strand, "path must be a Str or fs.Path")
                    })?;
                    let bytecode = load::compile_script_cached(
                        strand,
                        &path,
                        &compile_prelude,
                        cli.strict,
                        cli.cache,
                    )
                    .await?;
                    Output::set(strand, out, bytecode.as_slice());
                    Ok(())
                })
                .function("bundled_module", async move |strand, args, out| {
                    let name = bundled_name(strand, args)?;
                    match module_config.bundled_module(&name) {
                        Some(bytecode) => Output::set(strand, out, bytecode),
                        None => Output::set(strand, out, Nil),
                    }
                    Ok(())
                })
                .function("bundled_entrypoint", async move |strand, args, out| {
                    let name = bundled_name(strand, args)?;
                    match entrypoint_config.bundled_entrypoint(&name) {
                        Some(bytecode) => Output::set(strand, out, bytecode),
                        None => Output::set(strand, out, Nil),
                    }
                    Ok(())
                })
                .commit()
                .importer(async move |strand, name, out| {
                    let path = load::find_module_file(strand, name, &cli.module_paths).await?;
                    load::load(
                        strand,
                        &path,
                        compile::Mode::Module { name },
                        &[],
                        cli.strict,
                        cli.cache,
                        out,
                    )
                    .await
                })
                .importer(async move |strand, name, mut out| {
                    if let Some(bytes) = importer_config.bundled_module(name) {
                        runtime::Bytecode::new(bytes).run(strand, &mut out).await
                    } else {
                        Err(runtime::Error::import(strand, name))
                    }
                })
                .enter_with_slots(async move |strand, [mut stdin, mut stdout]| {
                    dolang_ext_shell::stdin(strand, &mut stdin);
                    dolang_ext_shell::default_output(strand, &mut stdout);
                    let ct = strand.interrupt_token();
                    let res =
                        Redirect::new(strand)
                            .input(stdin)
                            .output(&stdout)
                            .enter(async |strand| {
                                dolang_ext_shell::set_args(strand, cli.args.drain(..)).await?;
                                dolang_ext_shell::set_program(
                                    strand,
                                    cli.path.as_ref().map(|path| {
                                        if cli.main {
                                            dolang_ext_shell::ProgramSource::Module(
                                                path.to_string_lossy().into_owned(),
                                            )
                                        } else {
                                            dolang_ext_shell::ProgramSource::Path(path.clone())
                                        }
                                    }),
                                )
                                .await?;
                                if let Some(path) = &cli.path {
                                    let entrypoint = if cli.main {
                                        let name = path.to_string_lossy();
                                        Some(
                                            batch_config
                                                .bundled_entrypoint(name.as_ref())
                                                .ok_or_else(|| {
                                                    runtime::Error::runtime(
                                                        strand,
                                                        format!(
                                                            "unknown bundled entrypoint: {name}"
                                                        ),
                                                    )
                                                })?,
                                        )
                                    } else {
                                        None
                                    };
                                    batch::main(
                                        strand,
                                        path,
                                        action,
                                        entrypoint,
                                        &cli.prelude,
                                        cli.strict,
                                        cli.cache,
                                    )
                                    .await
                                } else {
                                    interactive::main(strand, &cli.prelude, cli.strict).await
                                }
                            });

                    let res = {
                        tokio::pin!(res);

                        loop {
                            tokio::select! {
                                res = (&mut res) => { break res }
                                _ = interrupt_signal(), if !ct.is_canceled() => { ct.cancel() }
                            }
                        }
                    };

                    // Tokio stdio handles can retain buffered output when the
                    // runtime shuts down, so flush the process's standard
                    // streams and the console writer while they are still alive.
                    let flush = dolang_ext_shell::flush(strand).await;
                    let res = match (res, flush) {
                        (result @ Err(_), _) => result,
                        (Ok(()), flush) => flush,
                    };

                    match res {
                        Ok(()) => Outcome::Exit(0),
                        Err(e) => {
                            let exec = (e.kind() == ErrorKind::Abort)
                                .then(|| e.source()?.downcast_ref::<Exec>())
                                .flatten();
                            if let Some(exec) = exec {
                                return Outcome::Exec(exec.command());
                            }
                            let exit_code = (e.kind() == ErrorKind::Abort)
                                .then(|| {
                                    e.source()
                                        .and_then(|e| e.downcast_ref::<Exit>())
                                        .map(|exit| exit.code)
                                })
                                .flatten();

                            if let Some(exit_code) = exit_code {
                                Outcome::Exit(exit_code)
                            } else {
                                let _ = dolang_ext_shell::print_error_stderr(strand, e).await;
                                Outcome::Exit(1)
                            }
                        }
                    }
                })
                .await
        })
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::Config;

    struct EmptyConfig;

    impl Config for EmptyConfig {
        fn bundled_module(&self, _name: &str) -> Option<&'static [u8]> {
            None
        }
    }

    #[test]
    fn config_defaults_have_no_bundled_entrypoint_policy() {
        let config = EmptyConfig;
        assert!(config.bundled_entrypoint("main").is_none());
        assert!(config.default_entrypoint().is_none());
    }
}
