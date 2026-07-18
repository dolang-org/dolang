#![deny(warnings)]

use std::{error::Error, sync::Arc};

use tokio::runtime::Builder;

use dolang::{
    compile,
    extension::VmExt,
    runtime::{
        self, Output,
        error::ErrorKind,
        strand::Redirect,
        value::{Empty, Root},
        vm,
    },
};

use dolang_ext_shell::Exit;

use crate::batch::Action;
use crate::cli::{Cli, ParseOutcome};
use crate::interactive::{DYNAMIC_PRELUDE, DynamicPrelude};
use crate::terminal_state::TerminalRestoreGuard;

mod batch;
mod cli;
mod diagnostic;
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

fn get_action(cli: &Cli) -> Action {
    if cli.check {
        Action::Check
    } else if let Some(output) = &cli.compile {
        Action::Compile(output.clone())
    } else {
        Action::Run
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
            let _terminal_restore = TerminalRestoreGuard::capture_if_terminal();
            run(config)
        })
        .expect("failed to spawn main thread")
        .join()
        .expect("main thread panicked")
}

fn run(config: Arc<dyn Config>) -> i32 {
    let argv: Vec<_> = std::env::args_os().collect();
    let implicit_main =
        cli::infer_implicit_entrypoint(argv.first().map(|arg| arg.as_os_str()), |name| {
            config.bundled_entrypoint(name).is_some()
        });
    let mut cli = match cli::parse_from(argv, implicit_main) {
        ParseOutcome::Run(cli) => cli,
        ParseOutcome::Help(help) => {
            println!("{help}");
            return 0;
        }
        ParseOutcome::Error(error) => {
            eprintln!("{error}");
            return 2;
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

            let strict_mode = cli.strict;
            let module_paths = cli.module_paths.clone();
            builder.importer(async move |strand, name, out| {
                let path = load::find_module_file(strand, name, &module_paths).await?;
                load::load(
                    strand,
                    &path,
                    compile::Mode::Module { name },
                    strict_mode,
                    out,
                )
                .await
            });

            let importer_config = Arc::clone(&config);
            builder.importer(async move |strand, name, mut out| {
                if let Some(bytes) = importer_config.bundled_module(name) {
                    runtime::Bytecode::new(bytes).run(strand, &mut out).await
                } else {
                    Err(runtime::Error::import(strand, name))
                }
            });

            let batch_config = Arc::clone(&config);
            builder
                .enter_with_slots(async move |strand, [mut stdin, mut stdout]| {
                    dolang_ext_shell::stdin(strand, &mut stdin);
                    dolang_ext_shell::stdout(strand, &mut stdout);
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
                                    batch::main(strand, path, action, entrypoint, cli.strict).await
                                } else {
                                    interactive::main(strand, cli.strict).await
                                }
                            });

                    let res = {
                        tokio::pin!(res);

                        loop {
                            tokio::select! {
                                res = (&mut res) => { break res }
                                _ = tokio::signal::ctrl_c(), if !ct.is_canceled() => { ct.cancel() }
                            }
                        }
                    };

                    // Tokio stdout/stderr handles can retain buffered output
                    // when the runtime shuts down. Flush the exact stdout sink
                    // installed above and the shell extension's stderr writer
                    // while both are still alive.
                    let flush = dolang_ext_shell::flush(strand, &stdout).await;
                    let res = match (res, flush) {
                        (result @ Err(_), _) => result,
                        (Ok(()), flush) => flush,
                    };

                    match res {
                        Ok(()) => 0,
                        Err(e) => {
                            let exit_code = (e.kind() == ErrorKind::Abort)
                                .then(|| {
                                    e.source()
                                        .and_then(|e| e.downcast_ref::<Exit>())
                                        .map(|exit| exit.code)
                                })
                                .flatten();

                            if let Some(exit_code) = exit_code {
                                exit_code
                            } else {
                                diagnostic::print_backtrace(strand, e);
                                1
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
