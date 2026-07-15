use std::fmt;

use futures::future::MaybeDone;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Value,
    error::ResultExt as _,
    method,
    object::TypeBuilder,
    unpack,
    value::{Nil, Singleton},
    vm::Builder,
};
use dolang_shell_vfs::{AnyVfs, Child as _, Command, Utf8TypedPath, Vfs};

use crate::{
    error::{self, ResultExt as _},
    fs::{
        file::{self, File},
        path::{PathAnnex, create_path_annex, path_from_value},
    },
    global::Global,
    local::ChannelMode,
    pipe_channel::{self, RecvGuard, SendGuard},
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
    if global.types.unix_path.downcast(value).is_some()
        || global.types.windows_path.downcast(value).is_some()
    {
        let path = path_from_value(strand, global, value)?;
        let path = if path.is_absolute() {
            path
        } else {
            global.local.get(strand).cwd().join(path.as_str())
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
            .create(strand, crate::shell::Stderr, &mut stderr);
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
    let Ok(path) = path_from_value(strand, global, arg) else {
        return Ok(false);
    };

    let file = file::open(strand, global, path.to_path(), mode).await?;
    let (file, annex) = File::create(global, file, mode.contains('b'));
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
        .with_interrupt_mask(true, async move |strand| {
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

async fn configure_negotiated_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl Command<StdioRecv = StdioRecv>,
    input: &Value<'v>,
) -> Result<'v, 's, Option<RecvGuard>> {
    let recv_result = pipe_channel::negotiate_recv(input, strand, global).await?;
    if let Some(guard) = recv_result {
        let pipe = guard.recv_pipe().into_sys(strand)?;
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
        let pipe = guard.send_pipe().into_sys(strand)?;
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
    if global.types.stdin.downcast(input).is_some() {
        command.stdin_inherit().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(input)
        && let Some(stdio) = File::command_recv(file, strand).await?
    {
        command.stdin(stdio).into_sys(strand)?;
        return Ok(true);
    }
    Ok(false)
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
    if global.types.stdout.downcast(output).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stdout_inherit().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(output)
        && let Some(stdio) = File::command_send(file, strand).await?
    {
        command.stdout(stdio).into_sys(strand)?;
        return Ok(true);
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
    if global.types.stdout.downcast(stderr).is_some() {
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            return Ok(false);
        }
        command.stderr_inherit_stdout().into_sys(strand)?;
        return Ok(true);
    }
    if let Some(file) = global.types.file.downcast(stderr)
        && let Some(stdio) = File::command_send(file, strand).await?
    {
        command.stderr(stdio).into_sys(strand)?;
        return Ok(true);
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
    let channel_mode = strand
        .vm()
        .state::<Global<'v>>()
        .local
        .get(strand)
        .channel_mode();
    strand
        .with_slots(async move |strand, [mut inval]| {
            while input.next(strand, &mut inval).await? {
                match channel_mode {
                    ChannelMode::Line => {
                        if let Some(str) = inval.as_str(strand) {
                            writer
                                .write_all(str.pin().as_bytes())
                                .await
                                .into_sys(strand)?;
                            writer.write_all(b"\n").await.into_sys(strand)?;
                        } else if let Some(bin) = inval.as_bin(strand) {
                            writer.write_all(&bin.pin()).await.into_sys(strand)?;
                        } else {
                            let s = inval.to_arg(strand)?;
                            writer.write_all(s.as_bytes()).await.into_sys(strand)?;
                            writer.write_all(b"\n").await.into_sys(strand)?;
                        }
                    }
                    ChannelMode::Chunk => {
                        if let Some(str) = inval.as_str(strand) {
                            writer
                                .write_all(str.pin().as_bytes())
                                .await
                                .into_sys(strand)?;
                        } else if let Some(bin) = inval.as_bin(strand) {
                            writer.write_all(&bin.pin()).await.into_sys(strand)?;
                        } else {
                            let s = inval.to_arg(strand)?;
                            writer.write_all(s.as_bytes()).await.into_sys(strand)?;
                        }
                    }
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
    R: AsyncRead + Unpin,
{
    let channel_mode = strand
        .vm()
        .state::<Global<'v>>()
        .local
        .get(strand)
        .channel_mode();
    strand
        .with_slots(async move |strand, [mut outval]| {
            match channel_mode {
                ChannelMode::Line => {
                    let mut reader = BufReader::new(reader);
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.into_sys(strand)? != 0 {
                        Output::set(strand, &mut outval, line.trim_end_matches(['\r', '\n']));
                        line.clear();
                        output.put(strand, &mut outval).await?;
                    }
                }
                ChannelMode::Chunk => {
                    let mut buf = [0u8; 8192];
                    loop {
                        let n = reader.read(&mut buf).await.into_sys(strand)?;
                        if n == 0 {
                            break;
                        }
                        Output::set(strand, &mut outval, &buf[..n]);
                        output.put(strand, &mut outval).await?;
                    }
                }
            }
            Ok(())
        })
        .await
}

/// Runs input/output pumps and waits for process completion with unified error handling.
#[expect(clippy::too_many_arguments)]
async fn run_monitor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    process: &mut impl dolang_shell_vfs::Child,
    name: &str,
    input: &Value<'v>,
    output: &Value<'v>,
    stderr_output: Option<&Value<'v>>,
    stdin: Option<Box<dyn AsyncWrite + Unpin>>,
    stdout: Option<Box<dyn AsyncRead + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Unpin>>,
) -> Result<'v, 's, ()> {
    let (res, ires, ores, eres) = {
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

fn configure_default_stderr<'v, 's>(
    _strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    command: &mut impl Command,
) -> Result<'v, 's, bool> {
    if global.terminal.redirected.get() {
        return Ok(false);
    }
    command.stderr_inherit().into_sys(_strand)?;
    Ok(true)
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
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let program = match local.target().operating_system.path_type() {
        dolang_shell_vfs::PathType::Unix => {
            Utf8TypedPath::Unix(dolang_shell_vfs::Utf8UnixPath::new(name))
        }
        dolang_shell_vfs::PathType::Windows => {
            Utf8TypedPath::Windows(dolang_shell_vfs::Utf8WindowsPath::new(name))
        }
    };
    let mut command = vfs.command(program);
    configure_default_stderr(strand, global, &mut command)?;
    let mut stdin_pipe = None;
    let mut stdout_pipe = None;
    let mut stderr_pipe = None;
    let stderr_inherit = stderr.is_nil();
    let stderr_merge = !stderr_inherit && stderr.eq(strand, output);

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

    if !stdin_negotiated && !configure_direct_input(strand, global, &mut command, input).await? {
        let (parent_stdin, child_stdin) = vfs.pipe().into_sys(strand)?;
        command.stdin(child_stdin).into_sys(strand)?;
        stdin_pipe = Some(parent_stdin);
    }

    let stdout_direct =
        stdout_negotiated || configure_direct_output(strand, global, &mut command, output).await?;
    if stderr_merge {
        if stdout_negotiated {
            command
                .stderr(send_guard.as_ref().unwrap().send_pipe().into_sys(strand)?)
                .into_sys(strand)?;
        } else if stdout_direct {
            if output.is_nil() || output.eq(strand, Singleton::IterNull) {
                command.stderr_null();
            } else if global.types.stdout.downcast(output).is_some() {
                command.stderr_inherit_stdout().into_sys(strand)?;
            } else {
                if let Some(file) = global.types.file.downcast(output) {
                    command
                        .stderr(File::command_send(file, strand).await?.unwrap())
                        .into_sys(strand)?;
                } else {
                    unreachable!("stdout direct path should have been direct-fd capable")
                }
            }
        } else {
            let (child_stdout, parent_stdout) = vfs.pipe().into_sys(strand)?;
            let child_stderr = child_stdout.try_clone().into_sys(strand)?;
            command.stdout(child_stdout).into_sys(strand)?;
            command.stderr(child_stderr).into_sys(strand)?;
            stdout_pipe = Some(parent_stdout);
        }
    } else if !stdout_direct {
        let (child_stdout, parent_stdout) = vfs.pipe().into_sys(strand)?;
        command.stdout(child_stdout).into_sys(strand)?;
        stdout_pipe = Some(parent_stdout);
    }

    if !stderr_inherit
        && !stderr_merge
        && !stderr_negotiated
        && !configure_direct_stderr(strand, global, &mut command, stderr).await?
    {
        let (child_stderr, parent_stderr) = vfs.pipe().into_sys(strand)?;
        command.stderr(child_stderr).into_sys(strand)?;
        stderr_pipe = Some(parent_stderr);
    }

    apply_env_and_cwd(global, strand, &mut command);
    apply_args(strand, args, &mut command)?;

    let mut proc = command.spawn().await.into_sys(strand)?;
    let stdin = stdin_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncWrite + Unpin>);
    let stdout = stdout_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncRead + Unpin>);
    let stderr_pipe = stderr_pipe.map(|pipe| Box::new(pipe) as Box<dyn AsyncRead + Unpin>);
    let res = {
        strand
            .interrupt_guard(async |strand| {
                run_monitor(
                    strand,
                    &mut proc,
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
            let (args, cleanup) = resolve_io(
                strand,
                global,
                args,
                Slot::reborrow(&mut input),
                Slot::reborrow(&mut output),
                Slot::reborrow(&mut stderr),
            )
            .await?;

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
                            Utf8TypedPath::Unix(dolang_shell_vfs::Utf8UnixPath::new(name))
                        }
                        Utf8TypedPath::Windows(_) => {
                            Utf8TypedPath::Windows(dolang_shell_vfs::Utf8WindowsPath::new(name))
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<proc.Program {:?}>", this.annex().name).into_do(strand)
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
