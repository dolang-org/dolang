use std::{
    fmt::{self, Debug, Display},
    rc::Rc,
};

use tokio::io::AsyncWriteExt;

use dolang::runtime::object::fmt;

use dolang::{
    compile::Compiler,
    runtime::{
        Arg, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call, method,
        object::{Mut, TypeBuilder},
        unpack,
        value::{AsTuple, Nil, TypeObject},
        vm::Builder,
    },
};

use crate::util;
use crate::{
    env::Env as EnvObject,
    error::{ErrorExt, ResultExt as _},
    fs::path::{PathAnnex, create_path_annex, path_from_value},
    global::{Global, ProgramSource},
    io_mode::{IoMode, ValueEncoding, encode_value, read_raw, read_value, write_raw},
    local::{Env as LocalEnv, ProgramOverride},
    pipe_channel,
    shell_args::Args as ShellArgs,
};
use dolang::runtime::value::View;
use dolang_vfs::{
    AnyVfs, Client, OperatingSystem, Query, SecurityInfo, StdioRecv, StdioSend, TargetInfo,
    Utf8TypedPathBuf, Vfs as _, VfsSession,
};
use std::collections::HashMap;

use crate::error;

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

#[derive(Clone)]
pub(crate) struct Context {
    client: Client,
    cwd: Utf8TypedPathBuf,
    current_exe: Utf8TypedPathBuf,
    env: Rc<LocalEnv>,
    target: TargetInfo,
    security: SecurityInfo,
}

impl Context {
    pub(crate) async fn enter<'v, 's, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
    ) -> R {
        let local = global.local.get(strand);
        let orig = local.replace_vfs(AnyVfs::from(self.client.clone()));
        let orig_exe = local.replace_vfs_exe(Some(self.current_exe.clone()));
        let orig_cwd = local.replace_cwd(self.cwd.clone());
        let orig_env =
            local.replace_env(Rc::new(LocalEnv::derived(self.env.clone(), HashMap::new())));
        let orig_target = local.replace_target(self.target.clone());
        let orig_security = local.replace_security(Some(self.security.clone()));
        let res = f(strand).await;
        let local = global.local.get(strand);
        local.replace_vfs(orig);
        local.replace_vfs_exe(orig_exe);
        local.replace_cwd(orig_cwd);
        local.replace_env(orig_env);
        local.replace_target(orig_target);
        local.replace_security(orig_security);
        res
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }
}

/// The process's standard input, exported as `shell.stdin`.
///
/// A handle, not a wrapper: the underlying reader lives in [`Global::stdio`] so
/// that this object and the root strand's implicit input are the same buffered
/// reader. See [`crate::global::Stdio`].
pub(crate) struct Stdin;

impl Default for Stdin {
    fn default() -> Self {
        Self
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
    }

    /// There is exactly one `Stdin` per VM, so having the type is having the
    /// object.
    fn eq<'a, 's>(
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        Ok(global.types.stdin.cast(other).is_some())
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
        _this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = strand.state::<Global<'v>>();
        let mode = global.local.get(strand).io_mode();
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
        let global = strand.state::<Global<'v>>();
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
        let mode = global.local.get(strand).io_mode();
        let bytes = encode_value(
            strand,
            &value,
            mode,
            ValueEncoding::Display,
            OperatingSystem::current(),
        )?;
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
        let global = strand.state::<Global<'v>>();
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
        let mode = global.local.get(strand).io_mode();
        let bytes = encode_value(
            strand,
            &value,
            mode,
            ValueEncoding::Display,
            OperatingSystem::current(),
        )?;
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
    global: State<'v, Global<'v>>,
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
    let recv_guard = pipe_channel::negotiate_recv(input, strand, global)
        .await?
        .ok_or_else(|| Error::type_error(strand, "Vfs: stream iterator is not a pipe channel"))?;
    let recv = recv_guard.recv_pipe().await.into_sys(strand)?;

    let send_guard = pipe_channel::negotiate_send(output, strand, global)
        .await?
        .ok_or_else(|| Error::type_error(strand, "Vfs: stream sink is not a pipe channel"))?;
    let send = send_guard.send_pipe().await.into_sys(strand)?;

    Ok((recv_guard, recv, send_guard, send))
}

pub(crate) struct Vfs;

pub(crate) struct VfsAnnex<'v> {
    handle: Context,
    source: VfsSource,
    global: State<'v, Global<'v>>,
}

