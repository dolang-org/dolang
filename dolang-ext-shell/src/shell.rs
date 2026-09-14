use std::{
    fmt::{self, Debug, Display},
    path::PathBuf,
    process::Command,
};

use dolang::runtime::value::fmt::Format;

use tokio::io::AsyncWriteExt;

use dolang::runtime::object::fmt;

use dolang::{
    compile::Config,
    runtime::{
        AllocExt, Arg, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
        method,
        object::{Mut, Ref, TypeBuilder},
        strand::Redirect,
        unpack,
        value::{AsTuple, Nil, TypeObject},
        vm::Register,
    },
};

use crate::util;
use crate::{
    env::Env as EnvObject,
    error::{ErrorExt, ResultExt as _},
    fs::path::{cast_path, create_path, path_from_value},
    global::{FsGlobal, Global, ProgramSource, ShellGlobal},
    io_mode::{IoMode, encode_value, line_ending, read_raw, read_value, write_raw},
    local::{Local, ProgramOverride},
    pipe_channel,
    shell_args::Args as ShellArgs,
};
use dolang::runtime::value::View;
use dolang_vfs::{
    Vfs as VfsVfs,
    process::{StdioRecv, StdioSend},
};
use std::collections::HashMap;

use crate::error;
use dolang_vfs::path as vfs_path;

/// Exit error.
///
/// The `exit` function propagates an [`Error::abort`] containing
/// an instance of this type as the [`Error::source`](std::error::Error::source) which
/// can be recovered through downcasting.
#[derive(Debug)]
pub struct Exit {
    /// Status code specified to `exit`, or `0` by default.
    pub code: i32,
}

impl Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <Self as Debug>::fmt(self, f)
    }
}

impl std::error::Error for Exit {}

/// Process replacement requested by [`shell.exec`](https://dolang.dev/api/shell/#exec-program-args).
///
/// The abort source owns a snapshot of the resolved program, arguments,
/// working directory, and environment. Embedders may recover it by
/// downcasting [`Error::source`](std::error::Error::source).
#[derive(Debug)]
pub struct Exec {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, Option<String>)>,
}

impl Exec {
    /// Constructs the command to execute.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).current_dir(&self.cwd);
        for (key, value) in &self.env {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }
}

impl Display for Exec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <Self as Debug>::fmt(self, f)
    }
}

impl std::error::Error for Exec {}

/// The process's standard input, exported as `shell.stdin`.
///
/// A handle, not a wrapper: the underlying reader lives in [`Global::stdio`] so
/// that this object and the root strand's implicit input are the same buffered
/// reader. See [`crate::global::Stdio`].
///
/// The handle carries the framing it reads with. `lines()` and `chunks()`
/// return handles onto the same stream that quantize it differently — the
/// handles are distinct, the reader behind them is not, so taking one does not
/// fork or buffer anything. Both framings are lossless.
pub(crate) struct Stdin {
    mode: IoMode,
}

impl Stdin {
    pub(crate) fn new(mode: IoMode) -> Self {
        Self { mode }
    }
}

impl Default for Stdin {
    fn default() -> Self {
        Self::new(IoMode::Line)
    }
}

