use std::{
    fmt::{self},
    future::Future,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};

use futures::future::MaybeDone;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Value,
    error::ResultExt as _,
    method,
    object::TypeBuilder,
    unpack,
    value::{Nil, Singleton},
    vm::Builder,
};

#[cfg(unix)]
use crate::container::Context;
use crate::pipe_channel::{self, RecvGuard, SendGuard};
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use tokio::net::unix::pipe;

use crate::error::{self, ErrorExt as _, ResultExt as _};
use crate::fs::{
    file::{self, File},
    path::{PathAnnex, PathOrStr},
};
use crate::global::Global;

pub(crate) struct Program;

pub(crate) struct ProgramAnnex<'v> {
    name: String,
    global: State<'v, Global<'v>>,
}

/// Trait for command builders that support common configuration operations.
pub trait CommandBuilder {
    /// Add a command-line argument.
    fn arg(&mut self, arg: &str) -> &mut Self;

    /// Set an environment variable.
    fn env(&mut self, key: &str, val: &str) -> &mut Self;

    /// Remove an environment variable.
    fn env_remove(&mut self, key: &str) -> &mut Self;

    /// Set the working directory.
    fn current_dir(&mut self, dir: &Path) -> &mut Self;

    #[cfg(unix)]
    /// Redirect stdin from the given file descriptor.
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self;

    #[cfg(unix)]
    /// Redirect stdout to the given file descriptor.
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self;

    /// Redirect stdin from the null device.
    fn stdin_null(&mut self) -> &mut Self;

    /// Redirect stdout to the null device.
    fn stdout_null(&mut self) -> &mut Self;

    #[cfg(unix)]
    /// Redirect stderr to the given file descriptor.
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self;

    /// Redirect stderr to the null device.
    fn stderr_null(&mut self) -> &mut Self;
}

impl CommandBuilder for Command {
    fn arg(&mut self, arg: &str) -> &mut Self {
        Command::arg(self, arg)
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        Command::env(self, key, val)
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        Command::env_remove(self, key)
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        Command::current_dir(self, dir)
    }

    #[cfg(unix)]
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self {
        Command::stdin(self, Stdio::from(fd))
    }

    #[cfg(unix)]
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self {
        Command::stdout(self, Stdio::from(fd))
    }

    fn stdin_null(&mut self) -> &mut Self {
        Command::stdin(self, Stdio::null())
    }

    fn stdout_null(&mut self) -> &mut Self {
        Command::stdout(self, Stdio::null())
    }

    #[cfg(unix)]
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self {
        Command::stderr(self, Stdio::from(fd))
    }

    fn stderr_null(&mut self) -> &mut Self {
        Command::stderr(self, Stdio::null())
    }
}

#[cfg(unix)]
impl CommandBuilder for dolang_shell_vfs::CommandBuilder<'_> {
    fn arg(&mut self, arg: &str) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::arg(self, arg)
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::env(self, key, val)
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::env_remove(self, key)
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::current_dir(self, dir)
    }

    #[cfg(unix)]
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stdin(self, fd)
    }

    #[cfg(unix)]
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stdout(self, fd)
    }

    fn stdin_null(&mut self) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stdin_null(self)
    }

    fn stdout_null(&mut self) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stdout_null(self)
    }

    #[cfg(unix)]
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stderr(self, fd)
    }

    fn stderr_null(&mut self) -> &mut Self {
        dolang_shell_vfs::CommandBuilder::stderr_null(self)
    }
}

async fn resolve_io<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: Args<'v, 'a>,
    mut input: Slot<'v, '_>,
    mut output: Slot<'v, '_>,
    mut stderr: Slot<'v, '_>,
) -> Result<'v, 's, (Args<'v, 'a>, [bool; 3])> {
    let stdin_sym = global.syms.stdin;
    let stdout_sym = global.syms.stdout;
    let stderr_sym = global.syms.stderr;
    let ([], [stdin_key, stdout_key, stderr_key], rest) = unpack!(
        strand,
        args,
        0,
        0,
        stdin_sym = None,
        stdout_sym = None,
        stderr_sym = None,
        ...
    )?;
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
    } else if global.terminal.redirected.get() {
        // Terminal is redirected — pipe child stderr through the Stderr sink
        // so it goes through the redirect writer instead of directly to stderr fd
        global
            .types
            .stderr
            .create(strand, crate::sys::Stderr, &mut stderr);
        false
    } else {
        false
    };

    Ok((rest, [input_temp, output_temp, stderr_temp]))
}

