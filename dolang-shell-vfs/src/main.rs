#![deny(warnings)]

fn main() {
    if let Err(error) = dolang_shell_vfs::main(std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
