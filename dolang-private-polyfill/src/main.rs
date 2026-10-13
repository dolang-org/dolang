//! Minimal, byte-exact `cat`/`echo`/`grep` replacements for Do language shell
//! tests, so tests don't depend on cmd.exe built-ins (`more`, `findstr`,
//! `type`), which are line-oriented and mangle data in ways that differ
//! between real Windows and Wine.

use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(mode) = args.next() else {
        eprintln!(
            "dolang-private-polyfill: expected a subcommand (cat, echo, grep, sleep, unix-connect)"
        );
        return ExitCode::FAILURE;
    };
    let args: Vec<_> = args.collect();

    let result = match mode.to_str() {
        Some("cat") => cat(&args).map(|()| true),
        Some("echo") => echo(&args).map(|()| true),
        Some("grep") => grep(&args),
        Some("sleep") => sleep(&args).map(|()| true),
        Some("unix-connect") => unix_connect(&args).map(|()| true),
        Some("-c") => login_shell(&args).map(|()| true),
        _ => {
            eprintln!("dolang-private-polyfill: unknown subcommand {mode:?}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("dolang-private-polyfill: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Sleeps for the given number of seconds.
///
/// Exists so a test can start a process that stays alive without depending on
/// a platform shell: `sleep(1)` is POSIX-only, and cmd.exe has no equivalent
/// that behaves the same under Wine.
fn sleep(args: &[OsString]) -> io::Result<()> {
    let seconds: u64 = args
        .first()
        .and_then(|arg| arg.to_str())
        .and_then(|arg| arg.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sleep: expected seconds"))?;
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    Ok(())
}

/// Copies each file given as an argument to stdout in order, or copies
/// stdin to stdout exactly (no newline normalization, no buffering that
/// would drop a final line without a trailing newline) if no arguments are
/// given.
fn cat(args: &[OsString]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if args.is_empty() {
        io::copy(&mut io::stdin(), &mut stdout)?;
    } else {
        for path in args {
            let mut file = fs::File::open(path)?;
            io::copy(&mut file, &mut stdout)?;
        }
    }
    Ok(())
}

/// Prints its arguments separated by single spaces, followed by a newline.
fn echo(args: &[OsString]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            stdout.write_all(b" ")?;
        }
        stdout.write_all(arg.to_string_lossy().as_bytes())?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

/// Prints lines matching a regex pattern (first argument) from the given
/// files, or from stdin if none are given. Returns `Ok(false)` (mapped to a
/// failure exit code) if no line matched, mirroring standard `grep`.
fn grep(args: &[OsString]) -> io::Result<bool> {
    let mut args = args.iter();
    let pattern = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "grep: missing pattern"))?
        .to_string_lossy();
    let regex = regex::bytes::Regex::new(&pattern)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let paths: Vec<_> = args.collect();
    let mut matched = false;
    if paths.is_empty() {
        matched |= grep_reader(&regex, &mut io::stdin(), &mut stdout)?;
    } else {
        for path in paths {
            let mut file = fs::File::open(path)?;
            matched |= grep_reader(&regex, &mut file, &mut stdout)?;
        }
    }
    Ok(matched)
}

/// Reads `reader` fully, writes every line matching `regex` to `writer`
/// terminated by `\n`, and returns whether any line matched.
fn grep_reader(
    regex: &regex::bytes::Regex,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> io::Result<bool> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    // Drop a single trailing newline so a fully-terminated final line
    // doesn't produce a spurious empty line after it.
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    let mut matched = false;
    for line in data.split(|&byte| byte == b'\n') {
        if regex.is_match(line) {
            matched = true;
            writer.write_all(line)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(matched)
}

/// Byte-exact Unix socket relay. `--silent` announces readiness and only drains
/// the socket, for tests of peers that connect without starting a handshake.
#[cfg(unix)]
fn unix_connect(args: &[OsString]) -> io::Result<()> {
    use std::os::unix::net::UnixStream;
    let path = args.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unix-connect: expected socket path",
        )
    })?;
    let mut socket = UnixStream::connect(path)?;
    if args.get(1).is_some_and(|arg| arg == "--silent") {
        println!("READY");
        io::stdout().flush()?;
        io::copy(&mut socket, &mut io::sink())?;
    } else {
        let mut writer = socket.try_clone()?;
        // Stdin may remain open after the peer closes. Drive socket output on
        // the main thread so EOF ends the relay without waiting on stdin.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = io::copy(&mut io::stdin(), &mut writer)
                .and_then(|_| writer.shutdown(std::net::Shutdown::Write));
            if result.is_err() {
                let _ = writer.shutdown(std::net::Shutdown::Both);
            }
            let _ = send.send(result);
        });
        let mut output = io::stdout().lock();
        let mut buffer = [0u8; 8192];
        loop {
            let count = socket.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            // Binary protocols may never emit a newline to flush stdout.
            output.flush()?;
        }
        if let Ok(result) = receive.try_recv() {
            result?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn unix_connect(_args: &[OsString]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix-connect requires Unix",
    ))
}

/// Stand in for a login shell when the probe executes this binary as `-name -c
/// command`. Check the actual argv[0], then preserve the snapshot descriptor
/// while replacing this process with a shell that executes the command.
#[cfg(unix)]
fn login_shell(args: &[OsString]) -> io::Result<()> {
    use std::os::unix::{ffi::OsStrExt, process::CommandExt};
    let arg0 = std::env::args_os().next().unwrap_or_default();
    if !arg0.as_bytes().starts_with(b"-") || args.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "login shell: expected login-prefixed argv[0] and one command",
        ));
    }
    Err(std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&args[0])
        .env("DO_LOGIN_ARG0", arg0)
        .exec())
}

#[cfg(not(unix))]
fn login_shell(_args: &[OsString]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "login shell stand-in requires Unix",
    ))
}
