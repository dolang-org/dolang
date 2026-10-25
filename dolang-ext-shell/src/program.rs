use dolang::runtime::strand::InterruptMask;
use dolang_vfs::{
    Vfs,
    file::File as VfsFile,
    process::{ChildIn, ChildOut, Command, PipeRecv, PipeSend},
};
use futures::future::MaybeDone;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::compat::{FuturesAsyncReadCompatExt as _, FuturesAsyncWriteCompatExt as _};

use dolang::runtime::object::fmt;

use dolang::runtime::{
    AllocExt, Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    error::ErrorKind,
    io::{self, IoCaps},
    method,
    object::TypeBuilder,
    unpack,
    value::{Nil, Singleton, TypeObject},
    vm::Register,
};
use dolang_vfs::path as vfs_path;
use dolang_vfs::{process::ProcessControl, target::OperatingSystem};

use crate::{
    error::{self, ResultExt as _},
    fs::{
        file,
        path::{cast_path, create_path, path_from_value},
    },
    global::{FsTag, ProcGlobal, ProcTag, ShellTag, Tag},
    io_mode::{IoMode, encode_value, read_value},
    pipe_channel::{self, RecvGuard, SendGuard},
    proc::{parse_policy_dict, vfs_policy},
};

pub(crate) struct Program;

pub(crate) struct ProgramAnnex<'v> {
    name: String,
    global: State<'v, ProcGlobal<'v>>,
}

fn program_name_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, String> {
    if let Some(path) = cast_path(strand, value) {
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
    /// How a pumped output stream is quantized into values.
    mode: IoMode,
}

/// Parses a launch's reserved keywords, storing what each standard stream
/// names in `input`, `output` and `stderr`.
///
/// A path is opened as a file. An unnamed stdin or stdout is the ambient one,
/// and an unnamed stderr is left nil. How each stream reaches the child is
/// decided later, by the `connect_*` functions.
async fn resolve_io<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
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

    let input_temp = match stdin_key {
        Some(key) => resolve_io_value(strand, &key, "r", &mut input).await?,
        None => {
            strand.input(Slot::reborrow(&mut input));
            false
        }
    };

    let output_temp = match stdout_key {
        Some(key) => resolve_io_value(strand, &key, "w", &mut output).await?,
        None => {
            strand.output(Slot::reborrow(&mut output));
            false
        }
    };

    let stderr_temp = match stderr_key {
        Some(key)
            if key
                .as_sym(strand)
                .is_some_and(|sym| sym == global.syms.stdout_redirect) =>
        {
            // The same value as stdout, which `run` merges.
            Output::set(strand, &mut stderr, &output);
            false
        }
        Some(key) => resolve_io_value(strand, &key, "w", &mut stderr).await?,
        // Left nil: an unnamed stderr follows the ambient console, which `run`
        // resolves to either inheriting fd 2 or a byte pump into the console.
        None => false,
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

/// Stores the stream `arg` names in `out`, opening a path with `mode`.
///
/// Returns whether it opened a file, which the launch must close.
async fn resolve_io_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    arg: &Value<'v>,
    mode: &str,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    if resolve_io_file(strand, arg, mode, out).await? {
        return Ok(true);
    }
    Output::set(strand, out, arg);
    Ok(false)
}

async fn resolve_io_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    arg: &Value<'v>,
    mode: &str,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    let Ok(path) = path_from_value(strand, arg) else {
        return Ok(false);
    };

    let fs = strand.force_state::<FsTag>();
    let file = file::open(strand, fs, path.to_path(), mode).await?;
    file::create_file(strand, fs, file, mode, false, out);
    Ok(true)
}

async fn cleanup_io<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
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

/// How [`run`] linked one standard stream to the child.
enum Link<G, P> {
    /// Configured entirely on the command: null, inherited, a file descriptor,
    /// or a stream forwarded to a vfs relay.
    Wired,
    /// A pipe channel negotiated a pipe, whose guard must outlive the launch.
    Negotiated(G),
    /// The parent's end of a pipe of `run`'s own, which a pump serves.
    Pipe(P),
}

