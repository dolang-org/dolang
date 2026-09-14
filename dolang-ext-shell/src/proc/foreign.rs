//! The `proc` surface for processes this interpreter did not spawn:
//! `proc.Info`, `proc.Procs`, `proc.Proc`, and `proc.Status`.
//!
//! Everything here addresses the *target*, not the host. A remote VFS makes
//! `enumerate` list the far end's process table, which is why nothing below is
//! conditionally compiled: whether signals exist is a property of the target,
//! settled at runtime, and a Windows host driving a Unix target must still be
//! able to call [`Proc::signal`](Procs).

use dolang::runtime::{
    AllocExt, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call, method,
    object::{TypeBuilder, fmt},
    unpack,
    value::{AsTuple, Nil, TypeObject},
};
use dolang_vfs::process::{
    Process as VfsProcess, ProcessExit, ProcessInfo, Processes as VfsProcesses,
};

use crate::{
    error::ResultExt,
    fs::path::create_path,
    global::{FsGlobal, ProcGlobal, UnixSecurityGlobal, WindowsSecurityGlobal},
    proc::parse_signal,
    security::{create_identity, create_token_info},
};

/// A snapshot of one process on the target.
pub(crate) struct Info;

pub(crate) struct InfoAnnex<'v> {
    pub(crate) global: State<'v, ProcGlobal<'v>>,
    pub(crate) info: ProcessInfo,
}

pub(crate) fn create_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, ProcGlobal<'v>>,
    info: ProcessInfo,
    out: impl Output<'v>,
) {
    global
        .types
        .proc_info
        .create_with_annex(strand, Info, InfoAnnex { global, info }, out);
}

