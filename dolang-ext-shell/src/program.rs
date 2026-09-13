use dolang::runtime::strand::InterruptMask;
use dolang_vfs::process::Command;
use futures::future::MaybeDone;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value, method,
    object::{TypeBuilder, Unpack, UnpackItem},
    unpack,
    value::{Nil, Singleton},
    vm::Builder,
};
use dolang_vfs::path as vfs_path;
use dolang_vfs::{process::ProcessControl, target::OperatingSystem};

use crate::{
    error::{self, ResultExt as _},
    fs::{
        file::{self, File},
        path::{PathAnnex, create_path_annex, path_from_value},
    },
    global::Global,
    io_mode::{IoMode, encode_value, read_value},
    pipe_channel::{self, RecvGuard, SendGuard},
    proc::{parse_policy_dict, vfs_policy},
};

pub(crate) struct Program;

pub(crate) struct ProgramAnnex<'v> {
    name: String,
    global: State<'v, Global<'v>>,
}

fn program_name_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, String> {
    if global.types.unix_path.cast(value).is_some()
        || global.types.windows_path.cast(value).is_some()
    {
        let path = path_from_value(strand, global, value)?;
        let path = if path.is_absolute() {
            path
        } else {
            global.local.get(strand).cwd().join(path)
        };
        Ok(path.as_str().to_owned())
    } else if let Some(name) = value.as_str(strand) {
        Ok(name.to_string())
    } else if let Some(name) = value.as_sym(strand) {
        Ok(name.as_str(strand.vm()).to_string())
    } else {
        Err(Error::type_error(
            strand,
            "program must be a string, symbol, or Path",
        ))
    }
}

/// One value per standard stream.
///
/// Purely for grouping — the fields are the names, so callers say
/// `explicit.stdout` rather than indexing a bare triple.
#[derive(Clone, Copy, Debug, Default)]
struct Streams<T> {
    stdin: T,
    stdout: T,
    stderr: T,
}

/// What [`resolve_io`] worked out about a launch's standard streams.
struct ResolvedIo<'v, 'a> {
    /// The launch arguments with the reserved keywords removed.
    args: Args<'v, 'a>,
    /// Streams `run` opened itself and must close when the launch finishes.
    temp: Streams<bool>,
    /// Streams the caller named explicitly. A named stream is pinned to what it
    /// names; an unnamed one is anonymous and follows the ambient console.
    explicit: Streams<bool>,
    /// The reserved `policy:` argument, if given.
    policy: Option<Slot<'v, 'a>>,
    /// How a pumped output stream is quantized into values.
    mode: IoMode,
}

