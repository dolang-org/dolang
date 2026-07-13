#![deny(warnings)]

use std::process;

#[cfg(windows)]
use std::ffi::{OsStr, OsString};

mod stock_config;

extern crate dolang_ext_compile;
extern crate dolang_ext_load;

#[cfg(feature = "base64")]
extern crate dolang_ext_base64;
#[cfg(feature = "digest")]
extern crate dolang_ext_digest;
#[cfg(feature = "glob")]
extern crate dolang_ext_glob;
#[cfg(feature = "http")]
extern crate dolang_ext_http;
#[cfg(feature = "json")]
extern crate dolang_ext_json;
#[cfg(feature = "patch")]
extern crate dolang_ext_patch;
#[cfg(feature = "progress")]
extern crate dolang_ext_progress;
#[cfg(feature = "rand")]
extern crate dolang_ext_rand;
#[cfg(feature = "regex")]
extern crate dolang_ext_regex;
#[cfg(feature = "sqlite")]
extern crate dolang_ext_sqlite;
#[cfg(feature = "toml")]
extern crate dolang_ext_toml;
#[cfg(feature = "xml")]
extern crate dolang_ext_xml;
#[cfg(feature = "yaml")]
extern crate dolang_ext_yaml;
#[cfg(feature = "zip")]
extern crate dolang_ext_zip;

fn main() {
    #[cfg(windows)]
    if let Some(code) = run_vfs_mode(std::env::args_os().skip(1)) {
        process::exit(code);
    }

    process::exit(dolang_shell_core::main(stock_config::StockConfig));
}

#[cfg(windows)]
fn run_vfs_mode(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    match parse_vfs_mode(args) {
        VfsMode::Shell => None,
        VfsMode::Error(message) => {
            eprintln!("error: {message}");
            Some(2)
        }
        VfsMode::Server(pipe_name) => Some(match dolang_shell_vfs::serve_named_pipe(pipe_name) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("error: VFS server failed: {error}");
                1
            }
        }),
    }
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum VfsMode {
    Shell,
    Server(OsString),
    Error(&'static str),
}

#[cfg(windows)]
fn parse_vfs_mode(args: impl IntoIterator<Item = OsString>) -> VfsMode {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(OsStr::new("--vfs")) {
        return VfsMode::Shell;
    }

    let Some(pipe_name) = args.next() else {
        return VfsMode::Error("--vfs requires a named-pipe path");
    };
    if args.next().is_some() {
        return VfsMode::Error("--vfs accepts exactly one named-pipe path");
    }
    VfsMode::Server(pipe_name)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn routes_only_exact_vfs_mode() {
        assert_eq!(parse_vfs_mode(args(&[])), VfsMode::Shell);
        assert_eq!(parse_vfs_mode(args(&["program.dol"])), VfsMode::Shell);
        assert_eq!(
            parse_vfs_mode(args(&["--vfs", r"\\.\pipe\test"])),
            VfsMode::Server(r"\\.\pipe\test".into())
        );
        assert_eq!(
            parse_vfs_mode(args(&["--vfs"])),
            VfsMode::Error("--vfs requires a named-pipe path")
        );
        assert_eq!(
            parse_vfs_mode(args(&["--vfs", "pipe", "extra"])),
            VfsMode::Error("--vfs accepts exactly one named-pipe path")
        );
    }
}