impl<'v> Object<'v> for Stdin {
    const NAME: &'v str = "Stdin";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .method("read", async move |_this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size = size
                    .map(|size| {
                        size.to_i64(strand)
                            .ok()
                            .and_then(|size| usize::try_from(size).ok())
                            .ok_or_else(|| {
                                Error::type_error(strand, "size must be a non-negative integer")
                            })
                    })
                    .transpose()?;
                let global = strand.state::<Global<'v>>();
                let mut reader = global.stdio.stdin.lock().await;
                read_raw(&mut *reader, size, strand, out).await
            })
            .method("lines", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<ShellGlobal<'v>>();
                global
                    .types
                    .stdin
                    .create(strand, Stdin::new(IoMode::Line), out);
                Ok(())
            })
            .method("chunks", async move |_this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<ShellGlobal<'v>>();
                global
                    .types
                    .stdin
                    .create(strand, Stdin::new(IoMode::Chunk), out);
                Ok(())
            })
    }

    /// All handles share the one reader in [`Global::stdio`], so two are the
    /// same object exactly when they frame it the same way.
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<ShellGlobal<'v>>();
        let Some(other) = global.types.stdin.cast(other) else {
            return Ok(false);
        };
        let mode = this.borrow(strand)?.mode;
        other.enter_sync(strand, |strand, other| {
            Ok(other.borrow(strand)?.mode == mode)
        })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        let mode = this.borrow(strand)?.mode;
        let read = {
            let mut reader = global.stdio.stdin.lock().await;
            read_value(&mut *reader, mode, strand, &mut out)
                .await
                .map_err(|err| err.into_sys(strand))?
        };
        Ok(read)
    }
}

/// The process's standard output, exported as `shell.stdout`.
///
/// Always writes to the real stream. Naming this handle is how you opt out of
/// terminal takeover — use `term.console` to follow it instead.
pub(crate) struct Stdout;

impl Default for Stdout {
    fn default() -> Self {
        Self
    }
}

impl<'v> Object<'v> for Stdout {
    const NAME: &'v str = "Stdout";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let global = strand.state::<Global<'v>>();
                let mut writer = global.stdio.stdout.lock().await;
                write_raw(&mut *writer, data, strand, out).await
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                global
                    .stdio
                    .stdout
                    .lock()
                    .await
                    .flush()
                    .await
                    .map_err(|err| err.into_sys(strand))
            })
    }

    /// All instances share [`Global::stdio`], so any two are interchangeable.
    fn eq<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<ShellGlobal<'v>>();
        Ok(global.types.stdout.cast(other).is_some())
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let bytes = encode_value(strand, &value)?;
        let mut writer = global.stdio.stdout.lock().await;
        writer
            .write_all(&bytes)
            .await
            .map_err(|err| err.into_sys(strand))
    }
}

/// The process's standard error, exported as `shell.stderr`.
///
/// The real stream, unaffected by terminal takeover. For human-readable
/// diagnostics that should follow a progress display, use `term.console`.
pub(crate) struct Stderr;

impl Default for Stderr {
    fn default() -> Self {
        Self
    }
}

impl<'v> Object<'v> for Stderr {
    const NAME: &'v str = "Stderr";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Sink)
            .method("write", async move |_this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let global = strand.state::<Global<'v>>();
                let mut writer = global.stdio.stderr.lock().await;
                write_raw(&mut *writer, data, strand, out).await
            })
            .method("flush", async move |_this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                global
                    .stdio
                    .stderr
                    .lock()
                    .await
                    .flush()
                    .await
                    .map_err(|err| err.into_sys(strand))
            })
    }

    /// All instances share [`Global::stdio`], so any two are interchangeable.
    fn eq<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<ShellGlobal<'v>>();
        Ok(global.types.stderr.cast(other).is_some())
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let bytes = encode_value(strand, &value)?;
        let mut writer = global.stdio.stderr.lock().await;
        writer
            .write_all(&bytes)
            .await
            .map_err(|err| err.into_sys(strand))
    }
}

/// Kernel buffer size requested for the local pipe pair carrying a remote
/// `shell.Vfs` connection's RPC framing. Bigger than the OS default so
/// short reads/writes (and the send-side fragment-size backoff they
/// trigger) are the exception rather than routine.
const REMOTE_VFS_PIPE_BUFFER_SIZE: usize = 1024 * 1024;

