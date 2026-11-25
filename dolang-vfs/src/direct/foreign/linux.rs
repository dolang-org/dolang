//! Linux process backend.
//!
//! Identity comes from a pidfd, information from `/proc`. The pidfd is what
//! makes the pair safe to combine: holding one pins the PID number against
//! reuse for as long as the descriptor is open, so a `/proc/<pid>` read behind
//! a live pidfd either describes the same process or fails outright. Without
//! it, every path here would be a check-then-act against a number the kernel
//! is free to hand to something else.
//!
//! `pidfd_open` landed in 5.3 and `pidfd_send_signal` in 5.1, both old enough
//! to require unconditionally. They are issued as raw syscalls: `libc` carries
//! the per-architecture numbers but no wrappers, and `nix` has no pidfd support
//! at all, so this avoids a dependency rather than adding one.

use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    io::{self, ErrorKind as IoErrorKind},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::ffi::OsStringExt,
    },
};

use tokio::io::unix::AsyncFd;
use uuid::Uuid;

use crate::{
    direct::unix::signal_to_raw,
    error::{Error, Result},
    path,
    process::{ProcessExit, ProcessFamily, ProcessInfo, Signal, StartTime},
    security::UnixSecurityInfo,
};

use super::{Candidate, Process, Processes, gone, recycled};

/// Opens a pidfd for `pid`.
fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: `pidfd_open` takes a PID and a flag word and returns a new file
    // descriptor or -1. No pointers are involved.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the syscall returned a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// Sends `signal` to the process a pidfd refers to.
fn pidfd_send_signal(fd: BorrowedFd<'_>, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: the null `siginfo_t` pointer is the documented way to ask the
    // kernel to synthesize one, exactly as `kill(2)` does.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null_mut::<libc::siginfo_t>(),
            0 as libc::c_uint,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The fields of `/proc/<pid>/stat` this backend reads.
struct Stat {
    name: String,
    /// The `state` character, of which only `Z` (zombie) and `X` (dead) mean
    /// the process has exited.
    state: char,
    ppid: Option<u32>,
    start: StartTime,
}

/// Parses `/proc/<pid>/stat`.
///
/// The `comm` field is unquoted and unescaped, and a process can put anything
/// in it — spaces and parentheses included — so the only reliable split is
/// around the *last* `)` in the line rather than around whitespace.
fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_owned();
    let mut rest = text.get(close + 1..)?.split_ascii_whitespace();
    // Fields are 1-based and `comm` is field 2, so `rest` starts at field 3.
    let state = rest.next()?.chars().next()?;
    let ppid = rest.next()?.parse().ok();
    // `starttime` is field 22, four past `ppid`'s field 4.
    let start = rest.nth(17)?.parse().ok()?;
    Some(Stat {
        name,
        state,
        ppid,
        start: StartTime(start),
    })
}

/// Reads the credentials from `/proc/<pid>/status`.
///
/// The `Uid:`/`Gid:` lines carry real, effective, saved-set, and filesystem IDs
/// in that order; only the first two of each are represented here.
fn parse_status(text: &str) -> Option<UnixSecurityInfo> {
    fn pair(line: &str) -> Option<(u32, u32)> {
        let mut fields = line.split_ascii_whitespace();
        Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
    }

    let mut ids = None;
    let mut gids = None;
    let mut groups = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            ids = pair(rest);
        } else if let Some(rest) = line.strip_prefix("Gid:") {
            gids = pair(rest);
        } else if let Some(rest) = line.strip_prefix("Groups:") {
            groups = rest
                .split_ascii_whitespace()
                .filter_map(|group| group.parse().ok())
                .collect();
        }
    }
    let (uid, euid) = ids?;
    let (gid, egid) = gids?;
    Some(UnixSecurityInfo {
        uid,
        gid,
        euid,
        egid,
        groups,
    })
}

/// Reads `/proc/<pid>/cmdline`.
///
/// NUL-separated, usually with a trailing NUL. Empty for kernel threads, and
/// for a process whose `argv` the kernel cannot reach, which is reported the
/// same way as being unreadable.
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

/// Resolves one of the `/proc/<pid>` symlinks.
///
/// Returns `None` rather than an error for the common denials: reading another
/// user's `exe` or `cwd` needs `PTRACE_MODE_READ`, and a kernel thread has
/// neither.
fn read_link(pid: u32, name: &str) -> Option<path::PathBuf> {
    let target = fs::read_link(format!("/proc/{pid}/{name}")).ok()?;
    // A deleted executable resolves to "<path> (deleted)", which is not a path
    // the caller can use, but it is still the most accurate answer available.
    path::PathBuf::from_native(target).ok()
}

