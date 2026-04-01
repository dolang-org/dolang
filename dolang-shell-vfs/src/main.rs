#![deny(warnings)]

#[cfg(unix)]
fn main() {
    match dolang_shell_vfs::foreground(std::env::args().nth(1).expect("invalid arguments")) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("error: only supported on unix");
    std::process::exit(1);
}