/// Negotiates both ends of a `shell.Vfs` stream's pipe channel into real OS
/// pipes. Factored out of `Vfs::new` so the caller can unconditionally
/// clear the pending pipe-buffer-size hint afterward, success or failure.
async fn negotiate_stream_pipes<'v, 's>(
    strand: &mut Strand<'v, 's>,
    input: &Value<'v>,
    output: &Value<'v>,
) -> Result<
    'v,
    's,
    (
        pipe_channel::RecvGuard,
        StdioRecv,
        pipe_channel::SendGuard,
        StdioSend,
    ),
> {
    let recv_guard = pipe_channel::negotiate_recv(input, strand)
        .await?
        .ok_or_else(|| Error::type_error(strand, "Vfs: stream iterator is not a pipe channel"))?;
    // Stolen, not duplicated: the session outlives the pipeline stage that
    // would otherwise close the channel's own descriptors, so a copy left in
    // the channel would keep the pipe open past the server's exit and turn a
    // disconnect into a hang. See `SendGuard::steal_send_pipe`.
    let recv = recv_guard.steal_recv_pipe().into_sys(strand)?;

    let send_guard = pipe_channel::negotiate_send(output, strand)
        .await?
        .ok_or_else(|| Error::type_error(strand, "Vfs: stream sink is not a pipe channel"))?;
    let send = send_guard.steal_send_pipe().into_sys(strand)?;

    Ok((recv_guard, recv, send_guard, send))
}

pub(crate) struct Vfs;

pub(crate) struct VfsAnnex<'v> {
    vfs: VfsVfs,
    source: VfsSource,
    global: State<'v, ShellGlobal<'v>>,
}

enum VfsSource {
    Stream,
    Unix(vfs_path::PathBuf),
    WindowsAdmin,
}

