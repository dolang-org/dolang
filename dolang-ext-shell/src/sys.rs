use std::fmt::{self, Debug, Display};

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
    env::Env,
    error::ErrorExt as ShellErrorExt,
    fs::path::{Path, PathAnnex, path_from_value},
    global::{Global, ProgramSource},
};

/// Exit error.
///
/// The `exit` function propagates an [`Error::interrupt`] containing
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
    const MODULE: &'v str = "sys";
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
    const MODULE: &'v str = "sys";
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
    const MODULE: &'v str = "sys";
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

pub(crate) fn configure_compiler<'a>(compiler: &mut Compiler<'a>) {
    compiler
        .prelude()
        .import_items("sys")
        .items(["echo", "exit", "env", "cd", "print"])
        .commit();
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let env_ty = builder.register_type::<Env>();

    builder
        .module("sys")
        .function(
            "exit",
            async move |strand, args: dolang::runtime::Args<'v, '_>, _| {
                let (_, [code]) = unpack!(strand, args, 0, 1)?;
                let rc = match code {
                    Some(slot) => slot
                        .as_i64(strand)
                        .ok_or_else(|| Error::type_error(strand, "exit: not an integer"))?,
                    None => 0i64,
                };
                let code = rc.try_into().map_err(|_| Error::overflow(strand))?;
                Err(Error::interrupt(strand, Exit { code }))
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
        .value("os", std::env::consts::OS)
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
                    global.types.path.create_with_annex(
                        strand,
                        Path,
                        PathAnnex::new(path.clone(), global),
                        out,
                    );
                }
                Some(ProgramSource::Module(name)) => Output::set(strand, out, name.as_str()),
                None => Output::set(strand, out, Nil),
            }
            Ok(())
        })
        .value("Error", global.types.sys_error)
        .value("NotFoundError", global.types.not_found)
        .value("PermissionDeniedError", global.types.permission_denied)
        .value("AlreadyExistsError", global.types.already_exists)
        .value("TimedOutError", global.types.timed_out)
        .object("env", env_ty, Env { global })
        .object_with_annex(
            "exe",
            global.types.path,
            Path,
            PathAnnex {
                inner: std::env::current_exe().expect("could not get current exe"),
                global,
            },
        )
        .function("cd", async move |strand, mut args, out| {
            use crate::fs::path::{Path, PathAnnex};

            let dir = match args.next() {
                None => {
                    let cwd = global.local.get(strand).cwd().as_ref().to_owned();
                    global.types.path.create_with_annex(
                        strand,
                        Path,
                        PathAnnex::new(cwd, global),
                        out,
                    );
                    return Ok(());
                }
                Some(Arg::Pos(slot)) => slot,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
            };
            let dir = path_from_value(strand, global, &dir)?;
            let local = global.local.get(strand);

            let path = local.cwd().as_ref().join(&dir);
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
        .commit();

    // Register Stdin and Stdout types with global
    let _stdin_ty = builder.register_type::<Stdin>();
    let _stdout_ty = builder.register_type::<Stdout>();
}