impl<'v> Object<'v> for Info {
    const NAME: &'v str = "Info";
    const MODULE: &'v str = "proc";
    type Annex = InfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        fmt!(
            strand,
            w,
            "<proc.Info {} {:?}>",
            annex.info.pid(),
            annex.info.name()
        )
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let unix_id = builder.sym("unix_id");
        let token_info = builder.sym("token_info");
        let cmdline_win = builder.sym("cmdline_win");
        builder
            .get("pid", |this, strand, out| {
                Output::set(strand, out, this.annex().info.pid());
                Ok(())
            })
            .get("parent_pid", |this, strand, out| {
                match this.annex().info.parent_pid() {
                    Some(ppid) => Output::set(strand, out, ppid),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().info.name());
                Ok(())
            })
            .get("exe", |this, strand, out| {
                let annex = this.annex();
                match annex.info.exe() {
                    Some(exe) => {
                        let fs = strand.force_state::<FsGlobal<'v>>();
                        create_path(strand, fs, exe.to_path_buf(), out)
                    }
                    None => {
                        Output::set(strand, out, Nil);
                        Ok(())
                    }
                }
            })
            .get("cwd", |this, strand, out| {
                let annex = this.annex();
                match annex.info.cwd() {
                    Some(cwd) => {
                        let fs = strand.force_state::<FsGlobal<'v>>();
                        create_path(strand, fs, cwd.to_path_buf(), out)
                    }
                    None => {
                        Output::set(strand, out, Nil);
                        Ok(())
                    }
                }
            })
            .get("cmdline", |this, strand, out| {
                match this.annex().info.command_line() {
                    Some(cmdline) => Output::set(
                        strand,
                        out,
                        AsTuple::new(cmdline.iter().map(String::as_str)),
                    ),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            // The record itself knows which platform it came from, so an
            // inapplicable field reports `Unsupported` rather than an absence
            // the caller would have to interpret. That becomes a `FieldError`,
            // which is what a field the platform does not have means here.
            .get("unix_id", move |this, strand, mut out| {
                let annex = this.annex();
                match annex.info.identity() {
                    Ok(Some(identity)) => {
                        let security = strand.force_state::<UnixSecurityGlobal<'v>>();
                        create_identity(strand, security, identity, &mut out)
                    }
                    Ok(None) => Output::set(strand, out, Nil),
                    Err(_) => return Err(Error::field(strand, unix_id)),
                }
                Ok(())
            })
            .get("token_info", move |this, strand, mut out| {
                let annex = this.annex();
                match annex.info.token() {
                    Ok(Some(token)) => {
                        let security = strand.force_state::<WindowsSecurityGlobal<'v>>();
                        create_token_info(strand, security, token, &mut out)
                    }
                    Ok(None) => Output::set(strand, out, Nil),
                    Err(_) => return Err(Error::field(strand, token_info)),
                }
                Ok(())
            })
            .get("status", |this, strand, out| {
                let annex = this.annex();
                match annex.info.exit() {
                    Some(exit) => annex
                        .global
                        .types
                        .proc_status
                        .create_with_annex(strand, Status, exit, out),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .get("cmdline_win", move |this, strand, out| {
                match this.annex().info.windows_command_line() {
                    Ok(Some(cmdline)) => Output::set(strand, out, cmdline),
                    Ok(None) => Output::set(strand, out, Nil),
                    Err(_) => return Err(Error::field(strand, cmdline_win)),
                }
                Ok(())
            })
    }
}

/// How a process ended.
///
/// `code` is `nil` on a Unix target: reaping an arbitrary process is the
/// parent's privilege there, so a non-parent learns only that it is gone.
pub(crate) struct Status;

impl<'v> Object<'v> for Status {
    const NAME: &'v str = "Status";
    const MODULE: &'v str = "proc";
    type Annex = ProcessExit;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match this.annex().code() {
            Some(code) => fmt!(strand, w, "<proc.Status {code}>"),
            None => fmt!(strand, w, "<proc.Status>"),
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("code", |this, strand, out| {
            match this.annex().code() {
                Some(code) => Output::set(strand, out, code),
                None => Output::set(strand, out, Nil),
            }
            Ok(())
        })
    }
}

/// A lazy forward iterator over the target's process table.
///
/// Deliberately iteration-only, like `winscm.Services`: the table has no bound
/// this extension enforces, and entries are described as the cursor reaches
/// them, so `enumerate().find(...)` does not pay for the whole table.
pub(crate) struct Procs(pub(crate) VfsProcesses);

pub(crate) struct ProcsAnnex<'v> {
    pub(crate) global: State<'v, ProcGlobal<'v>>,
}

impl<'v> Object<'v> for Procs {
    const NAME: &'v str = "Procs";
    const MODULE: &'v str = "proc";
    type Annex = ProcsAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let entry = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?;
        let Some(entry) = entry else {
            return Ok(false);
        };
        create_info(strand, this.annex().global, entry, out);
        Ok(true)
    }
}

/// An open handle to a process on the target.
///
/// The handle, not the PID, is what makes the methods below refer to one
/// process: on Linux and Windows it holds a kernel reference that keeps the PID
/// from being reused underneath it.
pub(crate) struct Proc(pub(crate) Option<VfsProcess>);

impl<'v> Object<'v> for Proc {
    const NAME: &'v str = "Proc";
    const MODULE: &'v str = "proc";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let pid = this.borrow(strand)?.0.as_ref().map(VfsProcess::pid);
        match pid {
            Some(pid) => fmt!(strand, w, "<proc.Proc {pid}>"),
            None => fmt!(strand, w, "<proc.Proc closed>"),
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("pid", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let process = borrow
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "process handle is closed"))?;
                let pid = process.pid();
                drop(borrow);
                Output::set(strand, out, pid);
                Ok(())
            })
            .method("info", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<ProcGlobal<'v>>();
                let info = {
                    let borrow = this.borrow(strand)?;
                    let process = expect_open(strand, &borrow)?;
                    process.info().await.into_sys(strand)?
                };
                create_info(strand, global, info, out);
                Ok(())
            })
            .method("signal", async move |this, strand, args, _out| {
                let ([signal], []) = unpack!(strand, args, 1, 0)?;
                let signal = parse_signal(strand, &signal)?;
                let borrow = this.borrow(strand)?;
                let process = expect_open(strand, &borrow)?;
                process.signal(signal).await.into_sys(strand)
            })
            .method("terminate", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let process = expect_open(strand, &borrow)?;
                process.terminate().await.into_sys(strand)
            })
            .method("kill", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let process = expect_open(strand, &borrow)?;
                process.kill().await.into_sys(strand)
            })
            .method("wait", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<ProcGlobal<'v>>();
                let exit = {
                    let borrow = this.borrow(strand)?;
                    let process = expect_open(strand, &borrow)?;
                    process.wait().await.into_sys(strand)?
                };
                global
                    .types
                    .proc_status
                    .create_with_annex(strand, Status, exit, out);
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let process = this.borrow_mut(strand)?.0.take();
                match process {
                    Some(process) => process.close().await.into_sys(strand),
                    None => Ok(()),
                }
            })
    }
}