impl<G, P> Link<G, P> {
    fn split(self) -> (Option<G>, Option<P>) {
        match self {
            Self::Wired => (None, None),
            Self::Negotiated(guard) => (Some(guard), None),
            Self::Pipe(pipe) => (None, Some(pipe)),
        }
    }
}

/// What a pumped output pipe is copied into.
#[derive(Clone, Copy)]
enum OutTarget {
    /// The stream's value, as a sink.
    Sink,
    /// The console.
    Console,
}

impl OutTarget {
    fn pump<'v, 'a>(self, value: &'a Value<'v>) -> PumpTarget<'v, 'a> {
        match self {
            Self::Sink => PumpTarget::Sink(value),
            Self::Console => PumpTarget::Console,
        }
    }
}

type InLink = Link<RecvGuard, PipeSend>;
type OutLink = Link<SendGuard, (PipeRecv, OutTarget)>;

fn is_null<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, bool> {
    Ok(value.is_nil() || value.eq(strand, Singleton::Null)?)
}

fn is_shell_stdin<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    strand
        .try_state::<ShellTag>()
        .is_some_and(|shell| shell.types.stdin.cast(value).is_some())
}

fn is_shell_stderr<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    strand
        .try_state::<ShellTag>()
        .is_some_and(|shell| shell.types.stderr.cast(value).is_some())
}

/// Whether `value` is an unredirected default for standard output: either the
/// literal stream (`shell.stdout`, bound when stdout is not a terminal) or the
/// terminal-following handle (`term.default`, bound when it is). Either way,
/// nothing has redirected this stream, which is what licenses falling through
/// to raw fd inheritance instead of a value-framed pump.
fn is_default_stdout<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> bool {
    strand
        .try_state::<ShellTag>()
        .is_some_and(|shell| shell.types.stdout.cast(value).is_some())
        || dolang_ext_term::is_default_output(strand, value)
}

/// Returns `value`'s file to hand to the child as is.
async fn command_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Option<VfsFile>> {
    let Some(file) = strand
        .try_state::<FsTag>()
        .and_then(|fs| file::cast_file(fs, value))
    else {
        return Ok(None);
    };
    file.command_file(strand).await.map(Some)
}

/// Returns `value`'s byte handle, for a vfs relay to copy, if it provides one.
///
/// It must have `caps`, or `message` is raised.
fn byte_handle<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    caps: IoCaps,
    message: &'static str,
) -> Result<'v, 's, Option<io::ByteHandle>> {
    let handle = io::get(strand, value, |strand, have| {
        if have.contains(caps) {
            Ok(())
        } else {
            Err(Error::value(strand, message))
        }
    });
    match handle {
        Ok(handle) => Ok(Some(handle)),
        Err(err) if err.kind() == ErrorKind::Unsupported => Ok(None),
        Err(err) => Err(err),
    }
}

/// Returns `value`'s reader, if it is a stream.
fn forward_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Option<ChildIn>> {
    const MESSAGE: &str = "stdin must be readable";
    let Some(handle) = byte_handle(strand, value, IoCaps::READ, MESSAGE)? else {
        return Ok(None);
    };
    let reader = handle
        .into_read()
        .map_err(|_| Error::value(strand, MESSAGE))?;
    Ok(Some(ChildIn::reader(reader.compat())))
}

/// Returns `value`'s writer, if it is a stream.
fn forward_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    message: &'static str,
) -> Result<'v, 's, Option<ChildOut>> {
    let Some(handle) = byte_handle(strand, value, IoCaps::WRITE, message)? else {
        return Ok(None);
    };
    let writer = handle
        .into_write()
        .map_err(|_| Error::value(strand, message))?;
    Ok(Some(ChildOut::writer(writer.compat_write())))
}

