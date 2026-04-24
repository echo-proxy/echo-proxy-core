use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_std::{
    future,
    io::WriteExt,
    net::{TcpListener, TcpStream},
    task,
};
use async_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        http::StatusCode,
        Message,
    },
    WebSocketStream,
};
use clap::Parser;
use core_lib::protocol;
use futures::AsyncReadExt;
use futures_util::StreamExt;
use semver::{Version, VersionReq};
use uuid::Uuid;

const SUPPORTED_VERSION: &str = ">=0.1.0";
const TOKEN_TTL_SECS: u64 = 3600;
/// Allowed destination ports. Connections to any other port are rejected.
const DEFAULT_ALLOW_PORTS: &[u16] = &[80, 443];
const CONNECT_TIMEOUT_SECS: u64 = 10;

// ── TokenStore ────────────────────────────────────────────────────────────────

struct TokenEntry {
    user_id: String,
    expires_at: Instant,
}

#[derive(Default)]
struct TokenStore {
    map: Mutex<HashMap<String, TokenEntry>>,
}

impl TokenStore {
    fn issue(&self, user_id: String, ttl: Duration) -> String {
        let token = Uuid::new_v4().to_string();
        let mut map = self.map.lock().unwrap();
        // GC: remove expired entries when issuing a new token
        let now = Instant::now();
        map.retain(|_, v| v.expires_at > now);
        map.insert(
            token.clone(),
            TokenEntry {
                user_id,
                expires_at: now + ttl,
            },
        );
        token
    }

    /// Returns the user_id if the token is valid and not expired.
    fn validate(&self, token: &str) -> Option<String> {
        let map = self.map.lock().unwrap();
        map.get(token).and_then(|entry| {
            if entry.expires_at > Instant::now() {
                Some(entry.user_id.clone())
            } else {
                None
            }
        })
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// User id list.
    #[arg(long)]
    users: Vec<String>,

    /// Local server address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Local server port
    #[arg(long, default_value = "9001")]
    port: u16,

    /// Allowed destination ports (default: 80 443)
    #[arg(long)]
    allow_ports: Vec<u16>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let allow_ports: Vec<u16> = if cli.allow_ports.is_empty() {
        DEFAULT_ALLOW_PORTS.to_vec()
    } else {
        cli.allow_ports
    };

    let server_thread = std::thread::spawn(move || {
        let addr = format!("{}:{}", cli.host, cli.port);
        async_std::task::block_on(server(addr, cli.users, allow_ports));
    });
    let _ = server_thread.join();
}

async fn server(addr: String, user_list: Vec<String>, allow_ports: Vec<u16>) {
    let listener = TcpListener::bind(&addr).await.unwrap();
    println!("Listening on: {}", listener.local_addr().unwrap());

    let store = Arc::new(TokenStore::default());
    let allow_ports = Arc::new(allow_ports);

    while let Ok((stream, _)) = listener.accept().await {
        let user_list = user_list.clone();
        let store = Arc::clone(&store);
        let allow_ports = Arc::clone(&allow_ports);
        task::spawn(accept_connection(stream, user_list, store, allow_ports));
    }
}

// ── Connection acceptance ─────────────────────────────────────────────────────

async fn accept_connection(
    stream: TcpStream,
    user_list: Vec<String>,
    store: Arc<TokenStore>,
    allow_ports: Arc<Vec<u16>>,
) {
    let mut dist = String::new();
    let mut is_control_request = false;
    let mut auth_failed = false;

    let store_ref = Arc::clone(&store);
    let callback = |req: &Request, response: Response| {
        let path = req.uri().path();

        if path.ends_with(protocol::CONTROL_PATH) {
            is_control_request = true;
            return Ok(response);
        }

        // Data channel: require valid token
        let token = match req
            .headers()
            .get(protocol::HEADER_TOKEN)
            .and_then(|v| v.to_str().ok())
        {
            Some(t) => t,
            None => {
                auth_failed = true;
                let resp = Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Some("Missing token".into()))
                    .unwrap();
                return Err(resp);
            }
        };

        if store_ref.validate(token).is_none() {
            auth_failed = true;
            let resp = Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Some("Invalid or expired token".into()))
                .unwrap();
            return Err(resp);
        }

        // Decode destination host from query string
        let query = req.uri().query().unwrap_or("");
        let encoded_host = query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{}=", protocol::QUERY_KEY)));

        match encoded_host {
            Some(h) => match core_lib::decode_host(h) {
                Ok(host) => dist = host,
                Err(e) => {
                    auth_failed = true;
                    eprintln!("decode_host error: {}", e);
                    let resp = Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Some("Bad request".into()))
                        .unwrap();
                    return Err(resp);
                }
            },
            None => {
                auth_failed = true;
                let resp = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Some("Missing destination".into()))
                    .unwrap();
                return Err(resp);
            }
        }

        Ok(response)
    };

    let ws_stream = match accept_hdr_async(stream, callback).await {
        Ok(ws) => ws,
        Err(e) => {
            if !auth_failed {
                eprintln!("WebSocket handshake error: {}", e);
            }
            return;
        }
    };

    if is_control_request {
        process_control(ws_stream, user_list, store).await;
    } else {
        exchange_data(ws_stream, dist, allow_ports).await;
    }
}

