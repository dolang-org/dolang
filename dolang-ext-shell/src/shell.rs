use std::{
    fmt::{self, Debug, Display},
    rc::Rc,
};

use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use dolang::{
    compile::Compiler,
    runtime::{
        Arg, Error, Instance, Object, Output, Result, Slot, State, Strand,
        object::TypeBuilder,
        unpack,
        value::{Empty, Nil, TypeObject},
        vm::Builder,
    },
};

use crate::{
    env::Env as EnvObject,
    error::{ErrorExt, ResultExt as _},
    fs::path::{PathAnnex, create_path_annex, path_from_value},
    global::{Global, ProgramSource},
    local::Env as LocalEnv,
};

#[cfg(windows)]
use dolang_shell_vfs::WindowsSession;
use dolang_shell_vfs::{AnyVfs, Client, Query, TargetInfo, Utf8TypedPathBuf};
use std::collections::HashMap;
#[cfg(unix)]
use std::path::PathBuf;

use dolang::runtime::error::ResultExt;

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
    env: Rc<LocalEnv>,
    target: TargetInfo,
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
        let orig_cwd = local.replace_cwd(self.cwd.clone());
        let orig_env =
            local.replace_env(Rc::new(LocalEnv::derived(self.env.clone(), HashMap::new())));
        let orig_target = local.replace_target(self.target.clone());
        let res = f(strand).await;
        let local = global.local.get(strand);
        local.replace_vfs(orig);
        local.replace_cwd(orig_cwd);
        local.replace_env(orig_env);
        local.replace_target(orig_target);
        res
    }

    #[cfg(unix)]
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }
}

pub(crate) struct Stdin(BufReader<io::Stdin>);

impl Stdin {
    pub(crate) fn new() -> Self {
        Self(BufReader::new(io::stdin()))
    }
}

impl Default for Stdin {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Object<'v> for Stdin {
    const NAME: &'v str = "Stdin";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut line = String::new();
        if this
            .borrow_mut(strand)?
            .0
            .read_line(&mut line)
            .await
            .map_err(|err| err.into_sys(strand))?
            != 0
        {
            Output::set(strand, out, line.as_str());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) struct Stdout(io::Stdout);

impl Stdout {
    pub(crate) fn new() -> Self {
        Self(io::stdout())
    }
}

impl Default for Stdout {
    fn default() -> Self {
        Self::new()
    }
}

impl<'v> Object<'v> for Stdout {
    const NAME: &'v str = "Stdout";
    const MODULE: &'v str = "shell";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Sink)
    }

    async fn output<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        if global.terminal.redirected.get() && global.terminal.stdout_is_terminal {
            // Route through the terminal writer so output goes through
            // the redirect target (e.g. MultiProgress::println).
            let s = value.to_string(strand)?;
            let mut writer = global.terminal.writer.lock().await;
            writer
                .write_all(s.as_bytes())
                .await
                .map_err(|e| e.into_sys(strand))?;
            writer
                .write_all(b"\n")
                .await
                .map_err(|e| e.into_sys(strand))?;
            writer.flush().await.map_err(|e| e.into_sys(strand))
        } else {
            this.borrow_mut(strand)?
                .0
                .write_all(value.to_string(strand)?.as_bytes())
                .await
                .map_err(|err| err.into_sys(strand))
        }
    }
}

/// Sink that writes to the current terminal writer (echo/print destination).
///
/// When terminal output is redirected via `with_terminal`, this writes to the
/// redirect target. Otherwise it writes to stderr.
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
        builder.supertype(TypeObject::Sink)
    }

    async fn output<'a, 's>(
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
        let s = value.to_string(strand)?;
        let mut writer = global.terminal.writer.lock().await;
        writer
            .write_all(s.as_bytes())
            .await
            .map_err(|e| e.into_sys(strand))?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|e| e.into_sys(strand))?;
        writer.flush().await.map_err(|e| e.into_sys(strand))
    }
}

