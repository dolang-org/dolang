//! Login environment probe for Unix.
//!
//! When the VFS helper is launched by `execve` into a bare environment (an SSH
//! command being the motivating case), profile-derived environment changes are
//! absent, most visibly from `PATH`. [`import`] recovers them by running the
//! account's shell as a login shell, having it re-execute this binary in the
//! [`emit`] mode, and merging the resulting snapshot into the process
//! environment.
//!
//! The probe is a separate process so that profile chatter cannot corrupt the
//! RPC stream. The snapshot travels over a dedicated pipe inherited as file
//! descriptor 3 while the probe's stdout is redirected to `/dev/null`, so the
//! payload needs no escaping: it is a sequence of NUL-terminated `NAME=VALUE`
//! records, preserving arbitrary non-UTF-8 environment bytes.
//!
//! An empty record terminates the snapshot. End-of-file is deliberately *not*
//! the completion signal: a profile that starts a background daemon leaks the
//! inherited descriptor into it, and the write end would then stay open long
//! after the probe itself exits. Reading to the terminator instead means the
//! parent stops as soon as the payload is complete, after which it closes the
//! pipe and kills the probe's entire process group, so nothing the profile
//! started outlives the import.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// File descriptor the probe writes its snapshot to.
const SNAPSHOT_FD: i32 = 3;

/// Shell used when the passwd entry does not name one.
const FALLBACK_SHELL: &str = "/bin/sh";

/// Variables describing the probe's own state rather than the login
/// environment. Importing `PWD` in particular would contradict `--cd`.
const SKIP: &[&[u8]] = &[b"PWD", b"OLDPWD", b"SHLVL", b"_"];

/// Write the current environment to [`SNAPSHOT_FD`] as NUL-terminated
/// `NAME=VALUE` records.
///
/// This is the child half of the probe, reached via the internal
/// `--login-env-probe` mode.
pub(crate) fn emit() -> io::Result<()> {
    // SAFETY: the parent installs the pipe on this descriptor before exec.
    let file = unsafe { File::from_raw_fd(SNAPSHOT_FD) };
    let mut out = io::BufWriter::new(file);

    for (name, value) in std::env::vars_os() {
        out.write_all(name.as_bytes())?;
        out.write_all(b"=")?;
        out.write_all(value.as_bytes())?;
        out.write_all(b"\0")?;
    }
    // Empty record: tells the reader the snapshot is complete without it
    // having to wait for every inheritor of this descriptor to close it.
    out.write_all(b"\0")?;
    out.flush()?;

    // Leak rather than close: the descriptor was not ours to begin with, and
    // the process is about to exit anyway.
    let _ = out
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .into_raw_fd();

    Ok(())
}

/// Run the login shell and merge its environment into this process.
///
/// `shell` overrides shell discovery when supplied. Existing variables are
/// overwritten, but variables absent from the snapshot (`SSH_CONNECTION` and
/// friends) are left in place.
///
/// # Safety
///
/// Mutates the process environment, so the caller must be single-threaded.
pub(crate) unsafe fn import(shell: Option<&OsStr>) -> io::Result<()> {
    let shell = match shell {
        Some(shell) => PathBuf::from(shell),
        None => login_shell()?,
    };

    let snapshot = run(&shell).map_err(|error| {
        io::Error::other(format!(
            "failed to probe login environment using {}: {error}: \
             omit --login-env to skip this step",
            shell.display()
        ))
    })?;

    for (name, value) in parse(&snapshot) {
        if SKIP.contains(&name) {
            continue;
        }
        // SAFETY: single-threaded, before tokio (see caller).
        unsafe { std::env::set_var(OsStr::from_bytes(name), OsStr::from_bytes(value)) };
    }

    Ok(())
}

/// Look up the current account's shell in the passwd database.
///
/// `$SHELL` is deliberately not consulted: the environment we are running in
/// is exactly the one that cannot be trusted to be complete or correct.
fn login_shell() -> io::Result<PathBuf> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .map_err(io::Error::from)?
        .ok_or_else(|| io::Error::other("no passwd entry for current user"))?;

    if user.shell.as_os_str().is_empty() {
        Ok(PathBuf::from(FALLBACK_SHELL))
    } else {
        Ok(user.shell)
    }
}

