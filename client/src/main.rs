use client::{
    RoutingConfig, TlsTrustConfig, geoip::load_geoip_dat, geosite::load_geosite_dat,
    make_shutdown_channel, run_proxy_resilient,
};
use clap::Parser;
use serde::Deserialize;
use std::path::Path;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_GEOIP_DAT: &str = "geoip-only-cn-private.dat";
const DEFAULT_GEOSITE_DAT: &str = "dlc.dat";

#[derive(Deserialize, Default)]
struct ClientSection {
    endpoint: Option<String>,
    user: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    socks_host: Option<String>,
    socks_port: Option<u16>,
    proxy: Option<Vec<String>>,
    bypass: Option<Vec<String>>,
    bypass_geoip_dat: Option<String>,
    bypass_geoip_tags: Option<Vec<String>>,
    bypass_geosite_dat: Option<String>,
    bypass_geosite_tags: Option<Vec<String>>,
    /// SHA-256 hex digest of server certificate for self-signed cert trust.
    cert_hash: Option<String>,
    log_level: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    client: Option<ClientSection>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: String,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    socks_host: Option<String>,
    #[arg(long)]
    socks_port: Option<u16>,
    /// SHA-256 cert hash (hex) for self-signed server certificates.
    #[arg(long)]
    cert_hash: Option<String>,
}

fn load_file_config(path: &str) -> ClientSection {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<FileConfig>(&s).ok())
        .and_then(|f| f.client)
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

    let endpoint = cli.endpoint.or(file.endpoint).unwrap_or_else(|| {
        tracing::error!("--endpoint is required (or set [client] endpoint in config.toml)");
        std::process::exit(1);
    });
    let user = cli.user.or(file.user).unwrap_or_else(|| {
        tracing::error!("--user is required (or set [client] user in config.toml)");
        std::process::exit(1);
    });
    let host = cli
        .host
        .or(file.host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = cli.port.or(file.port).unwrap_or(9002);

    let socks_host = cli
        .socks_host
        .or(file.socks_host)
        .unwrap_or_else(|| host.clone());
    let socks_port = cli.socks_port.or(file.socks_port);

    let cert_hash_str = cli.cert_hash.or(file.cert_hash);

    let trust = build_trust_config(cert_hash_str);

    let routing = build_routing(
        file.proxy.unwrap_or_default(),
        file.bypass.unwrap_or_default(),
        file.bypass_geosite_dat,
        file.bypass_geosite_tags,
        file.bypass_geoip_dat,
        file.bypass_geoip_tags,
    );

    let addr = format!("{host}:{port}");
    let http_listener = TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        tracing::error!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });

    let socks_listener = if let Some(sp) = socks_port {
        let socks_addr = format!("{socks_host}:{sp}");
        Some(TcpListener::bind(&socks_addr).await.unwrap_or_else(|e| {
            tracing::error!("failed to bind socks5 {socks_addr}: {e}");
            std::process::exit(1);
        }))
    } else {
        None
    };

    let (_tx, rx) = make_shutdown_channel();
    run_proxy_resilient(endpoint, user, trust, http_listener, socks_listener, rx, routing)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("proxy error: {e}");
            std::process::exit(1);
        });
}

fn build_trust_config(cert_hash_str: Option<String>) -> TlsTrustConfig {
    if let Some(hash_hex) = cert_hash_str {
        use wtransport::tls::{Sha256Digest, Sha256DigestFmt};
        let hash = Sha256Digest::from_str_fmt(&hash_hex, Sha256DigestFmt::DottedHex)
            .unwrap_or_else(|_| {
                // Try parsing as raw hex (no dots) by inserting colons
                let dotted = hash_hex
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(2)
                    .map(|c| c.iter().collect::<String>())
                    .collect::<Vec<_>>()
                    .join(":");
                Sha256Digest::from_str_fmt(&dotted, Sha256DigestFmt::DottedHex)
                    .unwrap_or_else(|e| {
                        tracing::error!("invalid cert_hash: {e}");
                        std::process::exit(1);
                    })
            });
        TlsTrustConfig::CertHash(hash)
    } else {
        TlsTrustConfig::NativeCerts
    }
}

fn build_routing(
    proxy_patterns: Vec<String>,
    bypass_patterns: Vec<String>,
    geosite_dat: Option<String>,
    geosite_tags: Option<Vec<String>>,
    geoip_dat: Option<String>,
    geoip_tags: Option<Vec<String>>,
) -> RoutingConfig {
    use client::HostPattern;

    let proxy = proxy_patterns.iter().map(|s| HostPattern::parse(s)).collect();
    let bypass = bypass_patterns.iter().map(|s| HostPattern::parse(s)).collect();

    let bypass_geosite = match dat_path_or_default(geosite_dat, DEFAULT_GEOSITE_DAT) {
        None => {
            tracing::debug!(file = DEFAULT_GEOSITE_DAT, "default GeoSite dat not found");
            None
        }
        Some((dat_path, required)) => {
            let tags =
                geosite_tags.unwrap_or_else(|| vec!["cn".to_string(), "private".to_string()]);
            match load_geosite_dat(Path::new(&dat_path), &tags) {
                Ok(matcher) => {
                    tracing::info!(
                        domains = matcher.domains.len(),
                        fulls = matcher.fulls.len(),
                        file = dat_path,
                        "loaded GeoSite rules"
                    );
                    Some(matcher)
                }
                Err(e) => {
                    if required {
                        tracing::error!("{e}");
                        std::process::exit(1);
                    }
                    tracing::warn!("{e}");
                    None
                }
            }
        }
    };

    let bypass_cidrs = match dat_path_or_default(geoip_dat, DEFAULT_GEOIP_DAT) {
        None => {
            tracing::debug!(file = DEFAULT_GEOIP_DAT, "default GeoIP dat not found");
            vec![]
        }
        Some((dat_path, required)) => {
            let tags =
                geoip_tags.unwrap_or_else(|| vec!["cn".to_string(), "private".to_string()]);
            match load_geoip_dat(Path::new(&dat_path), &tags) {
                Ok(cidrs) => {
                    tracing::info!(count = cidrs.len(), file = dat_path, "loaded GeoIP CIDRs");
                    cidrs
                }
                Err(e) => {
                    if required {
                        tracing::error!("{e}");
                        std::process::exit(1);
                    }
                    tracing::warn!("{e}");
                    vec![]
                }
            }
        }
    };

    RoutingConfig {
        proxy,
        bypass,
        bypass_geosite,
        bypass_cidrs,
    }
}

fn dat_path_or_default(
    configured: Option<String>,
    default_name: &str,
) -> Option<(String, bool)> {
    match configured {
        Some(p) => Some((p, true)),
        None => {
            if Path::new(default_name).exists() {
                Some((default_name.to_string(), false))
            } else {
                None
            }
        }
    }
}
