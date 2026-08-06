use futures::future::MaybeDone;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Value, method,
    object::TypeBuilder,
    unpack,
    value::{Nil, Singleton},
    vm::Builder,
};
use dolang_vfs::{
    AnyVfs, Child as _, Command, OperatingSystem, ProcessControl, Utf8TypedPath, Vfs,
};

use crate::{
    error::{self, ResultExt as _},
    fs::{
        file::{self, File},
        path::{PathAnnex, create_path_annex, path_from_value},
    },
    global::Global,
    io_mode::{ValueEncoding, encode_value, read_value},
    pipe_channel::{self, RecvGuard, SendGuard},
    proc::{parse_policy_dict, vfs_policy},
};

pub(crate) struct Program;

type StdioSend = <AnyVfs as Vfs>::StdioSend;
type StdioRecv = <AnyVfs as Vfs>::StdioRecv;

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
    } else {
        Err(Error::type_error(
            strand,
            "program must be a string or Path",
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
    let ([], [stdin_key, stdout_key, stderr_key, policy_key], rest) = unpack!(
        strand,
        args,
        0,
        0,
        stdin_sym = None,
        stdout_sym = None,
        stderr_sym = None,
        policy_sym = None,
        ...
    )?;
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
            && sym == global.syms.stdout
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
    let (file, annex) = File::create(strand, global, file, mode.contains('b'));
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
        .with_interrupt_mask(true, async move |strand| {
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
    command: &mut impl Command<StdioRecv = StdioRecv>,
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
    command: &mut impl Command<StdioSend = StdioSend>,
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
    command: &mut impl Command<StdioRecv = StdioRecv>,
    input: &Value<'v>,
) -> Result<'v, 's, bool> {
    if input.is_nil() || input.eq(strand, Singleton::IterNull) {
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
fn is_default_stdout<'v>(global: State<'v, Global<'v>>, value: &Value<'v>) -> bool {
    global.types.stdout.cast(value).is_some() || global.types.default.cast(value).is_some()
}

async fn configure_direct_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl Command<StdioSend = StdioSend>,
    output: &Value<'v>,
) -> Result<'v, 's, bool> {
    if output.is_nil() || output.eq(strand, Singleton::IterNull) {
        command.stdout_null();
        return Ok(true);
    }
    if is_default_stdout(global, output) {
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
    command: &mut impl Command<StdioSend = StdioSend>,
    stderr: &Value<'v>,
) -> Result<'v, 's, bool> {
    if stderr.is_nil() || stderr.eq(strand, Singleton::IterNull) {
        command.stderr_null();
        return Ok(true);
    }
    if is_default_stdout(global, stderr) {
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
    command: &mut impl Command,
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
    command: &mut impl Command,
) -> Result<'v, 's, ()> {
    for arg in args {
        match arg {
            Arg::Pos(slot) => {
                command.arg(slot.to_arg(strand)?.as_str());
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
    let local = strand.vm().state::<Global<'v>>().local.get(strand);
    let io_mode = local.io_mode();
    let operating_system = local.target().operating_system;
    strand
        .with_slots(async move |strand, [mut inval]| {
            while input.next(strand, &mut inval).await? {
                let bytes = encode_value(
                    strand,
                    &inval,
                    io_mode,
                    ValueEncoding::Argument,
                    operating_system,
                )?;
                writer.write_all(&bytes).await.into_sys(strand)?;
            }
            Ok(())
        })
        .await
}

/// Where a child's output is being pumped.
#[derive(Clone, Copy)]
enum PumpTarget<'v, 'a> {
    /// A Do sink. Values are framed per the ambient I/O mode.
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
        crate::console::write(strand, &buf[..read]).await?;
    }
    Ok(())
}

async fn output_pump<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    output: &Value<'v>,
    reader: R,
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
    let io_mode = strand
        .vm()
        .state::<Global<'v>>()
        .local
        .get(strand)
        .io_mode();
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
    /// `None` when stderr is inherited or merged into stdout, so nothing pumps
    /// it.
    stderr: Option<PumpTarget<'v, 'a>>,
}

/// Runs input/output pumps and waits for process completion with unified error handling.
async fn run_monitor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    process: &mut impl dolang_vfs::Child,
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
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader).await,
                    PumpTarget::Console => console_pump(strand, reader).await,
                }))
            }
        };

        let epump = match (target.stderr, pipes.stderr) {
            (Some(output), Some(reader)) => {
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader).await,
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
        if res.signal() == Some(13) {
            return Err(Error::sink_stop(strand));
        }

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
    let operating_system = target.operating_system;
    let program = match operating_system.path_type() {
        dolang_vfs::PathType::Unix => Utf8TypedPath::Unix(dolang_vfs::Utf8UnixPath::new(name)),
        dolang_vfs::PathType::Windows => {
            Utf8TypedPath::Windows(dolang_vfs::Utf8WindowsPath::new(name))
        }
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
    let stdout_to_console = !io.explicit.stdout
        && console_owned
        && global.terminal.stdout_is_terminal
        && is_default_stdout(global, io.value.stdout);
    // A capture routes regardless of whether stderr is a terminal: gating it on
    // a tty would make capture work interactively and silently not in CI.
    let captured = !global.capture.slot(strand).is_nil();
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
    // The guards must outlive the launch. `send_guard` is also read below, to
    // duplicate the negotiated stdout pipe when stderr merges into it.
    let _recv_guard = recv_guard;
    let _stderr_guard = stderr_guard;

    if !negotiated.stdin
        && !configure_direct_input(strand, global, &mut command, io.value.stdin).await?
    {
        let (parent_stdin, child_stdin) = vfs.pipe().await.into_sys(strand)?;
        command.stdin(child_stdin).into_sys(strand)?;
        stdin_pipe = Some(parent_stdin);
    }

    let stdout_direct = negotiated.stdout
        || (!stdout_to_console
            && configure_direct_output(strand, global, &mut command, io.value.stdout).await?);
    if stderr_merge {
        if negotiated.stdout {
            command
                .stderr(
                    send_guard
                        .as_ref()
                        .unwrap()
                        .send_pipe()
                        .await
                        .into_sys(strand)?,
                )
                .into_sys(strand)?;
        } else if stdout_direct {
            if io.value.stdout.is_nil() || io.value.stdout.eq(strand, Singleton::IterNull) {
                command.stderr_null();
            } else if is_default_stdout(global, io.value.stdout) {
                command.stderr_inherit_stdout().into_sys(strand)?;
            } else {
                if let Some(file) = global.types.file.cast(io.value.stdout) {
                    let stdio = file
                        .enter(strand, async |strand, inst| {
                            File::command_send(inst, strand).await
                        })
                        .await?
                        .unwrap();
                    command.stderr(stdio).into_sys(strand)?;
                } else {
                    unreachable!("stdout direct path should have been direct-fd capable")
                }
            }
        } else {
            let (child_stdout, parent_stdout) = vfs.pipe().await.into_sys(strand)?;
            let child_stderr = child_stdout.try_clone().await.into_sys(strand)?;
            command.stdout(child_stdout).into_sys(strand)?;
            command.stderr(child_stderr).into_sys(strand)?;
            stdout_pipe = Some(parent_stdout);
        }
    } else if !stdout_direct {
        let (child_stdout, parent_stdout) = vfs.pipe().await.into_sys(strand)?;
        command.stdout(child_stdout).into_sys(strand)?;
        stdout_pipe = Some(parent_stdout);
    }

    if !stderr_inherit
        && !stderr_merge
        && !negotiated.stderr
        && (stderr_to_console
            || !configure_direct_stderr(strand, global, &mut command, io.value.stderr).await?)
    {
        let (child_stderr, parent_stderr) = vfs.pipe().await.into_sys(strand)?;
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
            .with_interrupt_mask(true, async move |_strand| proc.terminate().await)
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
                    match cwd.to_path() {
                        Utf8TypedPath::Unix(_) => {
                            Utf8TypedPath::Unix(dolang_vfs::Utf8UnixPath::new(name))
                        }
                        Utf8TypedPath::Windows(_) => {
                            Utf8TypedPath::Windows(dolang_vfs::Utf8WindowsPath::new(name))
                        }
                    },
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
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<proc.Program {:?}>", this.annex().name)
    }
}

struct Run<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Run<'v> {
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

    fn get<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?
            .get(strand, field.as_str(strand).into(), out);
        Ok(())
    }

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

    async fn method<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        _: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let name = method.as_str(strand.vm());
        let global = this.borrow(strand)?.global;
        dispatch_run(strand, name, args, global).await
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
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let factory_ty = builder.register_type::<Run>();

    builder.module_object("proc.run", &factory_ty, Run { global });
}
