#![deny(warnings)]

#[cfg(feature = "winnet")]
extern crate dolang_vfs_winnet;
#[cfg(feature = "winreg")]
extern crate dolang_vfs_winreg;
#[cfg(feature = "winscm")]
extern crate dolang_vfs_winscm;

#[cfg(not(asan))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    if let Err(error) = dolang_shell_vfs::main(std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