async fn resolve_io_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    arg: &Value<'v>,
    mode: &str,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    let Ok(path) = PathOrStr::new(strand, global, arg) else {
        return Ok(false);
    };

    let file = file::open(strand, global, path.as_ref(), mode)
        .await
        .into_sys(strand)?;
    let (file, annex) = File::create(global, file, mode.contains('b'), path.as_ref().to_owned());
    global
        .types
        .file
        .create_with_annex(strand, file, annex, out);
    Ok(true)
}

async fn cleanup_io<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    input: &Value<'v>,
    output: &Value<'v>,
    stderr: &Value<'v>,
    cleanup: [bool; 3],
) {
    strand
        .with_cancel_mask(true, async move |strand| {
            strand
                .with_slots(async move |strand, [mut tmp]| {
                    if cleanup[0] {
                        let _ = method!(strand, input, global.syms.close, &mut tmp).await;
                    }
                    if cleanup[1] {
                        let _ = method!(strand, output, global.syms.close, &mut tmp).await;
                    }
                    if cleanup[2] {
                        let _ = method!(strand, stderr, global.syms.close, &mut tmp).await;
                    }
                })
                .await
        })
        .await
}

#[cfg_attr(not(unix), allow(dead_code))]
async fn configure_negotiated_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
    input: &Value<'v>,
) -> Result<'v, 's, Option<RecvGuard>> {
    #[cfg(unix)]
    {
        let recv_result = pipe_channel::negotiate_recv(input, strand, global).await?;
        if let Some(guard) = recv_result {
            let fd = guard.fd().into_sys(strand)?;
            command.stdin_fd(fd);
            Ok(Some(guard))
        } else {
            Ok(None)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        pipe_channel::negotiate_recv(input, strand, global).await
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
async fn configure_negotiated_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
    output: &Value<'v>,
) -> Result<'v, 's, Option<SendGuard>> {
    #[cfg(unix)]
    {
        let send_result = pipe_channel::negotiate_send(output, strand, global).await?;
        if let Some(guard) = send_result {
            let fd = guard.fd().into_sys(strand)?;
            command.stdout_fd(fd);
            Ok(Some(guard))
        } else {
            Ok(None)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        pipe_channel::negotiate_send(output, strand, global).await
    }
}

#[cfg(unix)]
fn configure_direct_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
    input: &Value<'v>,
) -> Result<'v, 's, bool> {
    if input.is_nil() || input.eq(strand, Singleton::IterNull) {
        command.stdin_null();
        return Ok(true);
    }
    if global.types.stdin.downcast(input).is_some() {
        command.stdin_fd(
            std::io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .into_sys(strand)?,
        );
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(input)
        && let Some(fd) = File::fd(file, strand)?
    {
        command.stdin_fd(fd);
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(unix))]
fn configure_direct_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command,
    input: &Value<'v>,
) -> Result<'v, 's, bool> {
    if input.is_nil() || input.eq(strand, Singleton::IterNull) {
        command.stdin_null();
        return Ok(true);
    }
    if global.types.stdin.downcast(input).is_some() {
        command.stdin(Stdio::inherit());
        return Ok(true);
    }
    Ok(false)
}

#[cfg(unix)]
fn configure_direct_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
    output: &Value<'v>,
) -> Result<'v, 's, bool> {
    if output.is_nil() || output.eq(strand, Singleton::IterNull) {
        command.stdout_null();
        return Ok(true);
    }
    if global.types.stdout.downcast(output).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stdout_fd(
            std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .into_sys(strand)?,
        );
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(output)
        && let Some(fd) = File::fd(file, strand)?
    {
        command.stdout_fd(fd);
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(unix))]
fn configure_direct_output<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command,
    output: &Value<'v>,
) -> Result<'v, 's, bool> {
    if output.is_nil() || output.eq(strand, Singleton::IterNull) {
        command.stdout_null();
        return Ok(true);
    }
    if global.types.stdout.downcast(output).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stdout(Stdio::inherit());
        return Ok(true);
    }
    Ok(false)
}

