use async_std::task;
use clap::Parser;
use serde::Deserialize;
use server::{ServerConfig, bind, make_shutdown_channel, serve};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Deserialize, Default)]
struct ServerSection {
    users: Option<Vec<String>>,
    host: Option<String>,
    port: Option<u16>,
    /// Log level: off, error, warn, info, debug, trace (default: info)
    log_level: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    server: Option<ServerSection>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to config file (TOML)
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Allowed user IDs
    #[arg(long)]
    users: Vec<String>,

    /// Local server address
    #[arg(long)]
    host: Option<String>,
    /// Local server port
    #[arg(long)]
    port: Option<u16>,
}

fn load_file_config(path: &str) -> ServerSection {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<FileConfig>(&s).ok())
        .and_then(|f| f.server)
        .unwrap_or_default()
}

fn main() {
    let cli = Cli::parse();
    let file = load_file_config(&cli.config);

    let log_level = file.log_level.as_deref().unwrap_or("info");
    fmt()
        .with_env_filter(EnvFilter::new(log_level))
        .with_target(false)
        .init();

    let users = if !cli.users.is_empty() {
        cli.users
    } else {
        file.users.unwrap_or_default()
    };

    if users.is_empty() {
        tracing::error!(
            "at least one --users entry is required (or set [server] users in config.toml)"
        );
        std::process::exit(1);
    }

    let host = cli
        .host
        .or(file.host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = cli.port.or(file.port).unwrap_or(9001);

    let addr = format!("{}:{}", host, port);
    task::block_on(async {
        let listener = bind(&addr).await.unwrap();
        let (_tx, rx) = make_shutdown_channel();
        serve(listener, ServerConfig { users }, rx).await.unwrap();
    });
}