/// Replaces `slot`'s value with its iterator.
async fn make_iter<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut iter]| {
            slot.iter(strand, Slot::reborrow(&mut iter)).await?;
            Output::set(strand, slot, &*iter);
            Ok(())
        })
        .await
}

/// Replaces `slot`'s value with its sink.
async fn make_sink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut sink]| {
            slot.sink(strand, Slot::reborrow(&mut sink)).await?;
            Output::set(strand, slot, &*sink);
            Ok(())
        })
        .await
}

/// Connects the child's stdin to `input`.
///
/// In order: null, the shell's own stdin, a file's descriptor, a stream
/// forwarded to a vfs relay, a negotiated pipe channel, and finally a pipe
/// that `input_pump` fills from `input`'s items. A pipe channel is checked
/// before streams, since it negotiates a better pipe, and so is a file, which
/// gives the child an exact position. `input` is left holding its iterator
/// when one is needed.
async fn connect_stdin<'v, 's>(
    strand: &mut Strand<'v, 's>,
    vfs: &Vfs,
    command: &mut Command<'_>,
    input: &mut Slot<'v, '_>,
) -> Result<'v, 's, InLink> {
    if is_null(strand, input)? {
        command.stdin_null();
        return Ok(Link::Wired);
    }
    if is_shell_stdin(strand, input) {
        command.stdin_inherit().into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if let Some(file) = command_file(strand, input).await? {
        command.stdin(file).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if !pipe_channel::is_receiver(strand, input)
        && let Some(reader) = forward_input(strand, input)?
    {
        command.stdin(reader).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    make_iter(strand, input).await?;
    if let Some(guard) = pipe_channel::negotiate_recv(input, strand).await? {
        let pipe = guard.recv_pipe().await.into_sys(strand)?;
        command.stdin(pipe).into_sys(strand)?;
        return Ok(Link::Negotiated(guard));
    }
    let (parent, child) = vfs.pipe(None).await.into_sys(strand)?;
    command.stdin(child).into_sys(strand)?;
    Ok(Link::Pipe(parent))
}

/// Connects the child's stdout to `output`, or to the console when
/// `to_console`.
///
/// The order mirrors [`connect_stdin`], with the defaults for stdout and
/// stderr inherited. A pumped `output` is left holding its sink.
async fn connect_stdout<'v, 's>(
    strand: &mut Strand<'v, 's>,
    vfs: &Vfs,
    command: &mut Command<'_>,
    output: &mut Slot<'v, '_>,
    to_console: bool,
) -> Result<'v, 's, OutLink> {
    if to_console {
        let (child, parent) = vfs.pipe(None).await.into_sys(strand)?;
        command.stdout(child).into_sys(strand)?;
        return Ok(Link::Pipe((parent, OutTarget::Console)));
    }
    if is_null(strand, output)? {
        command.stdout_null();
        return Ok(Link::Wired);
    }
    if is_default_stdout(strand, output) {
        command.stdout_inherit().into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if is_shell_stderr(strand, output) {
        command.stdout_inherit_stderr().into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if let Some(file) = command_file(strand, output).await? {
        command.stdout(file).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if !pipe_channel::is_sender(strand, output)
        && let Some(writer) = forward_output(strand, output, "stdout must be writable")?
    {
        command.stdout(writer).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    make_sink(strand, output).await?;
    if let Some(guard) = pipe_channel::negotiate_send(output, strand).await? {
        let pipe = guard.send_pipe().await.into_sys(strand)?;
        command.stdout(pipe).into_sys(strand)?;
        return Ok(Link::Negotiated(guard));
    }
    let (child, parent) = vfs.pipe(None).await.into_sys(strand)?;
    command.stdout(child).into_sys(strand)?;
    Ok(Link::Pipe((parent, OutTarget::Sink)))
}

/// Connects the child's stderr to a named `stderr`, which isn't stdout's.
///
/// The order matches [`connect_stdout`]'s.
async fn connect_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    vfs: &Vfs,
    command: &mut Command<'_>,
    stderr: &mut Slot<'v, '_>,
) -> Result<'v, 's, OutLink> {
    if is_null(strand, stderr)? {
        command.stderr_null();
        return Ok(Link::Wired);
    }
    if is_default_stdout(strand, stderr) {
        command.stderr_inherit_stdout().into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if is_shell_stderr(strand, stderr) {
        command.stderr_inherit().into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if let Some(file) = command_file(strand, stderr).await? {
        command.stderr(file).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    if !pipe_channel::is_sender(strand, stderr)
        && let Some(writer) = forward_output(strand, stderr, "stderr must be writable")?
    {
        command.stderr(writer).into_sys(strand)?;
        return Ok(Link::Wired);
    }
    make_sink(strand, stderr).await?;
    if let Some(guard) = pipe_channel::negotiate_send(stderr, strand).await? {
        let pipe = guard.send_pipe().await.into_sys(strand)?;
        command.stderr(pipe).into_sys(strand)?;
        return Ok(Link::Negotiated(guard));
    }
    let (child, parent) = vfs.pipe(None).await.into_sys(strand)?;
    command.stderr(child).into_sys(strand)?;
    Ok(Link::Pipe((parent, OutTarget::Sink)))
}

fn apply_env_and_cwd<'v, 's>(
    global: State<'v, ProcGlobal<'v>>,
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
    let global = strand.vm().state::<ProcTag>();
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

/// The parent ends of whatever pipes `run` created for the child, with what
/// each is pumped from or into.
///
/// A stream is `None` when it was wired up otherwise — inherited, negotiated,
/// handed a file descriptor, or forwarded to a vfs relay — and so needs no
/// pump.
struct Pumps<'v, 'a> {
    stdin: Option<(PipeSend, &'a Value<'v>)>,
    stdout: Option<(PipeRecv, PumpTarget<'v, 'a>)>,
    stderr: Option<(PipeRecv, PumpTarget<'v, 'a>)>,
    /// Framing for whichever outputs are pumped into sinks.
    mode: IoMode,
}

/// Runs input/output pumps and waits for process completion with unified error handling.
async fn run_monitor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    process: &mut dolang_vfs::process::Child,
    name: &str,
    pumps: Pumps<'v, '_>,
) -> Result<'v, 's, ()> {
    let (res, ires, ores, eres) = {
        let mode = pumps.mode;
        // Create pumps
        let ipump = match pumps.stdin {
            None => MaybeDone::Done(Ok(())),
            Some((writer, input)) => {
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| {
                    input_pump(strand, input, writer).await
                }))
            }
        };

        let opump = match pumps.stdout {
            None => MaybeDone::Done(Ok(())),
            Some((reader, output)) => {
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader, mode).await,
                    PumpTarget::Console => console_pump(strand, reader).await,
                }))
            }
        };

        let epump = match pumps.stderr {
            None => MaybeDone::Done(Ok(())),
            Some((reader, output)) => {
                MaybeDone::Future(strand.spawn_scoped(None, async move |strand| match output {
                    PumpTarget::Sink(output) => output_pump(strand, output, reader, mode).await,
                    PumpTarget::Console => console_pump(strand, reader).await,
                }))
            }
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
            res?;
        }
        // So does a stream a vfs relay copied.
        if let Some(error) = process.relay_error() {
            return Err(io::error(strand, error));
        }
        Ok(())
    } else {
        Err(error::proc_status_error(strand, name, res))
    }
}

struct RunIo<'v, 'a> {
    /// What each standard stream names. Nil stderr means it was left unnamed,
    /// which `run` resolves against the ambient console.
    ///
    /// `run` replaces a pumped stream's value with its iterator or sink.
    streams: Streams<Slot<'v, 'a>>,
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
    global: State<'v, ProcGlobal<'v>>,
    io: RunIo<'v, '_>,
) -> Result<'v, 's, ()> {
    let RunIo {
        streams:
            Streams {
                stdin: mut input,
                stdout: mut output,
                mut stderr,
            },
        explicit,
        policy_override,
        mode,
    } = io;
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
    if let Some(policy_override) = policy_override {
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
    let core = strand.state::<Tag>();
    let console_owned = core.terminal.redirected.get();
    // A capture routes regardless of whether stdout/stderr is a terminal:
    // gating it on a tty would make capture work interactively and silently
    // not in CI.
    let captured = dolang_ext_term::is_captured(strand);
    let stdout_to_console = !explicit.stdout
        && is_default_stdout(strand, &output)
        && (captured || (console_owned && core.terminal.stdout_is_terminal));
    let stderr_to_console = captured || (console_owned && core.terminal.stderr_is_terminal);
    // Compared before stdout's value is replaced by its sink.
    let stderr_merge = explicit.stderr && stderr.eq(strand, &output)?;

    let stdin_link = connect_stdin(strand, &vfs, &mut command, &mut input).await?;
    let stdout_link =
        connect_stdout(strand, &vfs, &mut command, &mut output, stdout_to_console).await?;
    let stderr_link = if stderr_merge {
        command.stderr_to_stdout().into_sys(strand)?;
        Link::Wired
    } else if explicit.stderr {
        connect_stderr(strand, &vfs, &mut command, &mut stderr).await?
    } else if stderr_to_console {
        let (child, parent) = vfs.pipe(None).await.into_sys(strand)?;
        command.stderr(child).into_sys(strand)?;
        Link::Pipe((parent, OutTarget::Console))
    } else {
        command.stderr_inherit().into_sys(strand)?;
        Link::Wired
    };
    // Negotiated guards must outlive the launch.
    let (_stdin_guard, stdin_pipe) = stdin_link.split();
    let (_stdout_guard, stdout_pipe) = stdout_link.split();
    let (_stderr_guard, stderr_pipe) = stderr_link.split();

    apply_env_and_cwd(global, strand, &mut command);
    apply_args(strand, args, &mut command)?;

    let mut proc = command.spawn().await.into_sys(strand)?;
    let pumps = Pumps {
        stdin: stdin_pipe.map(|pipe| (pipe, &*input)),
        stdout: stdout_pipe.map(|(pipe, target)| (pipe, target.pump(&output))),
        stderr: stderr_pipe.map(|(pipe, target)| (pipe, target.pump(&stderr))),
        mode,
    };
    let res = {
        strand
            .interrupt_guard(async |strand| run_monitor(strand, &mut proc, name, pumps).await)
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
    global: State<'v, ProcGlobal<'v>>,
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

            let res = run(
                strand,
                name,
                resolved.args,
                global,
                RunIo {
                    streams: Streams {
                        stdin: Slot::reborrow(&mut input),
                        stdout: Slot::reborrow(&mut output),
                        stderr: Slot::reborrow(&mut stderr),
                    },
                    explicit: resolved.explicit,
                    policy_override: resolved.policy,
                    mode: resolved.mode,
                },
            )
            .await;
            // A file `run` opened is never replaced by an iterator or sink.
            let value = Streams {
                stdin: &*input,
                stdout: &*output,
                stderr: &*stderr,
            };
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
        let global = strand.state::<ProcTag>();
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
        builder.supertype(TypeObject::Func).method(
            "which",
            async move |this, strand, _args, out| {
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
                    let fs = strand.force_state::<FsTag>();
                    create_path(strand, fs, path, out)?;
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            },
        )
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
    global: State<'v, ProcGlobal<'v>>,
}

impl<'v> Run<'v> {
    pub(crate) fn new(global: State<'v, ProcGlobal<'v>>) -> Self {
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
}

pub(crate) fn register_run_type<'v>(
    builder: &mut Register<'v>,
) -> dolang::runtime::Type<'v, Run<'v>> {
    builder.register_type()
}
use dolang::runtime::value::fmt::Format;
