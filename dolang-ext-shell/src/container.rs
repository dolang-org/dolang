#[cfg(unix)]
use std::{
    collections::HashMap,
    env, fmt,
    path::{self, PathBuf},
    rc::Rc,
};

#[cfg(not(unix))]
use std::{
    env,
    fs::Metadata,
    path::{Path, PathBuf},
    rc::Rc,
};

#[cfg(not(unix))]
use tokio::{fs::OpenOptions, io};

#[cfg(unix)]
use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Result, Slot, State, Strand, Type, error::ResultExt,
    object::TypeBuilder, unpack, vm::Builder,
};

#[cfg(not(unix))]
use dolang::runtime::{Arg, Error, State, vm::Builder};

#[cfg(unix)]
use dolang_shell_vfs::{Client, Query};

use crate::{global::Global, local::Env};

#[cfg(unix)]
use crate::error;

#[cfg(not(unix))]
#[derive(Clone)]
pub(crate) struct Client;

#[cfg(not(unix))]
impl Client {
    pub(crate) fn open_options(&self) -> OpenOptions {
        unreachable!()
    }

    pub(crate) async fn metadata(&self, _path: &Path) -> io::Result<Metadata> {
        unreachable!()
    }

    pub(crate) async fn symlink_metadata(&self, _path: &Path) -> io::Result<Metadata> {
        unreachable!()
    }

    pub(crate) async fn copy(&self, _from: &Path, _to: &Path, _all: bool) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn move_(&self, _from: &Path, _to: &Path, _all: bool) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn symlink(&self, _src: &Path, _dst: &Path) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn canonicalize(&self, _path: &Path) -> io::Result<PathBuf> {
        unreachable!()
    }

    pub(crate) async fn read_link(&self, _path: &Path) -> io::Result<PathBuf> {
        unreachable!()
    }

    pub(crate) async fn create_dir(&self, _path: &Path, _all: bool) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn remove(&self, _path: &Path, _all: bool, _ignore: bool) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn remove_dir(
        &self,
        _path: &Path,
        _all: bool,
        _ignore: bool,
    ) -> io::Result<()> {
        unreachable!()
    }

    pub(crate) async fn glob(
        &self,
        _pattern: &str,
        _root: &Path,
        _follow: bool,
        _max_depth: Option<usize>,
    ) -> io::Result<Vec<PathBuf>> {
        unreachable!()
    }
}

#[cfg(not(unix))]
#[derive(Clone)]
pub(crate) struct Context;

#[cfg(not(unix))]
impl Context {
    pub(crate) fn client(&self) -> &Client {
        unreachable!()
    }
}

#[cfg(unix)]
#[derive(Clone)]
pub(crate) struct Context {
    client: Client,
    cwd: PathBuf,
    env: Rc<Env>,
}

#[cfg(unix)]
impl Context {
    pub(crate) async fn enter<'v, 's, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, Global<'v>>,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
    ) -> R {
        let local = global.local.get(strand);
        let orig = (*local.container_mut()).replace(self.clone());
        let orig_cwd = local.replace_cwd(self.cwd.clone());
        let orig_env = local.replace_env(Rc::new(Env::derived(self.env.clone(), HashMap::new())));
        let res = f(strand).await;
        let local = global.local.get(strand);
        *local.container_mut() = orig;
        local.replace_cwd(orig_cwd);
        local.replace_env(orig_env);
        res
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }
}

#[cfg(unix)]
pub(crate) struct Vfs;

#[cfg(unix)]
pub(crate) struct VfsAnnex<'v> {
    handle: Context,
    socket: PathBuf,
    global: State<'v, Global<'v>>,
}

#[cfg(unix)]
impl<'v> VfsAnnex<'v> {
    pub(crate) fn new(handle: Context, socket: &path::Path, global: State<'v, Global<'v>>) -> Self {
        Self {
            handle,
            socket: socket.into(),
            global,
        }
    }
}

#[cfg(unix)]
impl<'v> Object<'v> for Vfs {
    const NAME: &'v str = "Vfs";
    const MODULE: &'v str = "container";
    type Annex = VfsAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        use crate::fs::path::PathOrStr;

        let global = strand.vm().state::<Global<'v>>();
        let unix_socket = global.syms.unix_socket;

        let ([path], []) = unpack!(strand, args, 0, 0, unix_socket)?;
        let path = PathOrStr::new(strand, global, &path)?;
        let path = path.to_owned();

        let parent_client = {
            let local = global.local.get(strand);
            local
                .container()
                .as_ref()
                .map(|context| context.client().clone())
        };

        let client = if let Some(parent_client) = parent_client {
            let fd = error::io_result(
                strand,
                parent_client
                    .unix_stream_socket(None::<&path::Path>, Some(path.as_path()))
                    .await,
            )?;
            error::io_result(strand, Client::try_from(fd))?
        } else {
            error::io_result(strand, Client::connect(&path).await)?
        };
        let Query { env, cwd } = error::io_result(strand, client.query().await)?;
        let env = Rc::new(Env::new(None, true, env.into_iter()));

        global.types.vfs.create_with_annex(
            strand,
            Vfs,
            VfsAnnex::new(Context { client, env, cwd }, &path, global),
            out,
        );
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<container.Vfs socket: {:?}>", this.annex().socket).into_do(strand)
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
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
        builder.method("stop", async move |this, strand, _args, _out| {
            let borrow = this.annex();
            error::io_result(strand, borrow.handle.client().stop().await)?;
            Ok(())
        })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let mut module = builder.module("container");
    module = module.function("host", async move |strand, mut args, out| {
        let func = match args.next() {
            None => return Err(Error::missing_positional(strand, 0)),
            Some(Arg::Pos(slot)) => slot,
            Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
        };

        let local = global.local.get(strand);

        // Save current state
        let orig_agent = (*local.container_mut()).take();
        let orig_cwd = local.replace_cwd(env::current_dir().unwrap());
        let orig_env = local.replace_env(Rc::new(Env::root()));

        // Execute in fresh host context
        let result = func.call(strand, args, out).await;

        // Restore original state
        let local = global.local.get(strand);
        *local.container_mut() = orig_agent;
        local.replace_cwd(orig_cwd);
        local.replace_env(orig_env);

        result
    });

    // Register the Vfs type in the container module (Unix only)
    #[cfg(unix)]
    {
        module = module.value("Vfs", global.types.vfs);
    }

    module.commit();
}