enum VfsSource {
    Stream,
    Unix(Utf8TypedPathBuf),
    WindowsAdmin(VfsSession),
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
        w: &mut dyn dolang::runtime::Format<'v>,
    ) -> Result<'v, 's, ()> {
        match &this.annex().source {
            VfsSource::Stream => fmt!(strand, w, "<shell.Vfs stream>"),
            VfsSource::Unix(socket) => {
                fmt!(strand, w, "<shell.Vfs socket: {socket:?}>")
            }
            VfsSource::WindowsAdmin(_) => {
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
        let global = strand.state::<Global<'v>>();
        strand
            .with_slots(
                async move |strand, [mut module, mut stream, mut input, mut output]| {
                    strand.import("strand", &mut module).await?;
                    // The remote VFS connection's RPC framing runs over
                    // this pipe pair, so it wants a generous kernel buffer
                    // to make short reads/writes (and the fragment-size
                    // backoff they trigger) rare rather than routine. Read
                    // and cleared by `pipe_channel`'s factory closure; not
                    // threaded through `stream`'s public signature, since
                    // the pipe factory override is already internal-only.
                    global
                        .local
                        .get(strand)
                        .set_pending_pipe_buffer_size(Some(REMOTE_VFS_PIPE_BUFFER_SIZE));
                    method!(strand, &module, global.syms.stream, &mut stream, callable).await?;
                    stream.iter(strand, &mut input).await?;
                    stream.sink(strand, &mut output).await?;

                    // However this comes out, the pending buffer size hint
                    // must not leak into unrelated later pipe creation on
                    // this strand.
                    let negotiated = negotiate_stream_pipes(strand, global, &input, &output).await;
                    global.local.get(strand).set_pending_pipe_buffer_size(None);
                    let (recv_guard, recv, send_guard, send) = negotiated?;

                    let client = match Client::new_split(recv, send).await {
                        Ok(client) => client,
                        Err(negotiate_error) => {
                            let join = global.syms.join;
                            return match method!(strand, &stream, join, &mut module).await {
                                Ok(()) => Err(negotiate_error.into_sys(strand)),
                                Err(launcher_error) => Err(launcher_error),
                            };
                        }
                    };
                    let query = match client.query().await {
                        Ok(query) => query,
                        Err(query_error) => {
                            let join = global.syms.join;
                            match method!(strand, &stream, join, &mut module).await {
                                Ok(()) => return Err(query_error.into_sys(strand)),
                                Err(launcher_error) => return Err(launcher_error),
                            }
                        }
                    };
                    let Query {
                        env,
                        cwd,
                        current_exe,
                        target,
                        security,
                    } = query;
                    drop((recv_guard, send_guard));
                    let env = Rc::new(LocalEnv::new(None, true, env, target.operating_system));
                    global.types.vfs.create_with_annex(
                        strand,
                        Vfs,
                        VfsAnnex {
                            handle: Context {
                                client,
                                env,
                                cwd,
                                current_exe,
                                target,
                                security,
                            },
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
                                &stream,
                            );
                        });
                    Ok(())
                },
            )
            .await
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder.type_method("unix_socket", async move |_this, strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let global = strand.vm().state::<Global<'v>>();
            let path = path_from_value(strand, global, &path)?;
            let vfs = global.local.get(strand).vfs();
            let vfs = error::io_result(strand, vfs.unix_socket(path.to_path()).await)?;
            let client = vfs.into_client().ok_or_else(|| {
                Error::runtime(
                    strand,
                    "Unix VFS connection did not return a client backend",
                )
            })?;
            let Query {
                env,
                cwd,
                current_exe,
                target,
                security,
            } = error::io_result(strand, client.query().await)?;
            let env = Rc::new(LocalEnv::new(None, true, env, target.operating_system));
            let source = VfsSource::Unix(path.clone());

            global.types.vfs.create_with_annex(
                strand,
                Vfs,
                VfsAnnex {
                    handle: Context {
                        client,
                        env,
                        cwd,
                        current_exe,
                        target,
                        security,
                    },
                    source,
                    global,
                },
                out,
            );
            Ok(())
        });

        let builder = builder.method("with", async move |this, strand, mut args, out| {
            let func = match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
            };
            let borrow = this.annex();
            borrow
                .handle
                .enter(strand, borrow.global, async move |strand| {
                    func.call(strand, args, out).await
                })
                .await
        });

        let (builder, elevate_sym, cd_sym, env_sym) = {
            let mut builder = builder;
            let elevate_sym = builder.sym("elevate");
            let cd_sym = builder.sym("cd");
            let env_sym = builder.sym("env");
            (builder, elevate_sym, cd_sym, env_sym)
        };
        let builder = builder.method("stop", async move |this, strand, _args, _out| {
            let borrow = this.annex();
            match &borrow.source {
                VfsSource::Stream => error::io_result(strand, borrow.handle.client().stop().await)?,
                VfsSource::Unix(_) => {
                    error::io_result(strand, borrow.handle.client().stop().await)?
                }
                VfsSource::WindowsAdmin(session) => error::io_result(strand, session.stop().await)?,
            }
            Ok(())
        });

        builder.type_method("windows_admin", async move |_this, strand, args, out| {
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
            let global = strand.vm().state::<Global<'v>>();
            let current_cwd = global.local.get(strand).cwd().clone();
            let cwd = if let Some(cd) = cd {
                let cd = path_from_value(strand, global, &cd)?;
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
            let session = error::io_result(
                strand,
                vfs.windows_admin(cwd.to_path(), env_overrides, elevate)
                    .await,
            )?;
            let client = session.client().clone();
            let Query {
                env,
                cwd,
                current_exe,
                target,
                security,
            } = error::io_result(strand, client.query().await)?;
            let env = Rc::new(LocalEnv::new(None, true, env, target.operating_system));
            global.types.vfs.create_with_annex(
                strand,
                Vfs,
                VfsAnnex {
                    handle: Context {
                        client,
                        env,
                        cwd,
                        current_exe,
                        target,
                        security,
                    },
                    source: VfsSource::WindowsAdmin(session),
                    global,
                },
                out,
            );
            Ok(())
        })
    }
}

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler
        .prelude()
        .import_module("shell")
        .import_items("shell")
        .items(["exit", "env", "cd"])
        .commit();
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let env_ty = builder.register_type::<EnvObject>();
    let args_ty = builder.register_type::<ShellArgs>();
    let args_sym = builder.sym("args");
    let program_sym = builder.sym("program");

    builder
        .module("shell")
        .function("with_io_mode", async move |strand, args, out| {
            let ([mode, func], [], rest) = unpack!(strand, args, 2, 0, ...)?;
            let mode = match mode.as_sym(strand) {
                Some(sym) if sym == global.syms.line => IoMode::Line,
                Some(sym) if sym == global.syms.chunk => IoMode::Chunk,
                _ => return Err(Error::value(strand, "mode must be :LINE: or :CHUNK:")),
            };
            let old_mode = {
                let local = global.local.get(strand);
                let old_mode = local.io_mode();
                local.set_io_mode(mode);
                old_mode
            };
            let res = func.call(strand, rest, out).await;
            global.local.get(strand).set_io_mode(old_mode);
            res
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
                .unwrap_or_else(|| global.args.borrow().clone());
            args_ty.create_with_annex(strand, ShellArgs, args, out);
            Ok(())
        })
        .get("program", move |strand, out| {
            let invocation = global.local.get(strand).invocation();
            match invocation.program {
                Some(ProgramOverride::Path(path)) => {
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
                }
                Some(ProgramOverride::Module(name)) => Output::set(strand, out, name.as_ref()),
                None => match global.program.borrow().as_ref() {
                    Some(ProgramSource::Path(path)) => {
                        let path = dolang_vfs::typed_path(path.clone()).into_sys(strand)?;
                        let annex = PathAnnex::try_new(strand, path, global)?;
                        create_path_annex(strand, annex, out);
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
                        values.push(item.to_arg(strand)?.into_boxed_str());
                    }
                    Some(values.into())
                } else {
                    None
                };

                let program = if let Some(program) = program {
                    if let Some(name) = program.as_str(strand) {
                        Some(ProgramOverride::Module(name.to_string().into_boxed_str()))
                    } else if let Some(path) = global.types.unix_path.cast(&program) {
                        Some(ProgramOverride::Path(
                            path.enter_sync(strand, |_strand, path| path.annex().typed_path_buf()),
                        ))
                    } else if let Some(path) = global.types.windows_path.cast(&program) {
                        Some(ProgramOverride::Path(
                            path.enter_sync(strand, |_strand, path| path.annex().typed_path_buf()),
                        ))
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
            let annex = PathAnnex::new(
                dolang_vfs::typed_path(std::env::current_exe().expect("could not get current exe"))
                    .expect("current executable path is UTF-8"),
                global,
            );
            create_path_annex(strand, annex, out);
            Ok(())
        })
        .function("vfs_exe", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            match global.local.get(strand).vfs_exe() {
                Some(path) => {
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
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

            let local = global.local.get(strand);

            let orig_vfs = local.replace_vfs(Default::default());
            let orig_vfs_exe = local.replace_vfs_exe(None);
            let orig_cwd = local.replace_cwd(
                dolang_vfs::typed_path(std::env::current_dir().unwrap())
                    .expect("current directory is UTF-8"),
            );
            let orig_env = local.replace_env(Rc::new(LocalEnv::root()));
            let orig_target = local.replace_target(TargetInfo::current());
            let orig_security = local.replace_security(None);

            let result = func.call(strand, args, out).await;

            let local = global.local.get(strand);
            local.replace_vfs(orig_vfs);
            local.replace_vfs_exe(orig_vfs_exe);
            local.replace_cwd(orig_cwd);
            local.replace_env(orig_env);
            local.replace_target(orig_target);
            local.replace_security(orig_security);

            result
        })
        .function("cd", async move |strand, mut args, out| {
            use crate::fs::path::PathAnnex;

            let dir = match args.next() {
                None => {
                    let cwd = global.local.get(strand).cwd().clone();
                    let annex = PathAnnex::try_new(strand, cwd, global)?;
                    create_path_annex(strand, annex, out);
                    return Ok(());
                }
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            };
            let dir = path_from_value(strand, global, &dir)?;
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
        .object("stdin", global.types.stdin, Stdin)
        .object("stdout", global.types.stdout, Stdout)
        .object("stderr", global.types.stderr, Stderr)
        .commit();
}