async fn resolve_io<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, 'a>,
    mut input: Slot<'v, '_>,
    mut output: Slot<'v, '_>,
    mut stderr: Slot<'v, '_>,
) -> Result<'v, 's, ResolvedIo<'v, 'a>> {
    let stdin_sym = global.syms.stdin;
    let stdout_sym = global.syms.stdout;
    let stderr_sym = global.syms.stderr;
    let policy_sym = global.syms.policy;
    let mode_sym = global.syms.mode;
    let ([], [stdin_key, stdout_key, stderr_key, policy_key, mode_key], rest) = unpack!(
        strand,
        args,
        0,
        0,
        stdin_sym = None,
        stdout_sym = None,
        stderr_sym = None,
        policy_sym = None,
        mode_sym = None,
        ...
    )?;
    // Framing for whichever output streams end up pumped into a sink. Applies
    // to both, since a redirect that splits them is already naming two sinks
    // and can chomp them independently.
    let mode = crate::io_mode::parse_mode(strand, mode_key.as_deref())?;
    let explicit = Streams {
        stdin: stdin_key.is_some(),
        stdout: stdout_key.is_some(),
        stderr: stderr_key.is_some(),
    };

    let input_temp = if let Some(stdin_key) = stdin_key {
        if resolve_io_file(strand, global, &stdin_key, "r", &mut input).await? {
            true
        } else {
            stdin_key.iter(strand, Slot::reborrow(&mut input)).await?;
            false
        }
    } else {
        strand.input(Slot::reborrow(&mut input));
        false
    };

    let output_temp = if let Some(stdout_key) = stdout_key {
        if resolve_io_file(strand, global, &stdout_key, "w", &mut output).await? {
            true
        } else {
            stdout_key.sink(strand, Slot::reborrow(&mut output)).await?;
            false
        }
    } else {
        strand.output(Slot::reborrow(&mut output));
        false
    };

    let stderr_temp = if let Some(stderr_key) = stderr_key {
        if let Some(sym) = stderr_key.as_sym(strand)
            && sym == global.syms.stdout_redirect
        {
            Output::set(strand, &mut stderr, &output);
            false
        } else if resolve_io_file(strand, global, &stderr_key, "w", &mut stderr).await? {
            true
        } else {
            stderr_key.sink(strand, Slot::reborrow(&mut stderr)).await?;
            false
        }
    } else {
        // Left nil: an unnamed stderr follows the ambient console, which `run`
        // resolves to either inheriting fd 2 or a byte pump into the console.
        false
    };

    Ok(ResolvedIo {
        args: rest,
        temp: Streams {
            stdin: input_temp,
            stdout: output_temp,
            stderr: stderr_temp,
        },
        explicit,
        policy: policy_key,
        mode,
    })
}

async fn resolve_io_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    arg: &Value<'v>,
    mode: &str,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    let Ok(path) = path_from_value(strand, global, arg) else {
        return Ok(false);
    };

    let file = file::open(strand, global, path.to_path(), mode).await?;
    let (file, annex) = File::create(strand, global, file, mode);
    global
        .types
        .file
        .create_with_annex(strand, file, annex, out);
    Ok(true)
}

async fn cleanup_io<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Streams<&Value<'v>>,
    temp: Streams<bool>,
) {
    strand
        .with_interrupt_mask(InterruptMask::all(), async move |strand| {
            strand
                .with_slots(async move |strand, [mut tmp]| {
                    for (temp, value) in [
                        (temp.stdin, value.stdin),
                        (temp.stdout, value.stdout),
                        (temp.stderr, value.stderr),
                    ] {
                        if temp {
                            let _ = method!(strand, value, global.syms.close, &mut tmp).await;
                        }
                    }
                })
                .await
        })
        .await
}

async fn configure_negotiated_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command<'_>,
    input: &Value<'v>,
) -> Result<'v, 's, Option<RecvGuard>> {
    let recv_result = pipe_channel::negotiate_recv(input, strand, global).await?;
    if let Some(guard) = recv_result {
        let pipe = guard.recv_pipe().await.into_sys(strand)?;
        command.stdin(pipe).into_sys(strand)?;
        Ok(Some(guard))
    } else {
        Ok(None)
    }
}

async fn configure_negotiated_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command<'_>,
    output: &Value<'v>,
) -> Result<'v, 's, Option<SendGuard>> {
    let send_result = pipe_channel::negotiate_send(output, strand, global).await?;
    if let Some(guard) = send_result {
        let pipe = guard.send_pipe().await.into_sys(strand)?;
        command.stdout(pipe).into_sys(strand)?;
        Ok(Some(guard))
    } else {
        Ok(None)
    }
}