#[cfg(unix)]
fn configure_direct_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
    stderr: &Value<'v>,
) -> Result<'v, 's, bool> {
    if stderr.is_nil() || stderr.eq(strand, Singleton::IterNull) {
        command.stderr_null();
        return Ok(true);
    }
    if global.types.stdout.downcast(stderr).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stderr_fd(
            std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .into_sys(strand)?,
        );
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(stderr)
        && let Some(fd) = File::fd(file, strand)?
    {
        command.stderr_fd(fd);
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(unix))]
fn configure_direct_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut Command,
    stderr: &Value<'v>,
) -> Result<'v, 's, bool> {
    if stderr.is_nil() || stderr.eq(strand, Singleton::IterNull) {
        command.stderr_null();
        return Ok(true);
    }
    if global.types.stdout.downcast(stderr).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stderr(Stdio::inherit());
        return Ok(true);
    }
    Ok(false)
}

fn apply_env_and_cwd<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &Strand<'v, 's>,
    command: &mut impl CommandBuilder,
) {
    let local = global.local.get(strand);
    local.env().visit(&mut |k, v| {
        if let Some(v) = v {
            command.env(k, v);
        } else {
            command.env_remove(k);
        }
    });
    command.current_dir((*local.cwd()).as_ref());
}

fn apply_args<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    command: &mut impl CommandBuilder,
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
    strand
        .with_slots(async move |strand, [mut inval]| {
            while input.next(strand, &mut inval).await? {
                if let Some(str) = inval.as_str(strand) {
                    writer.write_all(str.as_bytes()).await.into_sys(strand)?;
                    writer.write_all(b"\n").await.into_sys(strand)?;
                } else if let Some(slice) = inval.as_u8_slice(strand) {
                    writer.write_all(slice).await.into_sys(strand)?;
                } else {
                    let s = inval.to_arg(strand)?;
                    writer.write_all(s.as_bytes()).await.into_sys(strand)?;
                    writer.write_all(b"\n").await.into_sys(strand)?;
                }
            }
            Ok(())
        })
        .await
}

async fn output_pump<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    output: &Value<'v>,
    mut reader: R,
) -> Result<'v, 's, ()>
where
    R: AsyncBufRead + Unpin,
{
    strand
        .with_slots(async move |strand, [mut outval]| {
            let mut line = String::new();
            while reader.read_line(&mut line).await.into_sys(strand)? != 0 {
                Output::set(strand, &mut outval, line.trim_end_matches(['\r', '\n']));
                line.clear();
                output.put(strand, &mut outval).await?;
            }
            Ok(())
        })
        .await
}

/// Runs input/output pumps and waits for process completion with unified error handling.
#[expect(clippy::too_many_arguments)]
async fn run_monitor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    process: impl Future<Output = io::Result<ExitStatus>>,
    name: &str,
    input: &Value<'v>,
    output: &Value<'v>,
    stderr_output: Option<&Value<'v>>,
    stdin: Option<impl AsyncWrite + Unpin>,
    stdout: Option<impl AsyncBufRead + Unpin>,
    stderr: Option<impl AsyncBufRead + Unpin>,
) -> Result<'v, 's, ()> {
    // Create pumps
    let ipump = match stdin {
        None => MaybeDone::Done(Ok(())),
        Some(writer) => MaybeDone::Future(strand.spawn_scoped(None, async move |strand| {
            input_pump(strand, input, writer).await
        })),
    };

    let opump = match stdout {
        None => MaybeDone::Done(Ok(())),
        Some(reader) => MaybeDone::Future(strand.spawn_scoped(None, async move |strand| {
            output_pump(strand, output, reader).await
        })),
    };

    let epump = match (stderr_output, stderr) {
        (Some(output), Some(reader)) => {
            MaybeDone::Future(strand.spawn_scoped(None, async move |strand| {
                output_pump(strand, output, reader).await
            }))
        }
        _ => MaybeDone::Done(Ok(())),
    };

    // Wait for completion
    let mut res = None;
    let mut idone = false;
    let mut odone = false;
    let mut edone = false;

    tokio::pin!(ipump);
    tokio::pin!(opump);
    tokio::pin!(epump);
    tokio::pin!(process);

    // Wait for everything to complete
    while res.is_none() || !idone || !odone || !edone {
        tokio::select! {
            biased;

            status = (&mut process), if res.is_none() => {
                res = Some(status.into_sys(strand)?);
                // Don't wait for input pump any longer, it might be stuck trying to receive on the
                // input iterator and hasn't noticed that the pipe was closed by the process
                // exiting.
                idone = true;
            }
            () = (&mut ipump), if !idone => idone = true,
            () = (&mut opump), if !odone => odone = true,
            () = (&mut epump), if !edone => edone = true,
        }
    }

    // Check results
    let res = res.unwrap();
    if res.success() {
        // Check pump results if they exited, but don't block as they could be stuck on a pending
        // input/output iterator receive/send.  They'll get canceled on scope exit in this case.
        if let Some(res) = ipump.take_output() {
            res?;
        }
        if let Some(res) = opump.take_output() {
            res?;
        }
        if let Some(res) = epump.take_output() {
            res?;
        }
        Ok(())
    } else {
        #[cfg(unix)]
        if res.signal() == Some(libc::SIGPIPE) {
            return Err(Error::sink_stop(strand));
        }

        Err(error::proc_status_error(strand, name, res))
    }
}

