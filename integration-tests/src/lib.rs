/// Shared test fixtures for integration tests.
///
/// All helpers return their bound address so tests can connect on the
/// OS-assigned port (`:0`). Each long-running task is controlled by a
/// broadcast `Sender<()>` — sending or dropping it stops the task.
use std::net::SocketAddr;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    task,
};
use wtransport::Identity;

// ── Test client config ────────────────────────────────────────────────────────

/// Create a `TlsTrustConfig` that skips certificate validation (test only).
pub fn insecure_client_config() -> client::TlsTrustConfig {
    client::TlsTrustConfig::NoCertValidation
}

// ── Upstream mock helpers ─────────────────────────────────────────────────────

/// Spawn a minimal HTTP/1.1 upstream that replies 200 with body `hello-upstream`.
pub async fn spawn_http_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            task::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut total = 0usize;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = b"hello-upstream";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
            });
        }
    });
    addr
}

/// Spawn an upstream that accepts and immediately drops the connection.
pub async fn spawn_upstream_close_immediately() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    task::spawn(async move {
        while let Ok((_stream, _)) = listener.accept().await {
            // Drop stream immediately.
        }
    });
    addr
}

/// Spawn a raw TCP echo server. Returns the bound address.
pub async fn spawn_raw_echo_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            task::spawn(async move {
                let (mut reader, mut writer) = stream.into_split();
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if writer.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

// ── Proxy server / client helpers ────────────────────────────────────────────

/// Start a proxy server with a fresh self-signed cert.
/// Returns `(addr, cert_identity, shutdown_tx)`.
pub async fn spawn_proxy_server(users: Vec<String>) -> (SocketAddr, broadcast::Sender<()>) {
    let identity = Identity::self_signed(["localhost", "127.0.0.1"]).expect("self-signed identity");
    let opts = server::ServerOptions { users, identity };
    let se =
        server::ServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), opts).expect("bind server");
    let addr = se.local_addr();
    let (tx, rx) = broadcast::channel::<()>(1);
    task::spawn(async move {
        se.serve(rx).await.ok();
    });
    // Brief yield to let the endpoint start
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    (addr, tx)
}

/// Endpoint URL for a WebTransport test server.
pub fn endpoint_url(addr: SocketAddr) -> String {
    format!("https://127.0.0.1:{}/wt", addr.port())
}

/// Start a proxy client (non-resilient, single token) connected to `server_addr`.
pub async fn spawn_proxy_client(
    server_addr: SocketAddr,
    user: &str,
) -> (SocketAddr, broadcast::Sender<()>) {
    spawn_proxy_client_with_routing(server_addr, user, client::RoutingConfig::default()).await
}

/// Like [`spawn_proxy_client`] but with explicit routing rules.
pub async fn spawn_proxy_client_with_routing(
    server_addr: SocketAddr,
    user: &str,
    routing: client::RoutingConfig,
) -> (SocketAddr, broadcast::Sender<()>) {
    let endpoint = endpoint_url(server_addr);
    spawn_resilient_proxy_client_inner(endpoint, user.to_string(), routing, None).await
}

/// Start a resilient proxy client that reconnects automatically.
pub async fn spawn_resilient_proxy_client(
    server_addr: SocketAddr,
    user: &str,
) -> (SocketAddr, broadcast::Sender<()>) {
    let endpoint = endpoint_url(server_addr);
    spawn_resilient_proxy_client_inner(
        endpoint,
        user.to_string(),
        client::RoutingConfig::default(),
        None,
    )
    .await
}

/// Like [`spawn_resilient_proxy_client`] but also starts a SOCKS5 listener.
pub async fn spawn_proxy_client_with_socks(
    server_addr: SocketAddr,
    user: &str,
    routing: client::RoutingConfig,
) -> ((SocketAddr, SocketAddr), broadcast::Sender<()>) {
    let endpoint = endpoint_url(server_addr);
    let user = user.to_string();
    let trust = insecure_client_config();

    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let socks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_addr = socks_listener.local_addr().unwrap();

    let (tx, rx) = broadcast::channel::<()>(1);
    task::spawn(async move {
        client::run_proxy_resilient(
            endpoint,
            user,
            trust,
            http_listener,
            Some(socks_listener),
            rx,
            routing,
        )
        .await
        .ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    ((http_addr, socks_addr), tx)
}

async fn spawn_resilient_proxy_client_inner(
    endpoint: String,
    user: String,
    routing: client::RoutingConfig,
    _socks: Option<TcpListener>,
) -> (SocketAddr, broadcast::Sender<()>) {
    let trust = insecure_client_config();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = broadcast::channel::<()>(1);
    task::spawn(async move {
        client::run_proxy_resilient(endpoint, user, trust, listener, None, rx, routing)
            .await
            .ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    (addr, tx)
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

/// Send `GET path HTTP/1.1` through the proxy at `proxy_addr`.
/// Returns `(status_code, body_bytes)`.
pub async fn http_get_via_proxy(proxy_addr: SocketAddr, host: &str, path: &str) -> (u16, Vec<u8>) {
    let stream = TcpStream::connect(proxy_addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        path = path,
        host = host
    );
    let (mut reader, mut writer) = stream.into_split();
    writer.write_all(request.as_bytes()).await.unwrap();
    drop(writer);

    let mut reader = tokio::io::BufReader::new(reader);
    let mut status_code: u16 = 0;
    let mut content_length: usize = 0;
    let mut first_line = true;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" {
            break;
        }
        if first_line {
            if let Some(code) = line.split_whitespace().nth(1) {
                status_code = code.parse().unwrap_or(0);
            }
            first_line = false;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            content_length = line["content-length:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .parse()
                .unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.unwrap();
    }
    (status_code, body)
}

/// Send `GET / HTTP/1.1` directly to `addr` (bypassing the proxy tunnel).
/// Returns `(status_code, body_string)`.
pub async fn http_get_direct(addr: SocketAddr) -> (u16, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    let (mut reader, mut writer) = stream.into_split();
    writer.write_all(request.as_bytes()).await.unwrap();
    drop(writer);

    let mut reader = tokio::io::BufReader::new(reader);
    let mut status_code: u16 = 0;
    let mut content_length: usize = 0;
    let mut first_line = true;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" {
            break;
        }
        if first_line {
            if let Some(code) = line.split_whitespace().nth(1) {
                status_code = code.parse().unwrap_or(0);
            }
            first_line = false;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            content_length = line["content-length:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .parse()
                .unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.unwrap();
    }
    (status_code, String::from_utf8_lossy(&body).into_owned())
}

/// Issue a CONNECT request through the proxy and return the tunnelled `TcpStream`.
pub async fn connect_via_proxy(proxy_addr: SocketAddr, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = vec![0u8; 256];
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf[total..]).await.unwrap();
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    stream
}

/// Connect through a SOCKS5 proxy and return the raw tunnelled `TcpStream`.
pub async fn connect_via_socks5(socks_addr: SocketAddr, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(socks_addr).await.unwrap();

    let (host, port_str) = target.rsplit_once(':').expect("target must be host:port");
    let port: u16 = port_str.parse().expect("valid port");
    let domain = host.as_bytes();

    // Greeting
    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut sel = [0u8; 2];
    stream.read_exact(&mut sel).await.unwrap();
    assert_eq!(sel[1], 0x00, "socks5: no auth accepted");

    // CONNECT request (ATYP=domain)
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await.unwrap();

    // Read reply (10 bytes for IPv4 binding)
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00, "socks5: expected success reply");

    stream
}