async fn configure_direct_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command<'_>,
    input: &Value<'v>,
) -> Result<'v, 's, bool> {
    if input.is_nil() || input.eq(strand, Singleton::Null) {
        command.stdin_null();
        return Ok(true);
    }
    if global.types.stdin.cast(input).is_some() {
        command.stdin_inherit().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.cast(input) {
        let stdio = file
            .enter(strand, async |strand, inst| {
                File::command_recv(inst, strand).await
            })
            .await?;
        if let Some(stdio) = stdio {
            command.stdin(stdio).into_sys(strand)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `value` is an unredirected default for standard output: either the
/// literal stream (`shell.stdout`, bound when stdout is not a terminal) or the
/// terminal-following handle (`term.default`, bound when it is). Either way,
/// nothing has redirected this stream, which is what licenses falling through
/// to raw fd inheritance instead of a value-framed pump.
fn is_default_stdout<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> bool {
    global.types.stdout.cast(value).is_some() || dolang_ext_term::is_default_output(strand, value)
}

async fn configure_direct_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command<'_>,
    output: &Value<'v>,
) -> Result<'v, 's, bool> {
    if output.is_nil() || output.eq(strand, Singleton::Null) {
        command.stdout_null();
        return Ok(true);
    }
    if is_default_stdout(strand, global, output) {
        command.stdout_inherit().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.cast(output) {
        let stdio = file
            .enter(strand, async |strand, inst| {
                File::command_send(inst, strand).await
            })
            .await?;
        if let Some(stdio) = stdio {
            command.stdout(stdio).into_sys(strand)?;
            return Ok(true);
        }
    }
    Ok(false)
}

async fn configure_direct_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command<'_>,
    stderr: &Value<'v>,
) -> Result<'v, 's, bool> {
    if stderr.is_nil() || stderr.eq(strand, Singleton::Null) {
        command.stderr_null();
        return Ok(true);
    }
    if is_default_stdout(strand, global, stderr) {
        command.stderr_inherit_stdout().into_sys(strand)?;
        return Ok(true);
    }
    if global.types.stderr.cast(stderr).is_some() {
        command.stderr_inherit().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.cast(stderr) {
        let stdio = file
            .enter(strand, async |strand, inst| {
                File::command_send(inst, strand).await
            })
            .await?;
        if let Some(stdio) = stdio {
            command.stderr(stdio).into_sys(strand)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn apply_env_and_cwd<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &Strand<'v, 's>,
    command: &mut Command<'_>,
) {
    let local = global.local.get(strand);
    local.env().visit(&mut |k, v| {
        if let Some(v) = v {
            command.env(k, v);
        } else {
            command.env_remove(k);
        }
    });
    command.current_dir(local.cwd().to_path());
}

fn apply_args<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    command: &mut Command<'_>,
) -> Result<'v, 's, ()> {
    for arg in args {
        match arg {
            Arg::Pos(slot) => {
                command.arg(slot.to_verbatim(strand)?.as_str());
            }
            Arg::Key(sym, _) => {
                return Err(Error::unexpected_key(strand, sym));
            }
        }
    }
    Ok(())
}

async fn input_pump<'v, 's, W>(
    strand: &mut Strand<'v, 's>,
    input: &Value<'v>,
    mut writer: W,
) -> Result<'v, 's, ()>
where
    W: AsyncWrite + Unpin,
{
    strand
        .with_slots(async move |strand, [mut inval]| {
            while input.next(strand, &mut inval).await? {
                let bytes = encode_value(strand, &inval)?;
                writer.write_all(&bytes).await.into_sys(strand)?;
            }
            Ok(())
        })
        .await
}

/// Where a child's output is being pumped.
#[derive(Clone, Copy)]
enum PumpTarget<'v, 'a> {
    /// A Do sink. Bytes are quantized into values per the redirect's `mode:`,
    /// losslessly either way — a sink that wants terminators gone asks with
    /// `chomp`.
    Sink(&'a Value<'v>),
    /// The console, because an unnamed channel is following an extension that
    /// has taken the terminal over.
    ///
    /// A byte-to-byte edge: the child emits bytes and the console consumes
    /// bytes, so no framing applies in either direction — nothing is quantized
    /// into lines, nothing is required to be valid UTF-8, and no line ending is
    /// added or translated.
    Console,
}

/// Copies a child's output straight to the console.
///
/// Deliberately not `tokio::io::copy`: the console writer is shared with
/// `term.echo`/`print` and diagnostics, so the lock is reacquired per chunk
/// rather than held for the child's entire lifetime. Terminal owners already
/// cope with arbitrary chunk boundaries — the progress writer buffers a partial
/// line and coalesces it with the next newline.
async fn console_pump<'v, 's, R>(strand: &mut Strand<'v, 's>, mut reader: R) -> Result<'v, 's, ()>
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let read = reader.read(&mut buf).await.into_sys(strand)?;
        if read == 0 {
            break;
        }
        dolang_ext_term::write(strand, &buf[..read]).await?;
    }
    Ok(())
}

async fn output_pump<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    output: &Value<'v>,
    reader: R,
    io_mode: IoMode,
) -> Result<'v, 's, ()>
where
    R: AsyncRead + Unpin,
{
    let global = strand.vm().state::<Global<'v>>();
    if let Some(capture) = global.types.capture.cast(output) {
        let mut reader = reader;
        let mut value = String::new();
        reader.read_to_string(&mut value).await.into_sys(strand)?;
        return capture.enter_sync(strand, |strand, capture| {
            capture.borrow_mut(strand)?.append(&value);
            Ok(())
        });
    }
    strand
        .with_slots(async move |strand, [mut outval]| {
            let mut reader = BufReader::new(reader);
            while read_value(&mut reader, io_mode, strand, &mut outval)
                .await
                .into_sys(strand)?
            {
                output.put(strand, &mut outval).await?;
            }
            Ok(())
        })
        .await
}

/// The parent ends of whatever pipes were created for the child.
///
/// A stream is `None` when it was wired up directly — inherited, negotiated, or
/// handed a file descriptor — and so needs no pump.
#[derive(Default)]
struct Pipes {
    stdin: Option<Box<dyn AsyncWrite + Unpin>>,
    stdout: Option<Box<dyn AsyncRead + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Unpin>>,
}

/// Where each pumped stream is going.
struct PumpTargets<'v, 'a> {
    stdin: &'a Value<'v>,
    stdout: PumpTarget<'v, 'a>,
    /// Framing for whichever of the above are sinks.
    mode: IoMode,
    /// `None` when stderr is inherited or merged into stdout, so nothing pumps
    /// it.
    stderr: Option<PumpTarget<'v, 'a>>,
}

/// Runs input/output pumps and waits for process completion with unified error handling.
async fn run_monitor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    process: &mut dolang_vfs::process::Child,
    name: &str,
    target: PumpTargets<'v, '_>,
    pipes: Pipes,
) -> Result<'v, 's, ()> {
    let (res, ires, ores, eres) = {
        // Create pumps
        let ipump = match pipes.stdin {
            None => MaybeDone::Done(Ok(())),
            Some(writer) => {
                let input = target.stdin;
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| {
                    input_pump(strand, input, writer).await
                }))
            }
        };

        let opump = match pipes.stdout {
            None => MaybeDone::Done(Ok(())),
            Some(reader) => {
                let output = target.stdout;
                let mode = target.mode;
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader, mode).await,
                    PumpTarget::Console => console_pump(strand, reader).await,
                }))
            }
        };

        let epump = match (target.stderr, pipes.stderr) {
            (Some(output), Some(reader)) => {
                let mode = target.mode;
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader, mode).await,
                    PumpTarget::Console => console_pump(strand, reader).await,
                }))
            }
            _ => MaybeDone::Done(Ok(())),
        };

        // Wait for completion
        let mut res = None;
        let mut idone = false;
        let mut odone = false;
        let mut edone = false;

        let wait = process.wait();
        tokio::pin!(wait);
        tokio::pin!(ipump);
        tokio::pin!(opump);
        tokio::pin!(epump);
        // Wait for everything to complete
        while res.is_none() || !idone || !odone || !edone {
            tokio::select! {
                biased;

                status = &mut wait, if res.is_none() => {
                    res = Some(status);
                    // Don't wait for input pump any longer, it might be stuck trying to receive on the
                    // iterator and hasn't noticed that the pipe was closed by the process
                    // exiting.
                    idone = true;
                }
                () = (&mut ipump), if !idone => idone = true,
                () = (&mut opump), if !odone => odone = true,
                () = (&mut epump), if !edone => edone = true,
            }
        }

        (
            res.unwrap(),
            ipump.take_output(),
            opump.take_output(),
            epump.take_output(),
        )
    };
    // Check results
    let res = res.into_sys(strand)?;
    if res.success() {
        // Check pump results if they exited, but don't block as they could be stuck on a pending
        // iterator/sink receive/send. They'll get canceled on scope exit in this case.
        if let Some(res) = ires {
            res?;
        }
        if let Some(res) = ores {
            res?;
        }
        if let Some(res) = eres {
            // Check results
            res?;
        }
        Ok(())
    } else {
        Err(error::proc_status_error(strand, name, res))
    }
}