fn resolve_program<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &Strand<'v, 's>,
    name: &str,
) -> Option<PathBuf> {
    let local = global.local.get(strand);
    let env = local.env();
    let paths = env.get("PATH");
    let cwd = local.cwd();
    which::which_in(name, paths.as_deref(), cwd.as_ref()).ok()
}

#[cfg(unix)]
fn configure_default_stderr<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl CommandBuilder,
) -> Result<'v, 's, bool> {
    if global.terminal.redirected.get() {
        return Ok(false);
    }
    command.stderr_fd(
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .into_sys(strand)?,
    );
    Ok(true)
}

#[cfg(unix)]
#[expect(clippy::too_many_arguments)]
async fn run_container<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    args: Args<'v, '_>,
    global: State<'v, Global<'v>>,
    context: &Context,
    input: &Value<'v>,
    output: &Value<'v>,
    stderr: &Value<'v>,
) -> Result<'v, 's, ()> {
    let mut command = context.client().command(name);
    let mut stdin_pipe = None;
    let mut stdout_pipe = None;
    let mut stderr_pipe = None;
    let stderr_inherit = stderr.is_nil();
    let stderr_merge = !stderr_inherit && stderr.eq(strand, output);

    configure_default_stderr(strand, global, &mut command)?;

    let recv_guard = configure_negotiated_input(strand, global, &mut command, input).await?;
    let send_guard = configure_negotiated_output(strand, global, &mut command, output).await?;
    let stderr_guard = if stderr_inherit || stderr_merge {
        None
    } else {
        configure_negotiated_output(strand, global, &mut command, stderr).await?
    };

    if recv_guard.is_none() && !configure_direct_input(strand, global, &mut command, input)? {
        let (sender, receiver) = pipe::pipe().into_sys(strand)?;
        command.stdin_fd(receiver.into_blocking_fd().into_sys(strand)?);
        stdin_pipe = Some(sender);
    }

    let stdout_direct =
        send_guard.is_some() || configure_direct_output(strand, global, &mut command, output)?;
    if stderr_merge {
        if let Some(guard) = send_guard.as_ref() {
            command.stderr_fd(guard.fd().into_sys(strand)?);
        } else if stdout_direct {
            if output.is_nil() || output.eq(strand, Singleton::IterNull) {
                command.stderr_null();
            } else if global.types.stdout.downcast(output).is_some() {
                command.stderr_fd(
                    std::io::stdout()
                        .as_fd()
                        .try_clone_to_owned()
                        .into_sys(strand)?,
                );
            } else if let Some(file) = global.types.file.downcast(output)
                && let Some(fd) = File::fd(file, strand)?
            {
                command.stderr_fd(fd);
            } else {
                unreachable!("stdout direct path should have been direct-fd capable")
            }
        } else {
            let (sender, receiver) = pipe::pipe().into_sys(strand)?;
            let stdout_fd = sender.into_blocking_fd().into_sys(strand)?;
            let stderr_fd = stdout_fd.as_fd().try_clone_to_owned().into_sys(strand)?;
            command.stdout_fd(stdout_fd);
            command.stderr_fd(stderr_fd);
            stdout_pipe = Some(receiver);
        }
    } else if !stdout_direct {
        let (sender, receiver) = pipe::pipe().into_sys(strand)?;
        command.stdout_fd(sender.into_blocking_fd().into_sys(strand)?);
        stdout_pipe = Some(receiver);
    }

    if !stderr_inherit
        && !stderr_merge
        && stderr_guard.is_none()
        && !configure_direct_stderr(strand, global, &mut command, stderr)?
    {
        let (sender, receiver) = pipe::pipe().into_sys(strand)?;
        command.stderr_fd(sender.into_blocking_fd().into_sys(strand)?);
        stderr_pipe = Some(receiver);
    }

    apply_env_and_cwd(global, strand, &mut command);
    apply_args(strand, args, &mut command)?;

    let proc = command.status();

    run_monitor(
        strand,
        proc,
        name,
        input,
        output,
        (!stderr_inherit && !stderr_merge).then_some(stderr),
        stdin_pipe,
        stdout_pipe.map(BufReader::new),
        stderr_pipe.map(BufReader::new),
    )
    .await
}