/// Builds a snapshot for `pid`, or `None` if it has gone away.
///
/// Only the `stat` read distinguishes "gone" from "here": everything after it
/// is optional, so a process that exits midway through is reported with
/// whatever was already collected rather than being dropped.
fn read_info(session: Uuid, pid: u32) -> Result<Option<ProcessInfo>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error)
            if matches!(
                error.kind(),
                IoErrorKind::NotFound | IoErrorKind::PermissionDenied
            ) || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            // A process that exits between the `/proc` listing and this read
            // can surface as `ESRCH` rather than `ENOENT` — e.g. reading a
            // zombie's `stat` file after its parent reaps it. Rust does not
            // map that errno to `NotFound`, so it needs a check of its own.
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let Some(Stat {
        name,
        state,
        ppid,
        start,
    }) = parse_stat(&stat)
    else {
        return Ok(None);
    };
    let identity = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|text| parse_status(&text));
    Ok(Some(ProcessInfo {
        session,
        pid,
        ppid,
        name,
        start,
        exe: read_link(pid, "exe"),
        cmdline: read_cmdline(pid),
        cwd: read_link(pid, "cwd"),
        family: ProcessFamily::Unix(identity),
        // A zombie is still listed, and still has a `/proc` entry, until its
        // parent reaps it. Only the parent learns the status, so this reports
        // the fact and nothing else.
        exit: matches!(state, 'Z' | 'X').then_some(ProcessExit { code: None }),
    }))
}

impl Processes {
    /// Lists the numeric entries of `/proc`.
    ///
    /// Captured once. `/proc` offers no atomic snapshot, so re-listing later
    /// would not make the result more coherent, only more expensive.
    pub(super) async fn impl_scan() -> Result<Vec<Candidate>> {
        tokio::task::spawn_blocking(|| {
            let mut pids = Vec::new();
            for entry in fs::read_dir("/proc")? {
                let name = OsString::from_vec(entry?.file_name().into_vec());
                if let Some(pid) = name.to_str().and_then(|name| name.parse().ok()) {
                    pids.push(pid);
                }
            }
            Ok(pids)
        })
        .await
        .map_err(Error::other)?
    }

    pub(super) async fn impl_describe_one(session: Uuid, pid: u32) -> Result<ProcessInfo> {
        tokio::task::spawn_blocking(move || read_info(session, pid)?.ok_or_else(|| gone(pid)))
            .await
            .map_err(Error::other)?
    }