// ── Control channel ───────────────────────────────────────────────────────────

async fn process_control(
    mut ws_stream: WebSocketStream<TcpStream>,
    user_list: Vec<String>,
    store: Arc<TokenStore>,
) {
    let msg = match ws_stream.next().await {
        Some(Ok(m)) => m,
        _ => {
            let _ = ws_stream.close(None).await;
            return;
        }
    };

    if !msg.is_text() {
        let _ = ws_stream.close(None).await;
        return;
    }

    let text = match msg.to_text() {
        Ok(t) => t.to_string(),
        Err(_) => {
            let _ = ws_stream.close(None).await;
            return;
        }
    };

    macro_rules! send_bye {
        ($reason:expr) => {{
            let msg = format!("{}{}", protocol::BYE_PREFIX, $reason);
            let _ = ws_stream.send(Message::Text(msg.into())).await;
            let _ = ws_stream.close(None).await;
            return;
        }};
    }

    if !text.starts_with(protocol::HELLO_PREFIX) {
        send_bye!("Unexpected message");
    }

    let parts: Vec<&str> = text.splitn(3, ':').collect();
    if parts.len() != 3 {
        send_bye!("Malformed handshake");
    }

    let client_version = parts[1];
    let client_user_id = parts[2].to_string();

    // Version check
    let installed = match Version::parse(client_version) {
        Ok(v) => v,
        Err(_) => {
            send_bye!("Invalid version format");
        }
    };
    let required = VersionReq::parse(SUPPORTED_VERSION).unwrap();
    if !required.matches(&installed) {
        let dl = "https://github.com/echo-proxy/echo-proxy-core/releases";
        send_bye!(format!(
            "The installed version is not supported, please download the latest version from {}",
            dl
        ));
    }

    // User check
    if !user_list.contains(&client_user_id) {
        send_bye!("User not allowed");
    }

    // Issue token
    let token = store.issue(client_user_id, Duration::from_secs(TOKEN_TTL_SECS));
    let hi = format!("{}{}", protocol::HI_PREFIX, token);
    if let Err(e) = ws_stream.send(Message::Text(hi.into())).await {
        eprintln!("Failed to send Hi: {}", e);
    }
}

// ── Data channel ──────────────────────────────────────────────────────────────

async fn exchange_data(
    ws_stream: WebSocketStream<TcpStream>,
    dist: String,
    allow_ports: Arc<Vec<u16>>,
) {
    // Validate dest port
    let port = match dist.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
        Some(p) => p,
        None => {
            eprintln!("exchange_data: cannot parse port from '{}'", dist);
            return;
        }
    };
    if !allow_ports.contains(&port) {
        eprintln!("exchange_data: port {} not in allow-list", port);
        return;
    }

    // DNS resolve + SSRF blacklist check (blocking resolver, run synchronously)
    let addrs: Vec<std::net::SocketAddr> = match dist.to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(e) => {
            eprintln!("exchange_data: DNS resolve failed for '{}': {}", dist, e);
            return;
        }
    };
    if addrs.is_empty() {
        eprintln!("exchange_data: no addresses resolved for '{}'", dist);
        return;
    }
    for addr in &addrs {
        if core_lib::is_blocked_addr(addr) {
            eprintln!("exchange_data: blocked address {} for '{}'", addr, dist);
            return;
        }
    }

    // Connect with timeout
    let tcp_stream = match future::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        TcpStream::connect(&dist),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("exchange_data: connect to '{}' failed: {}", dist, e);
            return;
        }
        Err(_) => {
            eprintln!("exchange_data: connect to '{}' timed out", dist);
            return;
        }
    };

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();
    let (ws_writer, mut ws_reader) = ws_stream.split();

    // ws -> tcp
    std::thread::spawn(move || {
        task::block_on(async {
            while let Some(msg) = ws_reader.next().await {
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("ws read error: {}", e);
                        break;
                    }
                };
                if msg.is_binary() {
                    if let Err(e) = tcp_writer.write_all(&msg.into_data()).await {
                        eprintln!("tcp write error: {}", e);
                        break;
                    }
                }
            }
        });
    });

    // tcp -> ws
    std::thread::spawn(move || {
        task::block_on(async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = match tcp_reader.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("tcp read error: {}", e);
                        break;
                    }
                };
                if n == 0 {
                    break;
                }
                if let Err(e) = ws_writer.send(Message::Binary(buf[..n].to_vec().into())).await {
                    eprintln!("ws send error: {}", e);
                    break;
                }
            }
        });
    });
}
