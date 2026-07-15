#![deny(warnings)]

use std::error::Error;

#[cfg(unix)]
fn socket(path: &str) -> Result<(), Box<dyn Error>> {
    dolang_shell_vfs::foreground(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn socket(_path: &str) -> Result<(), Box<dyn Error>> {
    Err("Unix socket mode is only supported on Unix".into())
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing socket path or --stdio")?;
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    if mode == "--stdio" {
        dolang_shell_vfs::serve_stdio()?;
        Ok(())
    } else {
        socket(&mode)
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
