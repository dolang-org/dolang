//! FreeBSD and macOS process backend.
//!
//! Neither target has an analogue of Linux's pidfd. `pdfork` is the only source
//! of process descriptors — that is Capsicum's design, not an oversight — so a
//! process this one did not fork cannot be held onto, and `kill(2)` takes a PID
//! regardless. Identity is therefore established at open and re-checked before
//! anything that acts on the process, which narrows the reuse window without
//! closing it. [`crate::process::Process`] documents that as a platform
//! property rather than hiding it.
//!
//! The two targets share the shape of every operation and almost none of the
//! calls: FreeBSD reads `kinfo_proc` out of `sysctl`, macOS reads `libproc`
//! structures that carry a different subset. Each half is `cfg`-gated below
//! rather than split into files, so the common scaffolding — the batching, the
//! start-time re-check, the kqueue wait — stays written once.

use std::{
    collections::VecDeque,
    io, mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
};

use tokio::io::{Interest, unix::AsyncFd};
use uuid::Uuid;

use crate::{
    direct::unix::signal_to_raw,
    error::{Error, Result},
    path,
    process::{ProcessExit, ProcessFamily, ProcessInfo, Signal, StartTime},
    security::UnixSecurityInfo,
};

use super::{Candidate, Process, Processes, gone, recycled};

/// A null `newp` for [`sysctl`], whose mutability differs between the targets.
#[cfg(target_os = "freebsd")]
fn null_new() -> *const libc::c_void {
    ptr::null()
}

#[cfg(target_os = "macos")]
fn null_new() -> *mut libc::c_void {
    ptr::null_mut()
}