struct RunIo<'v, 'a> {
    /// What each standard stream is connected to. Nil stderr means it was left
    /// unnamed, which `run` resolves against the ambient console.
    value: Streams<&'a Value<'v>>,
    /// Whether the caller named each stream explicitly.
    ///
    /// An unnamed stream is anonymous and follows the ambient console; a named
    /// one is pinned to exactly what it names, which is how `stdout:
    /// $shell.stdout` opts out of terminal takeover.
    explicit: Streams<bool>,
    policy_override: Option<Slot<'v, 'a>>,
    /// Framing for output streams pumped into a sink.
    mode: IoMode,
}

async fn run<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    args: Args<'v, '_>,
    global: State<'v, Global<'v>>,
    io: RunIo<'v, '_>,
) -> Result<'v, 's, ()> {
    let (vfs, target, background, mut termination_policy) = {
        let local = global.local.get(strand);
        (
            local.vfs(),
            local.target(),
            local.background(),
            local.termination_policy(),
        )
    };
    let operating_system = target.os();
    let program = match operating_system.path_kind() {
        vfs_path::Kind::Unix => vfs_path::Path::unix(name),
        vfs_path::Kind::Windows => vfs_path::Path::windows(name),
    };
    let mut command = vfs.command(program);
    if let Some(policy_override) = io.policy_override {
        termination_policy = parse_policy_dict(
            strand,
            global,
            &policy_override,
            termination_policy,
            operating_system != OperatingSystem::Windows,
        )?;
    }
    if operating_system != OperatingSystem::Windows
        && !termination_policy.signal.is_supported(operating_system)
    {
        return Err(Error::value(
            strand,
            format!(
                "{:?} is not supported by the target operating system",
                termination_policy.signal
            ),
        ));
    }
    command.process_control(if background {
        ProcessControl::Background
    } else {
        ProcessControl::Foreground
    });
    command.termination_policy(vfs_policy(&termination_policy));

    // An unnamed channel that would otherwise land on the terminal follows the
    // console instead, so a child's output does not scribble over an extension
    // that has taken the terminal over. Naming `shell.stdout`/`shell.stderr`
    // explicitly pins the channel to the real stream and opts out.
    let console_owned = global.terminal.redirected.get();
    // A capture routes regardless of whether stdout/stderr is a terminal:
    // gating it on a tty would make capture work interactively and silently
    // not in CI.
    let captured = dolang_ext_term::is_captured(strand);
    let stdout_to_console = !io.explicit.stdout
        && is_default_stdout(strand, global, io.value.stdout)
        && (captured || (console_owned && global.terminal.stdout_is_terminal));
    let stderr_to_console =
        !io.explicit.stderr && (captured || (console_owned && global.terminal.stderr_is_terminal));

    let mut stdin_pipe = None;
    let mut stdout_pipe = None;
    let mut stderr_pipe = None;
    let stderr_inherit = io.value.stderr.is_nil() && !stderr_to_console;
    if stderr_inherit {
        command.stderr_inherit().into_sys(strand)?;
    }
    let stderr_merge = !io.value.stderr.is_nil() && io.value.stderr.eq(strand, io.value.stdout);

    let recv_guard =
        configure_negotiated_input(strand, global, &mut command, io.value.stdin).await?;
    let send_guard =
        configure_negotiated_output(strand, global, &mut command, io.value.stdout).await?;
    let stderr_guard = if stderr_inherit || stderr_merge {
        None
    } else {
        configure_negotiated_output(strand, global, &mut command, io.value.stderr).await?
    };
    // Which streams were satisfied by pipe-channel negotiation and so need no
    // further wiring.
    let negotiated = Streams {
        stdin: recv_guard.is_some(),
        stdout: send_guard.is_some(),
        stderr: stderr_guard.is_some(),
    };
    // The guards must outlive the launch.
    let _recv_guard = recv_guard;
    let _send_guard = send_guard;
    let _stderr_guard = stderr_guard;

    if !negotiated.stdin
        && !configure_direct_input(strand, global, &mut command, io.value.stdin).await?
    {
        let (parent_stdin, child_stdin) = vfs.pipe(None).await.into_sys(strand)?;
        command.stdin(child_stdin).into_sys(strand)?;
        stdin_pipe = Some(parent_stdin);
    }

    let stdout_direct = negotiated.stdout
        || (!stdout_to_console
            && configure_direct_output(strand, global, &mut command, io.value.stdout).await?);
    if !stdout_direct {
        let (child_stdout, parent_stdout) = vfs.pipe(None).await.into_sys(strand)?;
        command.stdout(child_stdout).into_sys(strand)?;
        stdout_pipe = Some(parent_stdout);
    }
    if stderr_merge {
        command.stderr_to_stdout().into_sys(strand)?;
    }

    if !stderr_inherit
        && !stderr_merge
        && !negotiated.stderr
        && (stderr_to_console
            || !configure_direct_stderr(strand, global, &mut command, io.value.stderr).await?)
    {
        let (child_stderr, parent_stderr) = vfs.pipe(None).await.into_sys(strand)?;
        command.stderr(child_stderr).into_sys(strand)?;
        stderr_pipe = Some(parent_stderr);
    }

    apply_env_and_cwd(global, strand, &mut command);
    apply_args(strand, args, &mut command)?;

    let mut proc = command.spawn().await.into_sys(strand)?;
    let pipes = Pipes {
        stdin: stdin_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncWrite + Unpin>),
        stdout: stdout_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncRead + Unpin>),
        stderr: stderr_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncRead + Unpin>),
    };
    let target = PumpTargets {
        stdin: io.value.stdin,
        stdout: if stdout_to_console {
            PumpTarget::Console
        } else {
            PumpTarget::Sink(io.value.stdout)
        },
        mode: io.mode,
        stderr: (!stderr_inherit && !stderr_merge).then_some(if stderr_to_console {
            PumpTarget::Console
        } else {
            PumpTarget::Sink(io.value.stderr)
        }),
    };
    let res = {
        strand
            .interrupt_guard(async |strand| {
                run_monitor(strand, &mut proc, name, target, pipes).await
            })
            .await
    };

    if res.is_err() {
        let _ = strand
            .with_interrupt_mask(InterruptMask::all(), async move |_strand| {
                proc.terminate().await
            })
            .await;
    }

    res
}

