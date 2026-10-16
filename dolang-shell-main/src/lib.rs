#![deny(warnings)]

// Boot uses load.import_handler even in binaries that only link this crate.
extern crate dolang_ext_load;

use std::error::Error;

use tokio::runtime::Builder;

use dolang::{
    extension::VmExt,
    resource,
    runtime::{self, error::ErrorKind, strand::Redirect, vm},
};

use dolang_ext_shell::{Exec, Exit};

use crate::terminal_state::TerminalRestoreGuard;

mod interactive;
mod load;
mod services;
mod terminal_state;

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
/// want to register via `dolang::extension!` and resource-producing crates.
/// Link exactly one `dolang.entrypoint/boot` resource to supply startup policy.
/// Boot receives the executable name in `shell.program` and raw arguments in
/// `shell.args`; the launcher installs no bundled-module or filesystem loader.
pub fn main() -> i32 {
    // Spawn a thread with a larger stack to avoid stack overflow in debug
    // builds, where deep call stacks of uninlined frames can exceed the
    // default stack size (particularly on Windows).
    const STACK_SIZE: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let terminal_restore = TerminalRestoreGuard::capture_if_terminal();
            let outcome = run();
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

fn run() -> Outcome {
    let mut argv = Vec::new();
    for arg in std::env::args_os() {
        match arg.into_string() {
            Ok(arg) => argv.push(arg),
            Err(arg) => {
                eprintln!(
                    "error: argument '{}' is not valid UTF-8",
                    arg.to_string_lossy()
                );
                return Outcome::Exit(2);
            }
        }
    }
    let program = argv.first().cloned();

    let rt = Builder::new_current_thread().enable_all().build().unwrap();

    rt.block_on(async move {
        vm::Builder::build(async move |builder| {
            for ext in builder.extensions() {
                ext.apply(builder).unwrap();
            }

            services::register(builder);

            builder
                .enter_with_slots(async move |strand, [mut stdin, mut stdout]| {
                    dolang_ext_shell::stdin(strand, &mut stdin);
                    dolang_ext_shell::default_output(strand, &mut stdout);
                    let ct = strand.interrupt_token();
                    let res =
                        Redirect::new(strand)
                            .input(stdin)
                            .output(&stdout)
                            .enter(async |strand| {
                                dolang_ext_shell::set_args(strand, argv.into_iter().skip(1))
                                    .await?;
                                dolang_ext_shell::set_program(strand, program).await?;
                                let boot = resource::get(resource::ENTRYPOINT, "boot").ok_or_else(
                                    || {
                                        runtime::Error::runtime(
                                            strand,
                                            "missing bundled boot entrypoint",
                                        )
                                    },
                                )?;
                                strand
                                    .with_slots(async |strand, [out]| {
                                        runtime::Bytecode::new(boot.as_bytes())
                                            .run(strand, out)
                                            .await
                                    })
                                    .await
                            });

                    let res = {
                        tokio::pin!(res);

                        loop {
                            tokio::select! {
                                // Register Tokio's SIGINT handler before a readline
                                // worker can save and later restore that handler.
                                biased;
                                _ = interrupt_signal(), if !ct.is_canceled() => { ct.cancel() }
                                res = (&mut res) => { break res }
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
