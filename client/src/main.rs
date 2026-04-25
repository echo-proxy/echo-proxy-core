use async_std::net::TcpListener;
use async_std::task;
use clap::Parser;
use client::{
    RoutingConfig, geoip::load_geoip_dat, geosite::load_geosite_dat, make_shutdown_channel,
    obtain_token, run_proxy_with_routing,
};
use serde::Deserialize;
use std::{path::Path, time::Instant};
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_GEOIP_DAT: &str = "geoip-only-cn-private.dat";
const DEFAULT_GEOSITE_DAT: &str = "dlc.dat";

#[derive(Deserialize, Default)]
struct ClientSection {
    endpoint: Option<String>,
    user: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    /// Patterns that must always tunnel through the remote proxy.
    proxy: Option<Vec<String>>,
    /// Patterns that must always connect directly.
    bypass: Option<Vec<String>>,
    /// Path to a V2Ray `geoip.dat` file for CIDR-based bypass.
    bypass_geoip_dat: Option<String>,
    /// Tags to extract from the geoip dat (default: ["cn", "private"]).
    bypass_geoip_tags: Option<Vec<String>>,
    /// Path to a V2Ray GeoSite dat file for domain-based bypass.
    bypass_geosite_dat: Option<String>,
    /// Tags to extract from the geosite dat (default: ["cn", "private"]).
    bypass_geosite_tags: Option<Vec<String>>,
    /// Log level: off, error, warn, info, debug, trace (default: info)
    log_level: Option<String>,
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

    // Build routing config.
    let routing = build_routing(
        file.proxy.unwrap_or_default(),
        file.bypass.unwrap_or_default(),
        file.bypass_geosite_dat,
        file.bypass_geosite_tags,
        file.bypass_geoip_dat,
        file.bypass_geoip_tags,
    );

    let start = Instant::now();
    let token = task::block_on(obtain_token(&endpoint, &user));
    tracing::info!(elapsed = ?start.elapsed(), "control handshake");

    let token = match token {
        Some(t) => t,
        None => return,
    };

    let addr = format!("{}:{}", host, port);
    task::block_on(async {
        let listener = TcpListener::bind(&addr).await.unwrap();
        let (_tx, rx) = make_shutdown_channel();
        run_proxy_with_routing(endpoint, listener, token, rx, routing)
            .await
            .unwrap();
    });
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

    let proxy = proxy_patterns
        .iter()
        .map(|s| HostPattern::parse(s))
        .collect();
    let bypass = bypass_patterns
        .iter()
        .map(|s| HostPattern::parse(s))
        .collect();

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
                        keywords = matcher.keywords.len(),
                        regexes = matcher.regexes.len(),
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
            let tags = geoip_tags.unwrap_or_else(|| vec!["cn".to_string(), "private".to_string()]);
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

fn dat_path_or_default(configured: Option<String>, default_path: &str) -> Option<(String, bool)> {
    match configured {
        Some(path) => Some((path, true)),
        None if Path::new(default_path).is_file() => Some((default_path.to_string(), false)),
        None => None,
    }
}