async fn run<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &str,
    args: Args<'v, '_>,
    global: State<'v, Global<'v>>,
    input: &Value<'v>,
    output: &Value<'v>,
    stderr: &Value<'v>,
) -> Result<'v, 's, ()> {
    let path = resolve_program(global, strand, name).ok_or_else(|| {
        #[cfg(unix)]
        let err = io::Error::from_raw_os_error(libc::ENOENT);
        #[cfg(not(unix))]
        let err = io::Error::new(
            io::ErrorKind::NotFound,
            format!("program not found: {}", name),
        );
        err.into_sys(strand)
    })?;
    let mut command = Command::new(&path);
    if !global.terminal.redirected.get() {
        command.stderr(Stdio::inherit());
    }
    #[cfg(unix)]
    let mut stdout_pipe = None;
    let stderr_inherit = stderr.is_nil();
    let stderr_merge = !stderr_inherit && stderr.eq(strand, output);

    #[cfg(unix)]
    let (
        stdin_negotiated,
        stdout_negotiated,
        stderr_negotiated,
        _recv_guard,
        send_guard,
        _stderr_guard,
    ) = {
        let recv_guard = configure_negotiated_input(strand, global, &mut command, input).await?;
        let send_guard = configure_negotiated_output(strand, global, &mut command, output).await?;
        let stderr_guard = if stderr_inherit || stderr_merge {
            None
        } else {
            configure_negotiated_output(strand, global, &mut command, stderr).await?
        };
        let sn_in = recv_guard.is_some();
        let sn_out = send_guard.is_some();
        let sn_err = stderr_guard.is_some();
        (sn_in, sn_out, sn_err, recv_guard, send_guard, stderr_guard)
    };
    #[cfg(not(unix))]
    let (stdin_negotiated, stdout_negotiated, stderr_negotiated) = (false, false, false);

    if !stdin_negotiated && !configure_direct_input(strand, global, &mut command, input)? {
        command.stdin(Stdio::piped());
    }

    let stdout_direct =
        stdout_negotiated || configure_direct_output(strand, global, &mut command, output)?;
    #[cfg(unix)]
    if stderr_merge {
        if stdout_negotiated {
            command.stderr_fd(send_guard.as_ref().unwrap().fd().into_sys(strand)?);
        } else if stdout_direct {
            if output.is_nil() || output.eq(strand, Singleton::IterNull) {
                command.stderr(Stdio::null());
            } else if global.types.stdout.downcast(output).is_some() {
                command.stderr(Stdio::inherit());
            } else if let Some(file) = global.types.file.downcast(output) {
                command.stderr(Stdio::from(File::fd(file, strand)?.unwrap()));
            } else {
                unreachable!("stdout direct path should have been direct-fd capable")
            }
        } else {
            let (sender, receiver) = pipe::pipe().into_sys(strand)?;
            let stdout_fd = sender.into_blocking_fd().into_sys(strand)?;
            let stderr_fd = stdout_fd.as_fd().try_clone_to_owned().into_sys(strand)?;
            command.stdout(Stdio::from(stdout_fd));
            command.stderr(Stdio::from(stderr_fd));
            stdout_pipe = Some(receiver);
        }
    } else if !stdout_direct {
        command.stdout(Stdio::piped());
    }
    #[cfg(not(unix))]
    if stderr_merge {
        if stdout_direct {
            if output.is_nil() || output.eq(strand, Singleton::IterNull) {
                command.stderr(Stdio::null());
            } else if global.types.stdout.downcast(output).is_some() {
                command.stderr(Stdio::inherit());
            } else {
                command.stderr(Stdio::inherit());
            }
        } else {
            command.stdout(Stdio::piped());
            command.stderr(Stdio::inherit());
        }
    } else if !stdout_direct {
        command.stdout(Stdio::piped());
    }

    if !stderr_inherit
        && !stderr_merge
        && !stderr_negotiated
        && !configure_direct_stderr(strand, global, &mut command, stderr)?
    {
        command.stderr(Stdio::piped());
    }

    apply_env_and_cwd(global, strand, &mut command);
    apply_args(strand, args, &mut command)?;

    let mut proc = command.spawn().into_sys(strand)?;

    let stdin = proc.stdin.take();
    #[cfg(unix)]
    let stdout = match stdout_pipe {
        Some(stdout_pipe) => {
            Some(Box::new(BufReader::new(stdout_pipe)) as Box<dyn AsyncBufRead + Unpin>)
        }
        None => proc
            .stdout
            .take()
            .map(|stdout| Box::new(BufReader::new(stdout)) as Box<dyn AsyncBufRead + Unpin>),
    };
    #[cfg(not(unix))]
    let stdout = proc
        .stdout
        .take()
        .map(|stdout| Box::new(BufReader::new(stdout)) as Box<dyn AsyncBufRead + Unpin>);
    let stderr_pipe = proc
        .stderr
        .take()
        .map(|stderr| Box::new(BufReader::new(stderr)) as Box<dyn AsyncBufRead + Unpin>);
    let wait = proc.wait();

    let res = strand
        .cancel_guard(async move |strand| {
            run_monitor(
                strand,
                wait,
                name,
                input,
                output,
                (!stderr_inherit && !stderr_merge).then_some(stderr),
                stdin,
                stdout,
                stderr_pipe,
            )
            .await
        })
        .await;

    // Perform final cleanup of the process
    if res.is_err()
        && let Some(_pid) = proc.id()
    {
        // Avoid being dropped while we await process
        let _ = strand
            .with_cancel_mask(true, async move |_strand| {
                #[cfg(unix)]
                {
                    use nix::{
                        sys::signal::{self, Signal},
                        unistd::Pid,
                    };
                    use std::time::Duration;
                    use tokio::time::timeout;

                    signal::kill(Pid::from_raw(_pid as i32), Signal::SIGTERM).into_do(_strand)?;
                    let res = timeout(Duration::from_millis(500), proc.wait()).await;
                    if res.is_ok() {
                        Ok(())
                    } else {
                        proc.kill().await.into_sys(_strand)
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = proc.kill().await;
                }
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
            let (args, cleanup) = resolve_io(
                strand,
                global,
                args,
                Slot::reborrow(&mut input),
                Slot::reborrow(&mut output),
                Slot::reborrow(&mut stderr),
            )
            .await?;

            #[cfg(unix)]
            {
                let local = global.local.get(strand);
                let container = local.container();
                let handle = container.as_ref().cloned();
                drop(container);
                if let Some(handle) = handle {
                    let res = run_container(
                        strand, name, args, global, &handle, &input, &output, &stderr,
                    )
                    .await;
                    cleanup_io(strand, global, &input, &output, &stderr, cleanup).await;
                    return res;
                }
            }

            let res = run(strand, name, args, global, &input, &output, &stderr).await;
            cleanup_io(strand, global, &input, &output, &stderr, cleanup).await;
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
            let local = global.local.get(strand);
            let env = local.env();
            let paths = env.get("PATH");
            let cwd = local.cwd();

            let resolved = {
                #[cfg(unix)]
                {
                    if let Some(handle) = local.container().as_ref() {
                        handle
                            .client()
                            .which(name, paths.as_deref(), Some(cwd.as_ref()))
                            .await
                            .into_sys(strand)?
                    } else {
                        which::which_in(name, paths.as_deref(), cwd.as_ref()).ok()
                    }
                }
                #[cfg(not(unix))]
                {
                    which::which_in(name, paths.as_deref(), cwd.as_ref()).ok()
                }
            };

            if let Some(path) = resolved {
                global.types.path.create_with_annex(
                    strand,
                    crate::fs::path::Path,
                    PathAnnex::new(path, global),
                    out,
                );
            } else {
                Output::set(strand, out, Nil);
            }
            Ok(())
        })
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<proc.Program {:?}>", this.annex().name).into_do(strand)
    }
}

struct Run<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Run<'v> {
    fn get(&self, strand: &Strand<'v, '_>, name: &str, out: Slot<'v, '_>) {
        self.global.types.program.create_with_annex(
            strand,
            Program,
            ProgramAnnex {
                name: name.to_string(),
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
        this.borrow(strand)?.get(strand, field.as_str(strand), out);
        Ok(())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        this.borrow(strand)?.get(
            strand,
            index.as_str(strand).ok_or_else(|| Error::index(strand))?,
            out,
        );
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
        let name = name
            .as_str(strand)
            .ok_or_else(|| Error::type_error(strand, "program must be a string"))?
            .to_string();
        dispatch_run(strand, &name, args, global).await
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let factory_ty = builder.register_type::<Run>();

    builder.module_object("proc.run", &factory_ty, Run { global });
}
