/// Shared test fixtures for integration tests.
///
/// All helpers return their bound `SocketAddr` so tests can talk to them on
/// the OS-assigned port (`:0`).  Each long-running task is controlled by a
/// shutdown `Sender<()>` — dropping or sending into it stops the task.
use async_std::{
    channel::{self, Sender},
    net::{SocketAddr, TcpListener, TcpStream},
    task,
};
use futures::{AsyncReadExt, AsyncWriteExt};

// ── Upstream mock helpers ─────────────────────────────────────────────────────

/// Spawn a minimal HTTP/1.1 upstream that replies 200 with body `hello-upstream`
/// to every request.  Returns the bound address.
pub async fn spawn_http_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            task::spawn(async move {
                // Drain the request headers (stop at blank line).
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
/// Used to test proxy behaviour when the remote peer closes right away.
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
                let (mut reader, mut writer) = stream.split();
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

/// Start a proxy server. Returns `(addr, shutdown_tx)`.
pub async fn spawn_proxy_server(users: Vec<String>) -> (SocketAddr, Sender<()>) {
    let listener = server::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::bounded::<()>(1);
    task::spawn(async move {
        server::serve(listener, server::ServerConfig { users }, rx)
            .await
            .ok();
    });
    (addr, tx)
}

/// Start a proxy client connected to `server_addr` as `user`.
/// Returns `(local_proxy_addr, shutdown_tx)`.
pub async fn spawn_proxy_client(server_addr: SocketAddr, user: &str) -> (SocketAddr, Sender<()>) {
    spawn_proxy_client_with_routing(server_addr, user, client::RoutingConfig::default()).await
}
/// Like [`spawn_proxy_client`] but with explicit routing rules.
pub async fn spawn_proxy_client_with_routing(
    server_addr: SocketAddr,
    user: &str,
    routing: client::RoutingConfig,
) -> (SocketAddr, Sender<()>) {
    let endpoint = format!("ws://{}/", server_addr);
    let token = client::obtain_token(&endpoint, user)
        .await
        .expect("obtain_token failed");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::bounded::<()>(1);
    task::spawn(async move {
        client::run_proxy_with_routing(endpoint, listener, token, rx, routing)
            .await
            .ok();
    });
    // Brief yield to let the mux connection be established before tests send traffic.
    task::sleep(std::time::Duration::from_millis(50)).await;
    (addr, tx)
}

/// Start a **resilient** proxy client that reconnects automatically when the
/// remote WebSocket drops.  Returns `(local_proxy_addr, shutdown_tx)`.
pub async fn spawn_resilient_proxy_client(
    server_addr: SocketAddr,
    user: &str,
) -> (SocketAddr, Sender<()>) {
    let endpoint = format!("ws://{}/", server_addr);
    let user = user.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::bounded::<()>(1);
    task::spawn(async move {
        client::run_proxy_resilient(
            endpoint,
            user,
            listener,
            None,
            rx,
            client::RoutingConfig::default(),
        )
        .await
        .ok();
    });
    // Brief yield to let the initial mux connection be established.
    task::sleep(std::time::Duration::from_millis(100)).await;
    (addr, tx)
}

/// Start a resilient proxy client with **both** an HTTP and a SOCKS5 listener.
/// Returns `((http_addr, socks_addr), shutdown_tx)`.
pub async fn spawn_proxy_client_with_socks(
    server_addr: SocketAddr,
    user: &str,
    routing: client::RoutingConfig,
) -> ((SocketAddr, SocketAddr), Sender<()>) {
    let endpoint = format!("ws://{}/", server_addr);
    let user = user.to_string();
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let socks_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_addr = socks_listener.local_addr().unwrap();
    let (tx, rx) = channel::bounded::<()>(1);
    task::spawn(async move {
        client::run_proxy_resilient(
            endpoint,
            user,
            http_listener,
            Some(socks_listener),
            rx,
            routing,
        )
        .await
        .ok();
    });
    task::sleep(std::time::Duration::from_millis(100)).await;
    ((http_addr, socks_addr), tx)
}

/// Convert a SocketAddr to a `ws://` endpoint URL.
pub fn endpoint_url(addr: SocketAddr) -> String {
    format!("ws://{}/", addr)
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

/// Send `GET path HTTP/1.1` through the proxy at `proxy_addr`.
/// Returns `(status_code, body_bytes)`.
pub async fn http_get_via_proxy(proxy_addr: SocketAddr, host: &str, path: &str) -> (u16, Vec<u8>) {
    use async_std::io::BufReader;
    use futures::io::AsyncBufReadExt;

    let stream = TcpStream::connect(proxy_addr).await.unwrap();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );
    (&stream).write_all(request.as_bytes()).await.unwrap();

    // Parse response with BufReader so we can read header lines then body.
    let mut reader = BufReader::new(&stream);
    let mut header_text = String::new();
    let mut status_code: u16 = 0;
    let mut content_length: usize = 0;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" {
            break;
        }
        if header_text.is_empty() {
            // Status line: "HTTP/1.1 200 OK"
            if let Some(code) = line.split_whitespace().nth(1) {
                status_code = code.parse().unwrap_or(0);
            }
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
        header_text.push_str(&line);
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.unwrap();
    }
    // stream is dropped here → sends FIN → proxy tcp_reader gets EOF → cleanup
    (status_code, body)
}

/// Send `GET / HTTP/1.1` directly to `addr` (bypassing the proxy tunnel).
/// Returns `(status_code, body_string)`.
pub async fn http_get_direct(addr: SocketAddr) -> (u16, String) {
    use async_std::io::BufReader;
    use futures::io::AsyncBufReadExt;

    let stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );
    (&stream).write_all(request.as_bytes()).await.unwrap();

    let mut reader = BufReader::new(&stream);
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

/// Issue a CONNECT request through the proxy and return the raw tunnelled
/// `TcpStream` after receiving `200 Connection established`.
pub async fn connect_via_proxy(proxy_addr: SocketAddr, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n",
        target = target
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    // Read until we find the blank line ending the 200 response.
    let mut buf = vec![0u8; 256];
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf[total..]).await.unwrap();
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&buf[..total]).unwrap();
    assert!(
        header.contains("200"),
        "Expected 200 from CONNECT, got: {}",
        header
    );
    stream
}

/// Perform a SOCKS5 CONNECT handshake to `target` (e.g. `"example.com:80"`)
/// and return the raw tunnelled `TcpStream` ready for data transfer.
pub async fn connect_via_socks5(socks_addr: SocketAddr, target: &str) -> TcpStream {
    let (host, port_str) = target.rsplit_once(':').expect("target must be host:port");
    let port: u16 = port_str.parse().expect("invalid port");

    let mut stream = TcpStream::connect(socks_addr).await.unwrap();

    // Greeting: VER=5, NMETHODS=1, METHOD=0 (no auth)
    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();

    // Read method selection: VER METHOD
    let mut sel = [0u8; 2];
    stream.read_exact(&mut sel).await.unwrap();
    assert_eq!(sel[0], 0x05, "unexpected SOCKS version");
    assert_eq!(sel[1], 0x00, "server selected unsupported auth method");

    // Request: VER=5 CMD=1(CONNECT) RSV=0 ATYP=3(DOMAIN) LEN DOMAIN PORT
    let domain = host.as_bytes();
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await.unwrap();

    // Reply: VER REP RSV ATYP BND.ADDR(4) BND.PORT(2)
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0], 0x05, "unexpected SOCKS version in reply");
    assert_eq!(reply[1], 0x00, "SOCKS5 CONNECT failed: REP={}", reply[1]);

    stream
}