async fn dispatch_run<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    args: Args<'v, '_>,
    global: State<'v, Global<'v>>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut input, mut output, mut stderr]| {
            let resolved = resolve_io(
                strand,
                global,
                args,
                Slot::reborrow(&mut input),
                Slot::reborrow(&mut output),
                Slot::reborrow(&mut stderr),
            )
            .await?;

            let value = Streams {
                stdin: &*input,
                stdout: &*output,
                stderr: &*stderr,
            };
            let res = run(
                strand,
                name,
                resolved.args,
                global,
                RunIo {
                    value,
                    explicit: resolved.explicit,
                    policy_override: resolved.policy,
                    mode: resolved.mode,
                },
            )
            .await;
            cleanup_io(strand, global, value, resolved.temp).await;
            res
        })
        .await
}

impl<'v> Object<'v> for Program {
    const NAME: &'v str = "Program";
    const MODULE: &'v str = "proc";
    type Annex = ProgramAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([name], []) = unpack!(strand, args, 1, 0)?;
        let global = strand.state::<Global<'v>>();
        let name = program_name_from_value(strand, global, &name)?;
        this.create_with_annex(strand, Program, ProgramAnnex { name, global }, out);
        Ok(())
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        _: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.annex();
        let global = borrow.global;
        let name = borrow.name.clone();
        dispatch_run(strand, &name, args, global).await
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method("which", async move |this, strand, _args, out| {
            let borrow = this.annex();
            let global = borrow.global;
            let name = &borrow.name;
            let (vfs, paths, cwd) = {
                let local = global.local.get(strand);
                let env = local.env();
                (
                    local.vfs(),
                    env.get("PATH").as_deref().map(ToOwned::to_owned),
                    local.cwd().clone(),
                )
            };

            let resolved = vfs
                .which(
                    vfs_path::Path::new(name, cwd.kind()),
                    paths.as_deref(),
                    Some(cwd.to_path()),
                )
                .await
                .into_sys(strand)?;

            if let Some(path) = resolved {
                let annex = PathAnnex::try_new(strand, path, global)?;
                create_path_annex(strand, annex, out);
            } else {
                Output::set(strand, out, Nil);
            }
            Ok(())
        })
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<proc.Program {:?}>", this.annex().name)
    }
}

