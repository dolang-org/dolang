#![deny(warnings)]

#[cfg(feature = "winreg")]
extern crate dolang_vfs_winreg;
#[cfg(feature = "winscm")]
extern crate dolang_vfs_winscm;

fn main() {
    if let Err(error) = dolang_vfs::main(std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