    pub(super) async fn impl_describe(
        session: Uuid,
        batch: Vec<Candidate>,
    ) -> Result<VecDeque<ProcessInfo>> {
        tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .filter_map(|pid| read_info(session, pid).transpose())
                .collect()
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
            // `ESRCH` here is the ordinary "no such process", but Rust leaves
            // it uncategorized, so without this it reaches the caller as an
            // opaque errno rather than as a not-found.
            let pidfd = pidfd_open(pid).map_err(|error| match error.raw_os_error() {
                Some(libc::ESRCH) => gone(pid),
                _ => error.into(),
            })?;
            // Ordering is what makes this safe: the pidfd is open before the
            // start time is read, so the PID cannot be recycled in between. A
            // mismatch therefore means the process was already the wrong one,
            // not that it became so while being checked.
            if let Some(expected) = start {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
                if parse_stat(&stat).map(|stat| stat.start) != Some(expected) {
                    return Err(recycled(pid));
                }
            }
            Ok(Self {
                pid,
                session,
                pidfd,
            })
        })
        .await
        .map_err(Error::other)?
    }

    pub(super) async fn impl_info(&self) -> Result<ProcessInfo> {
        let (pid, session) = (self.pid, self.session);
        tokio::task::spawn_blocking(move || read_info(session, pid)?.ok_or_else(|| gone(pid)))
            .await
            .map_err(Error::other)?
    }

    pub(super) async fn impl_signal(&self, signal: Signal) -> Result<()> {
        pidfd_send_signal(self.pidfd.as_fd(), signal_to_raw(signal)?)?;
        Ok(())
    }

    pub(super) async fn impl_terminate(&self) -> Result<()> {
        self.impl_signal(Signal::Term).await
    }

    pub(super) async fn impl_kill(&self) -> Result<()> {
        self.impl_signal(Signal::Kill).await
    }

    pub(super) async fn impl_wait(&self) -> Result<ProcessExit> {
        // A pidfd becomes readable when its process exits, so readiness alone
        // is the answer and the descriptor is never read. It is duplicated
        // first because the reactor registers by descriptor number, and two
        // concurrent waits on one handle would otherwise collide.
        //
        // SAFETY: `watch` owns the duplicate and never replaces it, so the
        // descriptor stays open and unchanged until it is dropped.
        let watch =
            unsafe { AsyncFd::register(self.pidfd.try_clone()?) }.map_err(io::Error::from)?;
        let _guard = watch.readable().await?;
        // Only the parent may reap, and this process is by definition not it,
        // so `waitid(P_PIDFD)` would fail with ECHILD. Exit is all there is.
        Ok(ProcessExit { code: None })
    }

    pub(super) async fn impl_close(self) -> Result<()> {
        drop(self.pidfd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parses_around_the_last_paren() {
        // A `comm` containing both a space and a parenthesis, which is legal
        // and defeats any whitespace-based split.
        let line = "42 (od d) ne) S 7 42 42 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 99887766 \
                    1234 5678";
        let stat = parse_stat(line).expect("stat should parse");
        assert_eq!(stat.name, "od d) ne");
        assert_eq!(stat.ppid, Some(7));
        assert_eq!(stat.start, StartTime(99887766));
    }

    #[test]
    fn status_takes_real_and_effective_ids() {
        let text = "Name:\tsh\nUid:\t1000\t1001\t1002\t1003\nGid:\t100\t101\t102\t103\n\
                    Groups:\t4 24 27\n";
        let identity = parse_status(text).expect("status should parse");
        assert_eq!(identity.uid(), 1000);
        assert_eq!(identity.effective_uid(), 1001);
        assert_eq!(identity.gid(), 100);
        assert_eq!(identity.effective_gid(), 101);
        assert_eq!(identity.groups(), [4, 24, 27]);
    }

    #[test]
    fn status_without_credentials_is_rejected() {
        assert!(parse_status("Name:\tsh\n").is_none());
    }

    #[tokio::test]
    async fn enumeration_finds_the_current_process() {
        let session = Uuid::new_v4();
        let mut processes = Processes::open(session).await.unwrap();
        let self_pid = std::process::id();
        let mut found = None;
        while let Some(info) = processes.next_entry().await.unwrap() {
            if info.pid() == self_pid {
                found = Some(info);
                break;
            }
        }
        let found = found.expect("the current process should be in the table");
        assert_eq!(found.session(), session);
        assert!(found.identity().unwrap().is_some());
        // The record knows it came from a Unix target, so there is no token to
        // be absent — asking for one is an error, not a `None`.
        assert_eq!(
            found.token().unwrap_err().kind(),
            crate::error::ErrorKind::Unsupported
        );
    }

    #[tokio::test]
    async fn open_accepts_the_observed_start_time_and_rejects_any_other() {
        let session = Uuid::new_v4();
        let self_pid = std::process::id();
        let process = Process::open(session, self_pid, None).await.unwrap();
        let info = process.info().await.unwrap();
        assert_eq!(info.pid(), self_pid);

        assert!(
            Process::open(session, self_pid, Some(info.start_time()))
                .await
                .is_ok(),
            "matching start time should open"
        );

        let StartTime(raw) = info.start_time();
        let error = Process::open(session, self_pid, Some(StartTime(raw + 1)))
            .await
            .err()
            .expect("mismatched start time should be rejected");
        assert_eq!(error.kind(), crate::error::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn wait_returns_once_a_child_exits() {
        // Uses a child only because it is the one process this test can be
        // sure will exit; it is opened as a stranger, by PID, with no
        // parent-child relationship in play.
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let pid = child.id().expect("a spawned child has a PID");
        let process = Process::open(Uuid::new_v4(), pid, None).await.unwrap();

        let exit = process.wait().await.unwrap();
        // Linux cannot report a foreign exit code even when one exists: only
        // the parent may reap, and this handle is not it.
        assert_eq!(exit.code(), None);

        let status = child.wait().await.unwrap();
        assert_eq!(status.code(), Some(3));
    }
}
