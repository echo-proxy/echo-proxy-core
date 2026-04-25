use async_std::net::TcpListener;
use async_std::task;
use clap::Parser;
use client::{make_shutdown_channel, obtain_token, run_proxy};
use serde::Deserialize;
use std::time::Instant;

#[derive(Deserialize, Default)]
struct ClientSection {
    endpoint: Option<String>,
    user: Option<String>,
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    client: Option<ClientSection>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to config file (TOML)
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Remote server address
    #[arg(long)]
    endpoint: Option<String>,
    /// User id
    #[arg(long)]
    user: Option<String>,

    /// Local server address
    #[arg(long)]
    host: Option<String>,
    /// Local server port
    #[arg(long)]
    port: Option<u16>,
}

fn load_file_config(path: &str) -> ClientSection {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<FileConfig>(&s).ok())
        .and_then(|f| f.client)
        .unwrap_or_default()
}

fn main() {
    let cli = Cli::parse();
    let file = load_file_config(&cli.config);

    let endpoint = cli.endpoint.or(file.endpoint).unwrap_or_else(|| {
        eprintln!("error: --endpoint is required (or set [client] endpoint in config.toml)");
        std::process::exit(1);
    });
    let user = cli.user.or(file.user).unwrap_or_else(|| {
        eprintln!("error: --user is required (or set [client] user in config.toml)");
        std::process::exit(1);
    });
    let host = cli
        .host
        .or(file.host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = cli.port.or(file.port).unwrap_or(9002);

    let start = Instant::now();
    let token = task::block_on(obtain_token(&endpoint, &user));
    println!("[mux] check_server took {:?}", start.elapsed());

    let token = match token {
        Some(t) => t,
        None => return,
    };

    let addr = format!("{}:{}", host, port);
    task::block_on(async {
        let listener = TcpListener::bind(&addr).await.unwrap();
        let (_tx, rx) = make_shutdown_channel();
        run_proxy(endpoint, listener, token, rx).await.unwrap();
    });
}