pub(crate) struct Run<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Run<'v> {
    pub(crate) fn new(global: State<'v, Global<'v>>) -> Self {
        Self { global }
    }

    fn get(&self, strand: &mut Strand<'v, '_>, name: String, out: Slot<'v, '_>) {
        self.global.types.program.create_with_annex(
            strand,
            Program,
            ProgramAnnex {
                name,
                global: self.global,
            },
            out,
        );
    }
}

impl<'v> Object<'v> for Run<'v> {
    const NAME: &'v str = "run";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        let name = program_name_from_value(strand, global, index)?;
        this.borrow(strand)?.get(strand, name, out);
        Ok(())
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        let ([name], [], args) = unpack!(strand, args, 1, 0, ...)?;
        let name = program_name_from_value(strand, global, &name)?;
        dispatch_run(strand, &name, args, global).await
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if unpack.exhaustive() {
            return Err(Error::value(
                strand,
                "proc.run unpacking requires a trailing `...`",
            ));
        }

        let global = this.borrow(strand)?.global;
        for item in unpack.iter() {
            match item {
                UnpackItem::SymKey { key, slot, .. } => {
                    Run { global }.get(strand, key.as_str(strand.vm()).to_string(), slot);
                }
                UnpackItem::ConstKey { key, slot, .. } => {
                    let name = key.as_str(strand).ok_or_else(|| {
                        Error::type_error(strand, "proc.run unpack keys must be strings or symbols")
                    })?;
                    Run { global }.get(strand, name.to_string(), slot);
                }
                UnpackItem::Rest { slot } => Output::set(strand, slot, this),
                UnpackItem::Pos { .. } => {
                    return Err(Error::value(
                        strand,
                        "proc.run supports only keyed unpack patterns",
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn register_run_type<'v>(
    builder: &mut Builder<'v>,
) -> dolang::runtime::Type<'v, Run<'v>> {
    builder.register_type()
}
use dolang::runtime::value::fmt::Format;
