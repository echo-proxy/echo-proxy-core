use clap::Parser;
use serde::Deserialize;
use server::{ServerOptions, make_shutdown_channel, serve};
use tracing_subscriber::{EnvFilter, fmt};
use wtransport::Identity;

#[derive(Deserialize, Default)]
struct ServerSection {
    users: Option<Vec<String>>,
    host: Option<String>,
    port: Option<u16>,
    /// Path to TLS certificate PEM file. Required unless `self_signed` is true.
    cert: Option<String>,
    /// Path to TLS private key PEM file. Required unless `self_signed` is true.
    key: Option<String>,
    /// Generate a self-signed certificate (development/testing only).
    self_signed: Option<bool>,
    log_level: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    server: Option<ServerSection>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: String,
    #[arg(long)]
    users: Vec<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    cert: Option<String>,
    #[arg(long)]
    key: Option<String>,
    #[arg(long)]
    self_signed: bool,
}

fn load_file_config(path: &str) -> ServerSection {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<FileConfig>(&s).ok())
        .and_then(|f| f.server)
        .unwrap_or_default()
}

#[tokio::main]
async fn main() {
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
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let port = cli.port.or(file.port).unwrap_or(4433);

    let self_signed = cli.self_signed || file.self_signed.unwrap_or(false);

    let identity = if self_signed {
        let id = Identity::self_signed([host.as_str(), "localhost", "127.0.0.1"])
            .expect("failed to generate self-signed identity");
        let hash = id.certificate_chain().as_slice()[0].hash();
        tracing::warn!(
            cert_hash = %hash.fmt(wtransport::tls::Sha256DigestFmt::BytesArray),
            "using self-signed certificate — configure client with this cert_hash"
        );
        id
    } else {
        let cert_path = cli.cert.or(file.cert).unwrap_or_else(|| {
            tracing::error!("--cert is required (or set [server] cert in config.toml), or use --self-signed");
            std::process::exit(1);
        });
        let key_path = cli.key.or(file.key).unwrap_or_else(|| {
            tracing::error!("--key is required (or set [server] key in config.toml), or use --self-signed");
            std::process::exit(1);
        });
        Identity::load_pemfiles(&cert_path, &key_path)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("failed to load TLS identity: {e}");
                std::process::exit(1);
            })
    };

    let addr: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap_or_else(|e| {
        tracing::error!("invalid address: {e}");
        std::process::exit(1);
    });

    let (_tx, rx) = make_shutdown_channel();
    serve(addr, ServerOptions { users, identity }, rx)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("server error: {e}");
            std::process::exit(1);
        });
}
