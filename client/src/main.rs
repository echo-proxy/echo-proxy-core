use async_std::task;
use clap::Parser;
use client::{make_shutdown_channel, obtain_token, run_proxy};
use async_std::net::TcpListener;
use std::time::Instant;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Remote server address
    #[arg(long)]
    endpoint: String,
    /// User id
    #[arg(long)]
    user: String,

    /// Local server address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Local server port
    #[arg(long, default_value = "9002")]
    port: u16,
}

fn main() {
    let cli = Cli::parse();

    let start = Instant::now();
    let token = task::block_on(obtain_token(&cli.endpoint, &cli.user));
    println!("[mux] check_server took {:?}", start.elapsed());

    let token = match token {
        Some(t) => t,
        None => return,
    };

    let addr = format!("{}:{}", cli.host, cli.port);
    task::block_on(async {
        let listener = TcpListener::bind(&addr).await.unwrap();
        let (_tx, rx) = make_shutdown_channel();
        run_proxy(cli.endpoint, listener, token, rx).await.unwrap();
    });
}