fn expect_open<'v, 's, 'b>(
    strand: &mut Strand<'v, 's>,
    borrow: &'b impl std::ops::Deref<Target = Proc>,
) -> Result<'v, 's, &'b VfsProcess> {
    borrow
        .0
        .as_ref()
        .ok_or_else(|| Error::state_error(strand, "process handle is closed"))
}

/// Resolves the argument of `proc.open`, which takes either a `proc.Info` or a
/// bare PID.
///
/// A record carries the session it came from, so opening one re-checks that the
/// process is still the same process. A bare PID cannot: it is the escape hatch
/// for a PID that arrived from somewhere else — a pidfile, an argument — and it
/// races against reuse by construction.
pub(crate) async fn open_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, VfsProcess> {
    let vfs = global.local.get(strand).vfs();
    if let Some(info) = global.types.proc_info.cast(value) {
        let record = info.enter_sync(strand, |_strand, info| info.annex().info.clone());
        return vfs.open_process_info(&record).await.into_sys(strand);
    }
    let Some(pid) = value.as_int(strand) else {
        return Err(Error::type_error(
            strand,
            "open: expected a proc.Info or an Int pid",
        ));
    };
    let pid = u32::try_from(pid)
        .map_err(|_| Error::value(strand, "open: pid must fit in an unsigned 32-bit integer"))?;
    vfs.open_process(pid).await.into_sys(strand)
}

/// Implements `proc.info`.
///
/// Takes a PID rather than a record: a record already *is* the answer, and
/// re-reading one by PID could not confirm it still describes the same process
/// anyway — that is what a handle is for. Nothing is opened here, which is why
/// this reaches processes `open` cannot.
///
/// Without a PID it describes the target itself: the interpreter under a direct
/// target, the agent serving a remote one. Combined with `shell.with_host` that
/// is also how a script reaches the local interpreter while a remote target is
/// selected.
pub(crate) async fn describe<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
    pid: Option<&Value<'v>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let vfs = global.local.get(strand).vfs();
    let pid = match pid {
        Some(pid) => {
            let Some(pid) = pid.as_int(strand) else {
                return Err(Error::type_error(strand, "info: expected an Int pid"));
            };
            u32::try_from(pid).map_err(|_| {
                Error::value(strand, "info: pid must fit in an unsigned 32-bit integer")
            })?
        }
        None => vfs.pid(),
    };
    let info = vfs.describe_process(pid).await.into_sys(strand)?;
    create_info(strand, global, info, out);
    Ok(())
}

/// Implements `proc.open`, in both its plain and scoped forms.
///
/// A [`Proc`] can hold a kernel resource rather than just memory — a pidfd, a
/// Windows process handle, an entry in a remote session's object table — so it
/// follows the same scoping convention as `fs.open`: pass a block and the
/// handle is closed on the way out, whether the block returned or unwound, and
/// the call evaluates to whatever the block did. Without one the handle is
/// returned and closing it is the caller's business.
pub(crate) async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, ProcGlobal<'v>>,
    target: &Value<'v>,
    block: Option<&Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let process = open_value(strand, global, target).await?;
    let Some(block) = block else {
        global
            .types
            .proc_handle
            .create(strand, Proc(Some(process)), out);
        return Ok(());
    };
    strand
        .with_slots(async move |strand, [mut handle, mut tmp]| {
            global
                .types
                .proc_handle
                .create(strand, Proc(Some(process)), &mut handle);
            let result = call!(strand, block, out, &handle).await;
            // Closing is best-effort here: the block's outcome is the call's
            // outcome, and a close failure must not displace an error it raised.
            let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;
            result
        })
        .await
}
use dolang::runtime::value::fmt::Format;
