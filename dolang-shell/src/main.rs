#![deny(warnings)]

use std::process;

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
    process::exit(dolang_shell_core::main(stock_config::StockConfig));
}