/// Run `shell` as a login shell and collect the snapshot it writes.
fn run(shell: &Path) -> io::Result<Vec<u8>> {
    let exe = std::env::current_exe()?;
    let (read, write) = nix::unistd::pipe().map_err(io::Error::from)?;
    let write_fd = write.as_raw_fd();

    let mut command = Command::new(shell);
    command
        .arg0(login_arg0(shell))
        .arg("-c")
        .arg(probe_command(&exe))
        .stdin(Stdio::null())
        // Profile output on stdout must not reach the RPC stream. stderr is
        // left attached so a broken profile can explain itself.
        .stdout(Stdio::null())
        // Own process group, so anything the profile starts can be cleaned up
        // as a unit once the snapshot has been read.
        .process_group(0);

    // SAFETY: the closure only calls async-signal-safe functions and does not
    // allocate.
    unsafe {
        command.pre_exec(move || {
            if write_fd != SNAPSHOT_FD && libc::dup2(write_fd, SNAPSHOT_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            // dup2 clears FD_CLOEXEC on the new descriptor, but does nothing at
            // all when the source already is SNAPSHOT_FD, so clear it here
            // unconditionally.
            if libc::fcntl(SNAPSHOT_FD, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    };

    let mut child = command.spawn()?;

    // Our copy of the write end would otherwise keep a truncated snapshot from
    // ever reaching end-of-file.
    drop(write);

    // Read the snapshot — and close the pipe — before reaping: a snapshot
    // larger than the pipe buffer would otherwise deadlock against a child
    // blocked in write().
    let snapshot = read_snapshot(File::from(read));

    // Let the probe exit normally before tearing down anything its profile
    // started. Besides preserving ordinary exit handling, this lets an
    // instrumented probe finish writing its coverage profile. WNOWAIT leaves
    // the leader as a zombie so its pid (and therefore its process-group id)
    // cannot be recycled before the group is killed.
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: `info` points to writable storage and `child` is our child.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as _,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }

    // Tear down everything else in the probe's process group. A group that is
    // already gone yields ESRCH, which is fine.
    //
    // SAFETY: no preconditions beyond a valid pid, checked above.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };

    let status = child.wait()?;

    // A complete snapshot is all that was wanted; how the shell went on to
    // die (including by the signal just sent) says nothing about its validity.
    match snapshot {
        Ok(snapshot) => Ok(snapshot),
        Err(error) if status.success() => Err(error),
        Err(_) => Err(io::Error::other(format!("shell exited with {status}"))),
    }
}

/// Read records from `pipe` up to the terminating empty record.
fn read_snapshot(mut pipe: File) -> io::Result<Vec<u8>> {
    let mut snapshot = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut scanned = 0;

    loop {
        if let Some(end) = terminator(&snapshot, &mut scanned) {
            snapshot.truncate(end);
            return Ok(snapshot);
        }
        match pipe.read(&mut chunk)? {
            0 => {
                return Err(io::Error::other(
                    "shell produced no complete environment snapshot",
                ));
            }
            read => snapshot.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Offset of the empty record ending `snapshot`, if it has arrived yet.
///
/// `from` carries the scan position across calls so that a growing buffer is
/// still only walked once.
fn terminator(snapshot: &[u8], from: &mut usize) -> Option<usize> {
    while let Some(&byte) = snapshot.get(*from) {
        let at = *from;
        *from += 1;
        // A record is never empty, so a NUL at the start or straight after
        // another NUL can only be the terminator.
        if byte == b'\0' && (at == 0 || snapshot[at - 1] == b'\0') {
            return Some(at);
        }
    }
    None
}

/// Build the `argv[0]` that marks `shell` as a login shell.
///
/// sshd and `login(1)` signal this by prefixing the shell's name with `-`
/// rather than by passing `-l`, and shells that predate (or simply omit) the
/// `-l` option still honour the convention.
fn login_arg0(shell: &Path) -> OsString {
    let mut arg0 = OsString::from("-");
    arg0.push(shell.file_name().unwrap_or(shell.as_os_str()));
    arg0
}

/// Build the `sh -c` command that re-executes this binary in probe mode.
fn probe_command(exe: &Path) -> OsString {
    let mut command = Vec::from(b"exec ");
    // Single-quote, escaping embedded quotes. Working in bytes keeps paths
    // that are not valid UTF-8 intact.
    command.push(b'\'');
    for byte in exe.as_os_str().as_bytes() {
        if *byte == b'\'' {
            command.extend_from_slice(b"'\\''");
        } else {
            command.push(*byte);
        }
    }
    command.push(b'\'');
    command.extend_from_slice(b" --login-env-probe");

    OsString::from_vec(command)
}

/// Split a snapshot into `(name, value)` pairs.
///
/// Records without a `=` are ignored, as is the empty tail after the final
/// NUL terminator.
fn parse(snapshot: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    snapshot
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let pos = record.iter().position(|byte| *byte == b'=')?;
            let (name, rest) = record.split_at(pos);
            (!name.is_empty()).then(|| (name, &rest[1..]))
        })
}