impl<'v> Object<'v> for Vfs {
    const NAME: &'v str = "Vfs";
    const MODULE: &'v str = "shell";
    const SLOTS: usize = 1;
    type Annex = VfsAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match &this.annex().source {
            VfsSource::Stream => fmt!(strand, w, "<shell.Vfs stream>"),
            VfsSource::Unix(socket) => {
                fmt!(strand, w, "<shell.Vfs socket: {socket:?}>")
            }
            VfsSource::WindowsAdmin => {
                fmt!(strand, w, "<shell.Vfs windows admin>")
            }
        }
    }

    async fn new<'a, 's>(
        _this: dolang::runtime::Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: dolang::runtime::Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([callable], []) = unpack!(strand, args, 1, 0)?;
        let global = strand.state::<ShellGlobal<'v>>();
        strand
            .with_slots(
                async move |strand,
                            [mut to_bg_send, mut to_bg_recv, mut from_bg_send, mut from_bg_recv, mut handle, mut tmp]| {
                    // The remote VFS connection's RPC framing runs over these
                    // pipes, so they want a generous kernel buffer to make
                    // short reads/writes (and the fragment-size backoff they
                    // trigger) rare rather than routine. Fixed at
                    // construction time so it can't be forgotten when the
                    // channel is later negotiated into a real OS pipe.
                    pipe_channel::make_pair(
                        strand,
                        Slot::reborrow(&mut to_bg_send),
                        Slot::reborrow(&mut to_bg_recv),
                        Some(REMOTE_VFS_PIPE_BUFFER_SIZE),
                    );
                    pipe_channel::make_pair(
                        strand,
                        Slot::reborrow(&mut from_bg_send),
                        Slot::reborrow(&mut from_bg_recv),
                        Some(REMOTE_VFS_PIPE_BUFFER_SIZE),
                    );

                    // Bundled into a plain tuple so `callable`/the background
                    // strand's input/output stay rooted together as the
                    // single `arg` value `spawn_background` keeps alive until
                    // the background strand starts running.
                    let close_sym = global.syms.close;
                    strand.spawn_background(
                        AsTuple::new([&callable, &to_bg_recv, &from_bg_send]),
                        None,
                        &mut handle,
                        async move |strand, arg, out| {
                            let Some(tuple) = arg.as_tuple(strand) else {
                                unreachable!("spawn_background arg is not a tuple")
                            };
                            strand
                                .with_slots(
                                    async move |strand, [mut callable, mut input, mut output, mut tmp]| {
                                        tuple.get(strand, 0, &mut callable)?;
                                        tuple.get(strand, 1, &mut input)?;
                                        tuple.get(strand, 2, &mut output)?;
                                        let res = Redirect::new(strand)
                                            .input(&input)
                                            .output(&output)
                                            .enter(async move |strand| {
                                                call!(strand, &callable, out).await
                                            })
                                            .await;
                                        // Plain close, no poisoning: these pipe
                                        // ends get negotiated into a raw OS
                                        // pipe before any RPC framing runs
                                        // over them, which loses Do object-
                                        // passing semantics anyway. `Vfs.stop`
                                        // always joins and checks status, and a
                                        // dead server surfaces its own errors
                                        // on the next VFS operation.
                                        let _ = method!(strand, &input, close_sym, &mut tmp).await;
                                        let _ = method!(strand, &output, close_sym, &mut tmp).await;
                                        res
                                    },
                                )
                                .await
                        },
                    )?;

                    let (recv_guard, recv, send_guard, send) =
                        negotiate_stream_pipes(strand, &from_bg_recv, &to_bg_send).await?;

                    let vfs = match VfsVfs::new_split(recv, send).await {
                        Ok(client) => client,
                        Err(negotiate_error) => {
                            let join = global.syms.join;
                            return match method!(strand, &handle, join, &mut tmp).await {
                                Ok(()) => Err(negotiate_error.into_sys(strand)),
                                Err(launcher_error) => Err(launcher_error),
                            };
                        }
                    };
                    drop((recv_guard, send_guard));
                    global.types.vfs.create_with_annex(
                        strand,
                        Vfs,
                        VfsAnnex {
                            vfs,
                            source: VfsSource::Stream,
                            global,
                        },
                        &mut out,
                    );
                    global
                        .types
                        .vfs
                        .cast(&out)
                        .unwrap()
                        .enter_sync(strand, |strand, this| {
                            Output::set(
                                strand,
                                Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                                &handle,
                            );
                        });
                    Ok(())
                },
            )
            .await
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let key_sym = builder.sym("key");
        let elevate_sym = builder.sym("elevate");
        let cd_sym = builder.sym("cd");
        let env_sym = builder.sym("env");

        builder
            .type_method("unix_socket", async move |_this, strand, args, out| {
                let ([path], [key]) = unpack!(strand, args, 1, 0, key_sym = None)?;
                let global = strand.vm().state::<ShellGlobal<'v>>();
                let path = path_from_value(strand, &path)?;
                let key = key
                    .map(|key| bytes_from_value(strand, &key, "key"))
                    .transpose()?;
                let vfs = global.local.get(strand).vfs();
                let vfs = error::io_result(
                    strand,
                    vfs.unix_socket(path.to_path(), key.as_deref()).await,
                )?;
                let source = VfsSource::Unix(path.clone());

                global.types.vfs.create_with_annex(
                    strand,
                    Vfs,
                    VfsAnnex {
                        vfs,
                        source,
                        global,
                    },
                    out,
                );
                Ok(())
            })
            .method("with", async move |this, strand, mut args, out| {
                let func = match args.next() {
                    None => return Err(Error::missing_positional(strand, 0)),
                    Some(Arg::Pos(slot)) => slot,
                    Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                };
                let borrow = this.annex();
                Local::with_vfs(
                    strand,
                    borrow.global.local,
                    borrow.vfs.clone(),
                    async move |strand| func.call(strand, args, out).await,
                )
                .await
            })
            .method("stop", async move |this, strand, _args, _out| {
                // Stopping ends the RPC session too, which releases the stdio
                // pipe handles. Joining always waits for the launcher to observe
                // the helper's exit.
                let annex = this.annex();
                let stop_result = annex.vfs.clone().stop().await;
                let join_result = if matches!(&annex.source, VfsSource::Stream) {
                    strand
                        .with_slots(async move |strand, [mut stream, mut output]| {
                            let borrow = this.borrow(strand)?;
                            Output::set(strand, &mut stream, Ref::slot::<0>(&borrow));
                            drop(borrow);
                            method!(strand, &stream, annex.global.syms.join, &mut output).await
                        })
                        .await
                } else {
                    Ok(())
                };
                match (stop_result, join_result) {
                    // Joining is cleanup, so it must happen even when the
                    // stop request failed. Preserve that request's error when
                    // both operations fail.
                    (Err(error), _) => Err(error.into_sys(strand)),
                    (Ok(()), result) => result,
                }
            })
            .type_method("windows_admin", async move |_this, strand, args, out| {
                let ([], [elevate, cd, env_value]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    elevate_sym = None,
                    cd_sym = None,
                    env_sym = None
                )?;
                let elevate = match elevate {
                    Some(elevate) => util::bool(strand, elevate, "elevate")?,
                    None => true,
                };
                let global = strand.vm().state::<ShellGlobal<'v>>();
                let current_cwd = global.local.get(strand).cwd().clone();
                let cwd = if let Some(cd) = cd {
                    let cd = path_from_value(strand, &cd)?;
                    if cd.is_absolute() {
                        cd
                    } else {
                        current_cwd.join(cd.as_str())
                    }
                } else {
                    current_cwd
                };
                let mut env_overrides = HashMap::new();
                if let Some(env_value) = env_value {
                    let View::Dict(env_value) = env_value.view(strand) else {
                        return Err(Error::type_error(strand, "env: expected Dict"));
                    };
                    let mut pairs = env_value.pairs();
                    strand.with_slots_sync(|strand, [mut key, mut value]| {
                        while pairs.next(strand, &mut key, &mut value)? {
                            let key = match key.view(strand) {
                                View::Str(key) => key.to_string(),
                                View::Sym(key) => key.as_str(strand).to_string(),
                                _ => {
                                    return Err(Error::type_error(
                                        strand,
                                        "env key: expected Str or Sym",
                                    ));
                                }
                            };
                            let value = if value.is_nil() {
                                None
                            } else if value.as_sym(strand) == Some(global.syms.inherit) {
                                global
                                    .local
                                    .get(strand)
                                    .env()
                                    .get(&key)
                                    .map(|value| value.into_owned())
                            } else {
                                Some(value.to_string(strand)?)
                            };
                            env_overrides.insert(key, value);
                        }
                        Ok(())
                    })?;
                }
                let vfs = global.local.get(strand).vfs();
                let vfs = error::io_result(
                    strand,
                    vfs.windows_admin(cwd.to_path(), env_overrides, elevate)
                        .await,
                )?;
                global.types.vfs.create_with_annex(
                    strand,
                    Vfs,
                    VfsAnnex {
                        vfs,
                        source: VfsSource::WindowsAdmin,
                        global,
                    },
                    out,
                );
                Ok(())
            })
    }
}