pub(crate) struct Vfs;

pub(crate) struct VfsAnnex<'v> {
    handle: Context,
    source: VfsSource,
    global: State<'v, Global<'v>>,
}

enum VfsSource {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    Windows(WindowsSession),
}

impl<'v> Object<'v> for Vfs {
    const NAME: &'v str = "Vfs";
    const MODULE: &'v str = "shell";
    type Annex = VfsAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        match &this.annex().source {
            #[cfg(unix)]
            VfsSource::Unix(socket) => write!(w, "<shell.Vfs socket: {socket:?}>").into_do(strand),
            #[cfg(windows)]
            VfsSource::Windows(_) => write!(w, "<shell.Vfs windows admin>").into_do(strand),
        }
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: dolang::runtime::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
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
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        #[cfg(unix)]
        let builder = builder.type_method("unix_socket", async move |_this, strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let global = strand.vm().state::<Global<'v>>();
            let path = path_from_value(strand, global, &path)?;

            let parent_client = {
                let local = global.local.get(strand);
                local.vfs().into_client()
            };

            let client = if let Some(parent_client) = parent_client {
                let native = dolang_shell_vfs::native_path(path.to_path()).into_sys(strand)?;
                let fd = error::io_result(
                    strand,
                    parent_client
                        .unix_stream_socket(None::<&std::path::Path>, Some(native.as_path()))
                        .await,
                )?;
                error::io_result(strand, Client::try_from(fd))?
            } else {
                let native = dolang_shell_vfs::native_path(path.to_path()).into_sys(strand)?;
                error::io_result(strand, Client::connect(native).await)?
            };
            let Query { env, cwd, target } = error::io_result(strand, client.query().await)?;
            let env = Rc::new(LocalEnv::new(None, true, env, target.operating_system));
            let source =
                VfsSource::Unix(dolang_shell_vfs::native_path(path.to_path()).into_sys(strand)?);

            global.types.vfs.create_with_annex(
                strand,
                Vfs,
                VfsAnnex {
                    handle: Context {
                        client,
                        env,
                        cwd,
                        target,
                    },
                    source,
                    global,
                },
                out,
            );
            Ok(())
        });

        #[cfg(windows)]
        let (builder, elevate) = {
            let mut builder = builder;
            let elevate = builder.sym("elevate");
            (builder, elevate)
        };
        let builder = builder.method("stop", async move |this, strand, _args, _out| {
            let borrow = this.annex();
            let result = match &borrow.source {
                #[cfg(unix)]
                VfsSource::Unix(_) => borrow.handle.client().stop().await,
                #[cfg(windows)]
                VfsSource::Windows(session) => session.stop().await,
            };
            error::io_result(strand, result)?;
            Ok(())
        });

        #[cfg(windows)]
        let builder =
            builder.type_method("windows_admin", async move |_this, strand, args, out| {
                let ([], [elevate]) = unpack!(strand, args, 0, 0, elevate = None)?;
                let elevate = match elevate {
                    Some(elevate) => elevate
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "elevate: expected bool"))?,
                    None => true,
                };
                let global = strand.vm().state::<Global<'v>>();
                let cwd = global.local.get(strand).cwd().clone();
                let cwd = dolang_shell_vfs::native_path(cwd.to_path()).into_sys(strand)?;
                let result = if elevate {
                    WindowsSession::launch(cwd).await
                } else {
                    WindowsSession::launch_unelevated(cwd).await
                };
                let (session, Query { env, cwd, target }) = error::io_result(strand, result)?;
                let client = session.client().clone();
                let env = Rc::new(LocalEnv::new(None, true, env, target.operating_system));
                global.types.vfs.create_with_annex(
                    strand,
                    Vfs,
                    VfsAnnex {
                        handle: Context {
                            client,
                            env,
                            cwd,
                            target,
                        },
                        source: VfsSource::Windows(session),
                        global,
                    },
                    out,
                );
                Ok(())
            });

        builder
    }
}

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler
        .prelude()
        .import_items("shell")
        .items(["echo", "exit", "env", "cd", "print", "host"])
        .commit();
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let env_ty = builder.register_type::<EnvObject>();

    builder
        .module("shell")
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
        .function("echo", async move |strand, args, _| {
            // Collect formatted args before taking the writer, so to_arg
            // errors don't require restoring the writer.
            let mut parts: Vec<(Option<String>, String)> = Vec::new();
            for arg in args {
                match arg {
                    Arg::Pos(value) => {
                        parts.push((None, value.to_arg(strand)?));
                    }
                    Arg::Key(sym, value) => {
                        let key = sym.as_str(strand).to_owned();
                        let arg = value.to_arg(strand)?;
                        parts.push((Some(key), arg));
                    }
                }
            }
            let mut writer = global.terminal.writer.lock().await;
            let mut space = false;
            for (key, arg) in &parts {
                if space {
                    writer
                        .write_all(b" ")
                        .await
                        .map_err(|e| e.into_sys(strand))?;
                }
                space = true;
                if let Some(key) = key {
                    writer
                        .write_all(key.as_bytes())
                        .await
                        .map_err(|e| e.into_sys(strand))?;
                    writer
                        .write_all(b": ")
                        .await
                        .map_err(|e| e.into_sys(strand))?;
                }
                writer
                    .write_all(arg.as_bytes())
                    .await
                    .map_err(|e| e.into_sys(strand))?;
            }
            writer
                .write_all(b"\n")
                .await
                .map_err(|e| e.into_sys(strand))?;
            writer.flush().await.map_err(|e| e.into_sys(strand))
        })
        .function("print", async move |strand, args, _| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let arg = arg.to_arg(strand)?;
            let mut writer = global.terminal.writer.lock().await;
            writer
                .write_all(arg.as_bytes())
                .await
                .map_err(|e| e.into_sys(strand))?;
            writer.flush().await.map_err(|e| e.into_sys(strand))
        })
        .get("args", move |strand, mut out| {
            Output::set(strand, &mut out, Empty::Array);
            let array = out.as_array(strand).unwrap();
            for arg in global.args.borrow().iter() {
                array.push(strand, arg.as_str())?;
            }
            Ok(())
        })
        .get("program", move |strand, out| {
            match global.program.borrow().as_ref() {
                Some(ProgramSource::Path(path)) => {
                    let path = dolang_shell_vfs::typed_path(path.clone()).into_sys(strand)?;
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
                }
                Some(ProgramSource::Module(name)) => Output::set(strand, out, name.as_str()),
                None => Output::set(strand, out, Nil),
            }
            Ok(())
        })
        .object("env", env_ty, EnvObject { global })
        .get("exe", move |strand, out| {
            let annex = PathAnnex::new(
                dolang_shell_vfs::typed_path(
                    std::env::current_exe().expect("could not get current exe"),
                )
                .expect("current executable path is UTF-8"),
                global,
            );
            create_path_annex(strand, annex, out);
            Ok(())
        })
        .function("host", async move |strand, mut args, out| {
            let func = match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
            };

            let local = global.local.get(strand);

            let orig_vfs = local.replace_vfs(Default::default());
            let orig_cwd = local.replace_cwd(
                dolang_shell_vfs::typed_path(std::env::current_dir().unwrap())
                    .expect("current directory is UTF-8"),
            );
            let orig_env = local.replace_env(Rc::new(LocalEnv::root()));
            let orig_target = local.replace_target(TargetInfo::current());

            let result = func.call(strand, args, out).await;

            let local = global.local.get(strand);
            local.replace_vfs(orig_vfs);
            local.replace_cwd(orig_cwd);
            local.replace_env(orig_env);
            local.replace_target(orig_target);

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
        .commit();

    // Register Stdin and Stdout types with global
    let _stdin_ty = builder.register_type::<Stdin>();
    let _stdout_ty = builder.register_type::<Stdout>();
}
