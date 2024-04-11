use std::env::{set_var, var};
use std::process::exit;

use log::error;
use structopt::StructOpt;

mod client;
mod protocol;
mod server;
mod util;

#[tokio::main]
async fn main() {
    init_logger();
    if let Err(err) = match Options::from_args() {
        Options::Client(options) => client::start(options).await,
        Options::Server(options) => server::start(options).await,
    } {
        error!("{}", err);
        exit(1);
    }
}

#[derive(StructOpt)]
enum Options {
    /// 启动客户端
    Client(client::Options),
    /// 启动服务端
    Server(server::Options),
}

fn init_logger() {
    if var("RUST_LOG").is_err() {
        #[cfg(debug_assertions)]
        set_var("RUST_LOG", "debug");
        #[cfg(not(debug_assertions))]
        set_var("RUST_LOG", "info");
    }
    env_logger::init();
}
