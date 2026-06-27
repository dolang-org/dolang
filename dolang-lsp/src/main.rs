#![deny(warnings)]

use std::{fs, path::PathBuf};

use backtrace::Backtrace;
use tower_lsp_server::Server;

mod backend;

fn log_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .expect("no log path available")
        .join("dolang-lsp")
        .join("lsp.log")
}

fn log_init() {
    let path = log_path();
    fs::create_dir_all(path.parent().expect("log file has no parent directory?"))
        .expect("failed to crate logging directory");
    let log_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .expect("could not create log file");
    env_logger::Builder::new()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .filter_level(log::LevelFilter::Debug)
        .init();
}

fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        log::error!("panic: {}", msg);

        let backtrace = Backtrace::new();
        log::error!("backtrace:\n{:?}", backtrace);
    }));
}

#[tokio::main]
async fn main() {
    set_panic_hook();
    log_init();

    log::info!("logging started");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = backend::build_service();
    Server::new(stdin, stdout, socket).serve(service).await;

    log::info!("logging stopped");
}