/// Extracts raw bytes from a `Str` or `Bin` argument.
fn bytes_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, Vec<u8>> {
    if let Some(value) = value.as_str(strand) {
        Ok(strand.access(|x| value.as_str(x).as_bytes().to_vec()))
    } else if let Some(value) = value.as_bin(strand) {
        Ok(value.to_vec())
    } else {
        Err(Error::type_error(
            strand,
            format!("{name}: expected Str or Bin"),
        ))
    }
}

pub(crate) fn configure_compiler<'a>(config: &mut Config<'a>) {
    config
        .prelude()
        .import_module("shell")
        .import_items("shell")
        .items(["exit", "env", "cd"])
        .commit();
}

pub(crate) fn configure_vm<'v>(
    builder: &mut Register<'v>,
    core: State<'v, Global<'v>>,
    global: State<'v, ShellGlobal<'v>>,
) {
    let env_ty = builder.register_type::<EnvObject>();
    let args_ty = builder.register_type::<ShellArgs>();
    let args_sym = builder.sym("args");
    let program_sym = builder.sym("program");

    builder
        .module("shell")
        .function("line_ending", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            // A function rather than a getter: the answer follows the VFS
            // target, so it is a question about the current context rather
            // than a constant of the module.
            let os = global.local.get(strand).target().os();
            let ending = std::str::from_utf8(line_ending(os)).unwrap();
            Output::set(strand, out, ending);
            Ok(())
        })
        .function(
            "exit",
            async move |strand, args: dolang::runtime::Args<'v, '_>, _| {
                let (_, [code]) = unpack!(strand, args, 0, 1)?;
                let rc = match code {
                    Some(slot) => slot
                        .to_i64(strand)
                        .map_err(|_| Error::type_error(strand, "exit: not an integer"))?,
                    None => 0i64,
                };
                let code = rc.try_into().map_err(|_| Error::overflow(strand))?;
                Err(Error::abort(strand, Exit { code }))
            },
        )
        .function("exec", async move |strand, mut args, _| {
            let program = match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(program)) => path_from_value(strand, &program)?,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            };

            let (vfs, path, cwd, env) = {
                let local = global.local.get(strand);
                let vfs = local.vfs();
                if !vfs.is_direct() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "shell.exec is only supported on the host VFS",
                    )
                    .into_sys(strand));
                }
                let cwd = local.cwd().clone();
                let env = local.env();
                let path = vfs
                    .which(
                        program.to_path(),
                        env.get("PATH").as_deref(),
                        Some(cwd.to_path()),
                    )
                    .await
                    .into_sys(strand)?
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("executable not found: {program}"),
                        )
                        .into_sys(strand)
                    })?;
                (vfs, path, cwd, env.flatten_delta().into_iter().collect())
            };
            debug_assert!(vfs.is_direct());

            let program = path.to_native().into_sys(strand)?;
            let cwd = cwd.to_native().into_sys(strand)?;
            let mut command_args = Vec::new();
            for arg in args {
                match arg {
                    Arg::Pos(value) => {
                        command_args.push(value.to_verbatim(strand)?);
                    }
                    Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                }
            }
            Err(Error::abort(
                strand,
                Exec {
                    program,
                    args: command_args,
                    cwd,
                    env,
                },
            ))
        })
        .get("VERSION", move |strand, out| {
            let components: [i64; 3] = [
                env!("CARGO_PKG_VERSION_MAJOR"),
                env!("CARGO_PKG_VERSION_MINOR"),
                env!("CARGO_PKG_VERSION_PATCH"),
            ]
            .map(|component| {
                component
                    .parse()
                    .expect("CARGO_PKG_VERSION_* is not an integer")
            });
            Output::set(strand, out, AsTuple::new(components));
            Ok(())
        })
        .get("args", move |strand, out| {
            let invocation = global.local.get(strand).invocation();
            let args = invocation
                .args
                .unwrap_or_else(|| core.args.borrow().clone());
            args_ty.create_with_annex(strand, ShellArgs, args, out);
            Ok(())
        })
        .get("program", move |strand, out| {
            let invocation = global.local.get(strand).invocation();
            match invocation.program {
                Some(ProgramOverride::Path(path)) => {
                    let fs = strand.force_state::<FsGlobal<'v>>();
                    create_path(strand, fs, path, out)?;
                }
                Some(ProgramOverride::Module(name)) => Output::set(strand, out, name.as_ref()),
                None => match core.program.borrow().as_ref() {
                    Some(ProgramSource::Path(path)) => {
                        let path = vfs_path::PathBuf::from_native(path.clone()).into_sys(strand)?;
                        let fs = strand.force_state::<FsGlobal<'v>>();
                        create_path(strand, fs, path, out)?;
                    }
                    Some(ProgramSource::Module(name)) => Output::set(strand, out, name.as_str()),
                    None => Output::set(strand, out, Nil),
                },
            }
            Ok(())
        })
        .function_with_slots(
            "with_override",
            async move |strand, args, out, [mut iter, mut item]| {
                let ([func], [args, program]) =
                    unpack!(strand, args, 1, 0, args_sym = None, program_sym = None)?;

                let args = if let Some(args) = args {
                    let mut values = Vec::new();
                    args.iter(strand, &mut iter).await?;
                    while iter.next(strand, &mut item).await? {
                        values.push(item.to_verbatim(strand)?.into_boxed_str());
                    }
                    Some(values.into())
                } else {
                    None
                };

                let program = if let Some(program) = program {
                    if let Some(name) = program.as_str(strand) {
                        Some(ProgramOverride::Module(name.to_string().into_boxed_str()))
                    } else if let Some(path) = cast_path(strand, &program) {
                        Some(ProgramOverride::Path(path))
                    } else {
                        return Err(Error::type_error(
                            strand,
                            "program: expected fs.Path or Str",
                        ));
                    }
                } else {
                    None
                };

                let local = global.local.get(strand);
                let original = local.invocation();
                let mut invocation = original.clone();
                if let Some(args) = args {
                    invocation.args = Some(args);
                }
                if let Some(program) = program {
                    invocation.program = Some(program);
                }
                local.replace_invocation(invocation);

                let result = call!(strand, &func, out).await;
                global.local.get(strand).replace_invocation(original);
                result
            },
        )
        .object("env", env_ty, EnvObject { global })
        .get("exe", move |strand, out| {
            let exe = vfs_path::PathBuf::from_native(
                std::env::current_exe().expect("could not get current exe"),
            )
            .expect("current executable path is UTF-8");
            let fs = strand.force_state::<FsGlobal<'v>>();
            create_path(strand, fs, exe, out)
        })
        .function("vfs_exe", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            match global.local.get(strand).vfs_exe() {
                Some(path) => {
                    let fs = strand.force_state::<FsGlobal<'v>>();
                    create_path(strand, fs, path, out)?;
                }
                None => Output::set(strand, out, Nil),
            }
            Ok(())
        })
        .function("with_host", async move |strand, mut args, out| {
            let func = match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
            };

            let host_vfs = error::io_result(strand, VfsVfs::direct())?;
            Local::with_vfs(strand, global.local, host_vfs, async move |strand| {
                func.call(strand, args, out).await
            })
            .await
        })
        .function("cd", async move |strand, mut args, out| {
            let dir = match args.next() {
                None => {
                    let cwd = global.local.get(strand).cwd().clone();
                    let fs = strand.force_state::<FsGlobal<'v>>();
                    create_path(strand, fs, cwd, out)?;
                    return Ok(());
                }
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            };
            let dir = path_from_value(strand, &dir)?;
            let local = global.local.get(strand);

            let path = local.cwd().join(dir.as_str());
            let func = match args.next() {
                None => None,
                Some(Arg::Pos(slot)) => Some(slot),
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            };
            if let Some(func) = func {
                let old = local.replace_cwd(path);
                let res = func.call(strand, args, out).await;
                let local = global.local.get(strand);
                let _ = local.replace_cwd(old);
                res
            } else {
                let _ = local.replace_cwd(path);
                Ok(())
            }
        })
        .value("Vfs", global.types.vfs)
        .value("Stdin", global.types.stdin)
        .value("Stdout", global.types.stdout)
        .value("Stderr", global.types.stderr)
        .object("stdin", global.types.stdin, Stdin::default())
        .object("stdout", global.types.stdout, Stdout)
        .object("stderr", global.types.stderr, Stderr)
        .commit();
}
