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
    let endpoint = format!("ws://{}/", server_addr);
    let token = client::obtain_token(&endpoint, user)
        .await
        .expect("obtain_token failed");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::bounded::<()>(1);
    task::spawn(async move {
        client::run_proxy(endpoint, listener, token, rx).await.ok();
    });
    // Brief yield to let the mux connection be established before tests send traffic.
    task::sleep(std::time::Duration::from_millis(50)).await;
    (addr, tx)
}

/// Convert a SocketAddr to a `ws://` endpoint URL.
pub fn endpoint_url(addr: SocketAddr) -> String {
    format!("ws://{}/", addr)
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

/// Send `GET path HTTP/1.1` through the proxy at `proxy_addr`.
/// Returns `(status_code, body_bytes)`.
pub async fn http_get_via_proxy(
    proxy_addr: SocketAddr,
    host: &str,
    path: &str,
) -> (u16, Vec<u8>) {
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