/// Reads a `sysctl` whose size is not known up front.
fn sysctl(mib: &[libc::c_int]) -> io::Result<Vec<u8>> {
    let mut len = 0usize;
    // SAFETY: `mib` is a live slice of `len` ints; a null buffer asks for the
    // size only, which is the documented sizing call.
    if unsafe {
        libc::sysctl(
            mib.as_ptr().cast_mut(),
            mib.len() as libc::c_uint,
            ptr::null_mut(),
            &mut len,
            null_new(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // The process table can grow between being sized and being read, so ask
    // for slack rather than racing it.
    len += len / 8 + 4096;
    let mut buffer = vec![0u8; len];
    // SAFETY: `buffer` is `len` bytes long and `len` is updated in place.
    if unsafe {
        libc::sysctl(
            mib.as_ptr().cast_mut(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut len,
            null_new(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len);
    Ok(buffer)
}

/// Reads a NUL-terminated C string out of a fixed-size field.
fn c_string(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Splits a NUL-separated argument block.
fn split_args(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// One process, as far as the platform's enumeration call went.
///
/// The two targets fill this at different times, which the fields cannot help
/// showing: FreeBSD's `sysctl` returns the whole table already populated, while
/// macOS's `proc_listallpids` returns bare PIDs and everything else is fetched
/// when the entry is reached. A `start` of zero is what marks the latter.
pub(super) struct Listed {
    pid: u32,
    ppid: Option<u32>,
    name: String,
    start: StartTime,
    identity: Option<UnixSecurityInfo>,
    /// Whether the process is a zombie: exited, and still listed only because
    /// its parent has not reaped it.
    exited: bool,
}

#[cfg(target_os = "freebsd")]
mod platform {
    use std::ffi::CStr;

    use super::*;

    /// Converts one `kinfo_proc` into a listing.
    fn listed(kinfo: &libc::kinfo_proc) -> Listed {
        let ngroups = kinfo.ki_ngroups.max(0) as usize;
        let exported = &kinfo.ki_groups[..ngroups.min(kinfo.ki_groups.len())];
        // The exported list always leads with the effective GID, so the
        // supplementary groups are what follows it. That held when the
        // credential stored the effective GID in the first slot of its group
        // array, and it still holds now that the two are separate fields, since
        // the export re-prepends it to keep `kinfo_proc` ABI-compatible.
        let egid = exported.first().copied();
        let groups = exported.get(1..).unwrap_or_default().to_vec();
        Listed {
            pid: kinfo.ki_pid as u32,
            ppid: (kinfo.ki_ppid > 0).then_some(kinfo.ki_ppid as u32),
            name: c_string(&kinfo.ki_comm),
            start: StartTime(
                (kinfo.ki_start.tv_sec as u64) * 1_000_000 + (kinfo.ki_start.tv_usec as u64),
            ),
            identity: Some(UnixSecurityInfo {
                uid: kinfo.ki_ruid,
                gid: kinfo.ki_rgid,
                euid: kinfo.ki_uid,
                egid: egid.unwrap_or(kinfo.ki_rgid),
                groups,
            }),
            exited: kinfo.ki_stat == libc::SZOMB,
        }
    }

    /// Splits a `sysctl` result into `kinfo_proc` records.
    ///
    /// The stride comes from each record's own `ki_structsize` rather than from
    /// `size_of`, which is what makes this survive a kernel whose struct is
    /// larger than the one this was built against.
    fn parse_table(bytes: &[u8]) -> Vec<Listed> {
        let mut listed = Vec::new();
        let mut offset = 0;
        while offset + mem::size_of::<libc::kinfo_proc>() <= bytes.len() {
            // SAFETY: the remaining bytes are at least one record long, and
            // `read_unaligned` makes no alignment assumption about the buffer.
            let kinfo: libc::kinfo_proc =
                unsafe { ptr::read_unaligned(bytes[offset..].as_ptr().cast()) };
            let stride = kinfo.ki_structsize as usize;
            if stride == 0 {
                break;
            }
            listed.push(self::listed(&kinfo));
            offset += stride;
        }
        listed
    }

    pub(super) fn scan() -> Result<Vec<Listed>> {
        let mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PROC];
        Ok(parse_table(&sysctl(&mib)?))
    }

    /// Reads one process's `kinfo_proc`, or `None` if it is gone.
    pub(super) fn lookup(pid: u32) -> Result<Option<Listed>> {
        let mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        match sysctl(&mib) {
            Ok(bytes) => Ok(parse_table(&bytes).into_iter().next()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn exe(pid: u32) -> Option<path::PathBuf> {
        let mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PATHNAME,
            pid as libc::c_int,
        ];
        let bytes = sysctl(&mib).ok()?;
        let path = CStr::from_bytes_until_nul(&bytes).ok()?.to_str().ok()?;
        // A process with no executable — a kernel process, or one whose image
        // the kernel cannot name — answers with an empty string rather than an
        // error. That is an absent path, not a path that happens to be empty.
        if path.is_empty() {
            return None;
        }
        path::PathBuf::from_native(path.into()).ok()
    }

    /// Reads one process's working directory.
    ///
    /// `KERN_PROC_CWD` answers with a single `kinfo_file`, which is the same
    /// kernel data `libprocstat` returns — that library's contribution is
    /// walking the whole descriptor table to find this one entry, which is not
    /// worth linking a second library and hand-rolling the `struct filestat`
    /// layout for.
    ///
    /// Subject to `p_candebug`, so another user's working directory is denied
    /// rather than reported.
    pub(super) fn cwd(pid: u32) -> Option<path::PathBuf> {
        let mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_CWD,
            pid as libc::c_int,
        ];
        let bytes = sysctl(&mib).ok()?;
        if bytes.len() < mem::size_of::<libc::kinfo_file>() {
            return None;
        }
        // SAFETY: the buffer holds at least one record, and `read_unaligned`
        // makes no alignment assumption about it.
        let kinfo: libc::kinfo_file = unsafe { ptr::read_unaligned(bytes.as_ptr().cast()) };
        // The record leads with its own size so that a consumer can tell it is
        // reading the layout it was built against. `libc` describes the middle
        // of the structure as opaque padding standing in for a union, so a
        // disagreement here would move `kf_path` rather than merely truncate
        // it — refuse the record instead of reading a path out of the wrong
        // offset.
        if kinfo.kf_structsize as usize != mem::size_of::<libc::kinfo_file>() {
            return None;
        }
        let path = c_string(&kinfo.kf_path);
        (!path.is_empty())
            .then(|| path::PathBuf::from_native(path.into()).ok())
            .flatten()
    }

    pub(super) fn cmdline(pid: u32) -> Option<Vec<String>> {
        let mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_ARGS,
            pid as libc::c_int,
        ];
        let bytes = sysctl(&mib).ok()?;
        let args = split_args(&bytes);
        (!args.is_empty()).then_some(args)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// The prefix of macOS's `struct kinfo_proc`, up to and including the
    /// credentials.
    ///
    /// `libc` has no `kinfo_proc` for Apple, and `libproc` — which it does
    /// wrap — carries no supplementary group list, so the only route to one is
    /// to transcribe the layout from `<sys/sysctl.h>` and `<sys/proc.h>`. It
    /// is a frozen binary-compatibility ABI, so this is stable, but it is
    /// still a hand copy of a header and is treated as such: nothing below is
    /// believed until [`Ucred`] has been checked against an independent
    /// reading of the same credentials, and a mismatch degrades to no group
    /// list rather than to a wrong one.
    ///
    /// Only the prefix is declared. `e_ucred` is followed by `struct vmspace`
    /// and the tty and session fields, none of which are wanted here, and
    /// leaving them out removes them as a source of transcription error.
    /// Pointer fields are `usize` rather than raw pointers: identical in size
    /// and alignment on a 64-bit target, and they are never dereferenced.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KinfoProcPrefix {
        kp_proc: ExternProc,
        e_paddr: usize,
        e_sess: usize,
        e_pcred: Pcred,
        e_ucred: Ucred,
    }

    /// `struct extern_proc` from `<sys/proc.h>`.
    ///
    /// Declared in full only because the credentials sit behind it, and its
    /// size is what places them.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExternProc {
        /// A union of two list pointers and a `timeval`, both 16 bytes.
        p_un: [usize; 2],
        p_vmspace: usize,
        p_sigacts: usize,
        p_flag: libc::c_int,
        p_stat: libc::c_char,
        p_pid: libc::pid_t,
        p_oppid: libc::pid_t,
        p_dupfd: libc::c_int,
        user_stack: usize,
        exit_thread: usize,
        p_debugger: libc::c_int,
        sigwait: libc::c_int,
        p_estcpu: libc::c_uint,
        p_cpticks: libc::c_int,
        /// `fixpt_t`
        p_pctcpu: u32,
        p_wchan: usize,
        p_wmesg: usize,
        p_swtime: libc::c_uint,
        p_slptime: libc::c_uint,
        p_realtimer: libc::itimerval,
        p_rtime: libc::timeval,
        p_uticks: u64,
        p_sticks: u64,
        p_iticks: u64,
        p_traceflag: libc::c_int,
        p_tracep: usize,
        p_siglist: libc::c_int,
        p_textvp: usize,
        p_holdcnt: libc::c_int,
        p_sigmask: libc::sigset_t,
        p_sigignore: libc::sigset_t,
        p_sigcatch: libc::sigset_t,
        p_priority: u8,
        p_usrpri: u8,
        p_nice: libc::c_char,
        p_comm: [libc::c_char; libc::MAXCOMLEN + 1],
        p_pgrp: usize,
        p_addr: usize,
        p_xstat: u16,
        p_acflag: u16,
        p_ru: usize,
    }

    /// `struct _pcred` from `<sys/sysctl.h>`.
    ///
    /// Its four ID fields are the first half of the cross-check: they sit
    /// immediately before [`Ucred`], so agreeing on them means the layout is
    /// aligned right up to the group list.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Pcred {
        pc_lock: [libc::c_char; 72],
        pc_ucred: usize,
        p_ruid: libc::uid_t,
        p_svuid: libc::uid_t,
        p_rgid: libc::gid_t,
        p_svgid: libc::gid_t,
        p_refcnt: libc::c_int,
    }

    /// `struct _ucred` from `<sys/sysctl.h>`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Ucred {
        cr_ref: i32,
        cr_uid: libc::uid_t,
        cr_ngroups: libc::c_short,
        cr_groups: [libc::gid_t; NGROUPS],
    }

    /// The kernel credential group list is fixed at `NGROUPS` entries, so a
    /// process in more groups than this reports a truncated list even to
    /// itself.
    const NGROUPS: usize = 16;

    /// Reads the supplementary group list for `pid`.
    ///
    /// Returns an empty list rather than a doubtful one. `bsd` is an
    /// independent reading of the same credentials through `libproc`, and
    /// every ID the two structures share has to agree before the group list
    /// is believed — five fields, four of them immediately preceding the
    /// groups, which is a far stronger check on the transcribed layout than
    /// the size of the buffer.
    fn groups(bsd: &libc::proc_bsdinfo) -> Vec<libc::gid_t> {
        let pid = bsd.pbi_pid;
        let mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        let Ok(bytes) = sysctl(&mib) else {
            return Vec::new();
        };
        if bytes.len() < mem::size_of::<KinfoProcPrefix>() {
            return Vec::new();
        }
        // SAFETY: the buffer is at least one prefix long, and
        // `read_unaligned` assumes nothing about its alignment. Every field is
        // a plain integer or an integer standing in for a pointer, so any bit
        // pattern is a valid value.
        let info: KinfoProcPrefix = unsafe { ptr::read_unaligned(bytes.as_ptr().cast()) };

        let agrees = info.kp_proc.p_pid as u32 == pid
            && info.e_pcred.p_ruid == bsd.pbi_ruid
            && info.e_pcred.p_svuid == bsd.pbi_svuid
            && info.e_pcred.p_rgid == bsd.pbi_rgid
            && info.e_pcred.p_svgid == bsd.pbi_svgid
            && info.e_ucred.cr_uid == bsd.pbi_uid;
        if !agrees {
            return Vec::new();
        }
        let count = info.e_ucred.cr_ngroups;
        if count < 0 || count as usize > NGROUPS {
            return Vec::new();
        }
        // The credential keeps the effective GID in the first slot of its group
        // array, so the supplementary groups are what follows it. Dropping it
        // is what keeps this field meaning the same thing on every target;
        // `egid` reports it, out of `pbi_gid`.
        info.e_ucred.cr_groups[..count as usize]
            .get(1..)
            .unwrap_or_default()
            .to_vec()
    }

    /// Reads `PROC_PIDTBSDINFO` for one process.
    fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
        let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a live, correctly sized out-parameter.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        (read == size).then_some(info)
    }

    fn listed_from(info: &libc::proc_bsdinfo) -> Listed {
        Listed {
            pid: info.pbi_pid,
            ppid: (info.pbi_ppid != 0).then_some(info.pbi_ppid),
            // `pbi_name` is the longer accounting name and `pbi_comm` the
            // truncated one; prefer the former when the kernel has it.
            name: match c_string(&info.pbi_name) {
                name if name.is_empty() => c_string(&info.pbi_comm),
                name => name,
            },
            start: StartTime(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
            identity: Some(UnixSecurityInfo {
                uid: info.pbi_ruid,
                gid: info.pbi_rgid,
                euid: info.pbi_uid,
                egid: info.pbi_gid,
                groups: groups(info),
            }),
            exited: info.pbi_status == libc::SZOMB,
        }
    }

    pub(super) fn scan() -> Result<Vec<Listed>> {
        // SAFETY: a null buffer asks for the required size.
        let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        // The table can grow between being sized and being read.
        let mut pids = vec![0i32; count as usize + 64];
        let size = (pids.len() * mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: `pids` is `size` bytes long.
        let read = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        if read <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        pids.truncate(read as usize);
        Ok(pids
            .into_iter()
            .filter(|pid| *pid > 0)
            .map(|pid| Listed {
                pid: pid as u32,
                ppid: None,
                name: String::new(),
                start: StartTime(0),
                identity: None,
                // A bare PID from `proc_listallpids`; everything, this
                // included, is filled in when the entry is reached.
                exited: false,
            })
            .collect())
    }

    pub(super) fn lookup(pid: u32) -> Result<Option<Listed>> {
        Ok(bsdinfo(pid).as_ref().map(listed_from))
    }

    pub(super) fn exe(pid: u32) -> Option<path::PathBuf> {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buffer` is as long as the length passed.
        let len = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if len <= 0 {
            return None;
        }
        buffer.truncate(len as usize);
        let path = String::from_utf8(buffer).ok()?;
        path::PathBuf::from_native(path.into()).ok()
    }

    pub(super) fn cwd(pid: u32) -> Option<path::PathBuf> {
        let mut info: libc::proc_vnodepathinfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: `info` is a live, correctly sized out-parameter.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        // `vip_path` is declared as nested arrays only to keep `libc` building
        // on older compilers; it is one flat `[c_char; MAXPATHLEN]`.
        let path = info.pvi_cdir.vip_path.as_flattened();
        let path = c_string(path);
        (!path.is_empty())
            .then(|| path::PathBuf::from_native(path.into()).ok())
            .flatten()
    }

    /// Reads the argument vector via `KERN_PROCARGS2`.
    ///
    /// The block starts with `argc`, then the executable path, then NUL-padded
    /// alignment, then the arguments themselves. Only readable for processes
    /// owned by the same user, which is reported the same way as being absent.
    pub(super) fn cmdline(pid: u32) -> Option<Vec<String>> {
        let mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let bytes = sysctl(&mib).ok()?;
        if bytes.len() < mem::size_of::<libc::c_int>() {
            return None;
        }
        let argc = libc::c_int::from_ne_bytes(bytes[..4].try_into().ok()?).max(0) as usize;
        // Skip the leading executable path and the padding that follows it.
        let rest = &bytes[4..];
        let exec_end = rest.iter().position(|byte| *byte == 0)?;
        let args_start = exec_end + rest[exec_end..].iter().take_while(|b| **b == 0).count();
        let mut args = split_args(&rest[args_start..]);
        args.truncate(argc);
        (!args.is_empty()).then_some(args)
    }
}

/// Builds a full record from a listing.
fn describe(session: Uuid, listed: Listed) -> ProcessInfo {
    let pid = listed.pid;
    ProcessInfo {
        session,
        pid,
        ppid: listed.ppid,
        name: listed.name,
        start: listed.start,
        exe: platform::exe(pid),
        cmdline: platform::cmdline(pid),
        cwd: platform::cwd(pid),
        family: ProcessFamily::Unix(listed.identity),
        // Only the parent may reap, and this process is not it, so a zombie is
        // reported as gone and nothing more.
        exit: listed.exited.then_some(ProcessExit { code: None }),
    }
}

impl Processes {
    pub(super) async fn impl_scan() -> Result<Vec<Candidate>> {
        tokio::task::spawn_blocking(platform::scan)
            .await
            .map_err(Error::other)?
    }

    pub(super) async fn impl_describe_one(session: Uuid, pid: u32) -> Result<ProcessInfo> {
        tokio::task::spawn_blocking(move || {
            let listed = platform::lookup(pid)?.ok_or_else(|| gone(pid))?;
            Ok(describe(session, listed))
        })
        .await
        .map_err(Error::other)?
    }

    pub(super) async fn impl_describe(
        session: Uuid,
        batch: Vec<Candidate>,
    ) -> Result<VecDeque<ProcessInfo>> {
        tokio::task::spawn_blocking(move || {
            Ok(batch
                .into_iter()
                .filter_map(|listed| {
                    // A listing whose core fields were not filled at scan time
                    // is re-read here, and dropped if the process has since
                    // gone; one that was is used as it stands.
                    let listed = if listed.start == StartTime(0) {
                        platform::lookup(listed.pid).ok().flatten()?
                    } else {
                        listed
                    };
                    Some(describe(session, listed))
                })
                .collect())
        })
        .await
        .map_err(Error::other)?
    }
}

impl Process {
    pub(super) async fn impl_open(
        session: Uuid,
        pid: u32,
        start: Option<StartTime>,
    ) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            let listed = platform::lookup(pid)?.ok_or_else(|| gone(pid))?;
            if let Some(expected) = start
                && listed.start != expected
            {
                return Err(recycled(pid));
            }
            Ok(Self {
                pid,
                session,
                start: listed.start,
            })
        })
        .await
        .map_err(Error::other)?
    }

    /// Confirms the PID still names the process this handle was opened for.
    ///
    /// This is the whole of the identity guarantee on these targets, and it is
    /// a check-then-act: the process can still be replaced between here and the
    /// `kill` that follows. It is nonetheless worth doing — the window is
    /// microseconds rather than the whole lifetime of the handle.
    fn verify(&self) -> Result<()> {
        let listed = platform::lookup(self.pid)?.ok_or_else(|| gone(self.pid))?;
        if listed.start != self.start {
            return Err(recycled(self.pid));
        }
        Ok(())
    }

    pub(super) async fn impl_info(&self) -> Result<ProcessInfo> {
        let (pid, session, start) = (self.pid, self.session, self.start);
        tokio::task::spawn_blocking(move || {
            let listed = platform::lookup(pid)?.ok_or_else(|| gone(pid))?;
            if listed.start != start {
                return Err(recycled(pid));
            }
            Ok(describe(session, listed))
        })
        .await
        .map_err(Error::other)?
    }

    pub(super) async fn impl_signal(&self, signal: Signal) -> Result<()> {
        let (pid, start) = (self.pid, self.start);
        let signal = signal_to_raw(signal)?;
        tokio::task::spawn_blocking(move || {
            let this = Self {
                pid,
                session: Uuid::nil(),
                start,
            };
            this.verify()?;
            // SAFETY: `kill` takes two scalars.
            if unsafe { libc::kill(pid as libc::pid_t, signal) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(Error::other)?
    }

    pub(super) async fn impl_terminate(&self) -> Result<()> {
        self.impl_signal(Signal::Term).await
    }

    pub(super) async fn impl_kill(&self) -> Result<()> {
        self.impl_signal(Signal::Kill).await
    }

    /// Waits for the process to exit, through a kqueue of its own.
    ///
    /// One kqueue per wait is wasteful in principle and irrelevant in practice:
    /// these are one-off waits, and a shared kqueue would need its own
    /// dispatch layer to route events back to the right caller.
    pub(super) async fn impl_wait(&self) -> Result<ProcessExit> {
        // SAFETY: `kqueue` takes no arguments and returns a descriptor or -1.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: the descriptor is fresh and owned by nothing else.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };

        // Registration is what makes this race-free: `EV_ADD` fails with
        // `ESRCH` if the process is already gone, so there is no window between
        // deciding to wait and being subscribed.
        match register_exit(queue.as_raw_fd(), self.pid) {
            Ok(()) => {}
            // Already gone is the outcome the caller was waiting for.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                return Ok(ProcessExit { code: None });
            }
            Err(error) => return Err(error.into()),
        }

        // A kqueue descriptor is itself readable when it holds an event, which
        // is what lets the reactor drive this instead of a blocking `kevent`.
        //
        // Read interest only, and not merely as an optimization: a kqueue
        // descriptor supports `EVFILT_READ` but has no write filter, so the
        // read/write registration `AsyncFd::register` performs is rejected
        // outright with `EINVAL`.
        //
        // SAFETY: `watch` owns the `OwnedFd` and never replaces it, so the
        // descriptor stays open and unchanged until it is dropped.
        let watch = unsafe { AsyncFd::register_with_interest(queue, Interest::READABLE) }
            .map_err(io::Error::from)?;
        loop {
            let mut guard = watch.readable().await?;
            if drain_exit(watch.get_ref().as_raw_fd())? {
                // `NOTE_EXITSTATUS` is restricted to the parent, so the exit
                // code is unavailable here even though the event carries a
                // slot for it.
                return Ok(ProcessExit { code: None });
            }
            // Readiness without an event: clear it and wait again rather than
            // reporting an exit that did not happen.
            guard.clear_ready();
        }
    }

    pub(super) async fn impl_close(self) -> Result<()> {
        Ok(())
    }
}

/// Subscribes a kqueue to one process's exit.
///
/// Split out and kept synchronous because `kevent` holds a raw `udata`
/// pointer, which would make any future holding one non-`Send`.
fn register_exit(queue: libc::c_int, pid: u32) -> io::Result<()> {
    let mut change: libc::kevent = unsafe { mem::zeroed() };
    change.ident = pid as usize;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: one live change entry, no result entries requested.
    if unsafe { libc::kevent(queue, &change, 1, ptr::null_mut(), 0, &raw const timeout) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads one pending event, reporting whether there was one.
///
/// The zero timeout makes this a non-blocking drain of whatever readiness
/// reported, never a second place that waits.
fn drain_exit(queue: libc::c_int) -> io::Result<bool> {
    let mut event: libc::kevent = unsafe { mem::zeroed() };
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: one result entry into a live `event`.
    let count = unsafe { libc::kevent(queue, ptr::null(), 0, &mut event, 1, &raw const timeout) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count > 0)
}

#[cfg(all(test, target_os = "freebsd"))]
mod tests {
    use super::*;

    /// Proves the exported group list leads with the effective GID, which is
    /// what [`platform::listed`] strips to arrive at the supplementary groups.
    ///
    /// The two kernels this has to hold on disagree about what `getgroups`
    /// returns — it leads with the effective GID where the credential keeps the
    /// two in one array, and does not where they are separate fields — so the
    /// assertion is that the stripped list is what remains either way.
    #[test]
    fn kinfo_group_list_leads_with_the_effective_gid() {
        let mut reported = vec![0 as libc::gid_t; 128];
        // SAFETY: the buffer is as long as the count passed.
        let count =
            unsafe { libc::getgroups(reported.len() as libc::c_int, reported.as_mut_ptr()) };
        assert!(
            count >= 0,
            "getgroups failed: {}",
            io::Error::last_os_error()
        );
        reported.truncate(count as usize);

        let listed = platform::lookup(std::process::id())
            .unwrap()
            .expect("the current process should be listed");
        let identity = listed
            .identity
            .expect("FreeBSD reports credentials for every process");
        let egid = unsafe { libc::getegid() };

        assert_eq!(identity.effective_gid(), egid);

        let stripped = identity.groups();
        let prepended: Vec<libc::gid_t> = std::iter::once(egid)
            .chain(stripped.iter().copied())
            .collect();
        assert!(
            reported == stripped || reported == prepended,
            "supplementary groups {stripped:?} match neither {reported:?} nor that list \
             without its leading effective GID"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Proves the hand-transcribed `kinfo_proc` prefix lines up with the
    /// kernel's, by checking the group list it produces against the one the
    /// process can read about itself.
    ///
    /// `getgroups` reports the credential's group array whole, so what it
    /// returns is the effective GID followed by the supplementary groups this
    /// strips it down to.
    ///
    /// A misread layout fails the credential cross-check and yields an empty
    /// list, so asserting the list is *non-empty* is what catches a bad
    /// transcription — every process belongs to at least one group.
    #[test]
    fn kinfo_group_list_matches_getgroups() {
        let mut expected = vec![0 as libc::gid_t; 32];
        // SAFETY: the buffer is as long as the count passed.
        let count =
            unsafe { libc::getgroups(expected.len() as libc::c_int, expected.as_mut_ptr()) };
        assert!(
            count >= 0,
            "getgroups failed: {}",
            io::Error::last_os_error()
        );
        expected.truncate(count as usize);

        let listed = platform::lookup(std::process::id())
            .unwrap()
            .expect("the current process should be listed");
        let identity = listed
            .identity
            .expect("macOS reports credentials for every process");

        assert!(
            !expected.is_empty(),
            "empty group list means the kinfo_proc layout failed its credential cross-check"
        );
        let prepended: Vec<libc::gid_t> = std::iter::once(identity.effective_gid())
            .chain(identity.groups().iter().copied())
            .collect();
        assert_eq!(prepended, expected);
        assert_eq!(identity.effective_gid(), unsafe { libc::getegid() });
        assert_eq!(identity.effective_uid(), unsafe { libc::geteuid() });
        assert_eq!(identity.uid(), unsafe { libc::getuid() });
    }
}
