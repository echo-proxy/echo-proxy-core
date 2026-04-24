use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::task;
use async_tungstenite::tungstenite::http::Request;
use async_tungstenite::tungstenite::http::Uri;
use async_tungstenite::{async_std::connect_async, tungstenite::Message};
use clap::Parser;
use core_lib::protocol;
use futures::{prelude::*, AsyncReadExt, StreamExt};
use futures_util::SinkExt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_LINES: usize = 128;
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// How many times to retry check_server when the data connection is rejected.
const MAX_TOKEN_RETRIES: usize = 3;

// ── CLI ───────────────────────────────────────────────────────────────────────

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

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let start = Instant::now();
    let token = task::block_on(check_server(&cli.endpoint, &cli.user));
    println!("Check server use time: {:?}", start.elapsed());

    let token = match token {
        Some(t) => t,
        None => return,
    };

    let addr = format!("{}:{}", cli.host, cli.port);
    task::block_on(run(cli.endpoint.clone(), addr, cli.user.clone(), token));
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.ends_with('/') {
        endpoint.to_string()
    } else {
        format!("{}/", endpoint)
    }
}

fn build_request(
    url_str: &str,
    token: &str,
) -> Result<
    async_tungstenite::tungstenite::http::Request<()>,
    async_tungstenite::tungstenite::http::Error,
> {
    let uri = Uri::from_str(url_str).map_err(|e| {
        async_tungstenite::tungstenite::http::Error::from(e)
    })?;
    let host_str = uri.host().unwrap_or(url_str);

    Request::builder()
        .uri(url_str)
        .header("Host", host_str)
        .header(protocol::HEADER_TOKEN, token)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            async_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
}

// ── Server health-check / token acquisition ───────────────────────────────────

/// Connects to the control endpoint, exchanges Hello/Hi, and returns the token.
/// Returns None if the server rejects the connection.
async fn check_server(endpoint: &str, user_id: &str) -> Option<String> {
    let base = normalize_endpoint(endpoint);
    let url = format!("{}{}", base, protocol::CONTROL_PATH);
    let app_version = env!("CARGO_PKG_VERSION");

    let (mut ws_stream, _) = match connect_async(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to control endpoint: {}", e);
            return None;
        }
    };

    let hello = format!("{}{}:{}", protocol::HELLO_PREFIX, app_version, user_id);
    if let Err(e) = ws_stream.send(Message::Text(hello.into())).await {
        eprintln!("Failed to send Hello: {}", e);
        return None;
    }
    if let Err(e) = ws_stream.flush().await {
        eprintln!("Failed to flush: {}", e);
        return None;
    }

    let msg = match ws_stream.next().await {
        Some(Ok(m)) => m,
        _ => {
            eprintln!("No response from control endpoint");
            return None;
        }
    };

    let text = match msg.to_text() {
        Ok(t) => t.to_string(),
        Err(_) => {
            eprintln!("Non-text response from control endpoint");
            return None;
        }
    };

    if let Some(token) = text.strip_prefix(protocol::HI_PREFIX) {
        Some(token.to_string())
    } else {
        let reason = text.strip_prefix(protocol::BYE_PREFIX).unwrap_or(&text);
        println!("{}", reason);
        None
    }
}

// ── Main proxy loop ───────────────────────────────────────────────────────────

async fn run(endpoint: String, addr: String, user_id: String, initial_token: String) {
    let listener = TcpListener::bind(&addr).await.unwrap();
    println!("http proxy listening on {}", listener.local_addr().unwrap());

    let token = Arc::new(Mutex::new(initial_token));

    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {}", e);
                continue;
            }
        };

        let endpoint = endpoint.clone();
        let user_id = user_id.clone();
        let token = Arc::clone(&token);

        std::thread::spawn(move || {
            task::block_on(handle_connection(&endpoint, &user_id, token, stream));
        });
    }
}

// ── HTTP header parsing ───────────────────────────────────────────────────────

async fn parse_request_header(
    tcp_stream: TcpStream,
) -> std::io::Result<(String, String, Vec<u8>)> {
    let mut headers = String::new();
    let mut host = String::new();
    let mut content_length: usize = 0;
    let mut line_count = 0usize;

    let mut reader = async_std::io::BufReader::new(tcp_stream);

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        line_count += 1;
        if line_count > MAX_HEADER_LINES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Too many header lines",
            ));
        }
        if headers.len() + line.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Headers too large",
            ));
        }

        headers.push_str(&line);

        if line.to_ascii_lowercase().starts_with("host:") {
            host = line
                .splitn(2, ':')
                .nth(1)
                .unwrap_or("")
                .trim()
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string();
            if !host.contains(':') {
                host.push_str(":80");
            }
        }

        if line.to_ascii_lowercase().starts_with("content-length:") {
            let size = line
                .splitn(2, ':')
                .nth(1)
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if size > MAX_BODY_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Content-Length exceeds limit",
                ));
            }
            content_length = size;
        }

        if line == "\r\n" {
            break;
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    Ok((host, headers, body))
}

// ── Per-connection dispatch ───────────────────────────────────────────────────

async fn handle_connection(
    remote_server: &str,
    user_id: &str,
    token: Arc<Mutex<String>>,
    mut tcp_stream: TcpStream,
) {
    let (host, headers, body) = match parse_request_header(tcp_stream.clone()).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parse_request_header error: {}", e);
            let _ = tcp_stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await;
            return;
        }
    };

    let base = normalize_endpoint(remote_server);
    let ws_url = format!(
        "{}?{}={}",
        base,
        protocol::QUERY_KEY,
        core_lib::encode_host(&host)
    );
    println!("accepted {}", host);

    if headers.starts_with("CONNECT ") {
        handle_https(ws_url, tcp_stream, user_id, token, remote_server).await;
    } else {
        handle_http(ws_url, tcp_stream, headers, body, user_id, token, remote_server).await;
    }
}

// ── Token refresh helper ──────────────────────────────────────────────────────

/// Re-runs check_server to obtain a fresh token, updating the shared store.
async fn refresh_token(
    remote_server: &str,
    user_id: &str,
    token: &Arc<Mutex<String>>,
) -> Option<String> {
    for attempt in 0..MAX_TOKEN_RETRIES {
        if attempt > 0 {
            async_std::task::sleep(std::time::Duration::from_secs(1)).await;
        }
        if let Some(new_token) = check_server(remote_server, user_id).await {
            *token.lock().unwrap() = new_token.clone();
            return Some(new_token);
        }
    }
    None
}

// ── HTTPS tunnel ──────────────────────────────────────────────────────────────

async fn handle_https(
    ws_url: String,
    mut tcp_stream: TcpStream,
    user_id: &str,
    token: Arc<Mutex<String>>,
    remote_server: &str,
) {
    let ws_stream = connect_with_token_retry(&ws_url, user_id, &token, remote_server).await;
    let ws_stream = match ws_stream {
        Some(ws) => ws,
        None => return,
    };

    if let Err(e) = tcp_stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
    {
        eprintln!("write 200 error: {}", e);
        return;
    }
    if let Err(e) = tcp_stream.flush().await {
        eprintln!("flush error: {}", e);
        return;
    }

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();
    let (ws_writer, mut ws_reader) = ws_stream.split();

    // ws -> tcp
    std::thread::spawn(move || {
        task::block_on(async {
            while let Some(msg) = ws_reader.next().await {
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                if msg.is_binary() {
                    if tcp_writer.write_all(&msg.into_data()).await.is_err() {
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
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                if ws_writer
                    .send(Message::Binary(buf[..n].to_vec().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    });
}

// ── HTTP proxy ────────────────────────────────────────────────────────────────

async fn handle_http(
    ws_url: String,
    mut tcp_stream: TcpStream,
    headers: String,
    body: Vec<u8>,
    user_id: &str,
    token: Arc<Mutex<String>>,
    remote_server: &str,
) {
    let mut ws_stream = match connect_with_token_retry(&ws_url, user_id, &token, remote_server).await {
        Some(ws) => ws,
        None => return,
    };

    if let Err(e) = ws_stream
        .send(Message::Binary(headers.as_bytes().to_vec().into()))
        .await
    {
        eprintln!("ws send headers error: {}", e);
        return;
    }
    if !body.is_empty() {
        if let Err(e) = ws_stream.send(Message::Binary(body.into())).await {
            eprintln!("ws send body error: {}", e);
            return;
        }
    }
    if let Err(e) = ws_stream.flush().await {
        eprintln!("ws flush error: {}", e);
        return;
    }

    while let Some(msg) = ws_stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        if msg.is_binary() {
            if tcp_stream.write_all(&msg.into_data()).await.is_err() {
                break;
            }
        }
    }
    let _ = tcp_stream.flush().await;
    let _ = tcp_stream.close().await;
}

// ── Shared connection helper with token retry ─────────────────────────────────

type WsStream = async_tungstenite::WebSocketStream<
    async_tungstenite::async_std::ConnectStream,
>;

async fn connect_with_token_retry(
    ws_url: &str,
    user_id: &str,
    token: &Arc<Mutex<String>>,
    remote_server: &str,
) -> Option<WsStream> {
    for attempt in 0..=1 {
        let t = token.lock().unwrap().clone();
        let req = match build_request(ws_url, &t) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("build_request error: {}", e);
                return None;
            }
        };
        match connect_async(req).await {
            Ok((ws, _)) => return Some(ws),
            Err(e) => {
                let msg = e.to_string();
                if attempt == 0
                    && (msg.contains("403") || msg.contains("401") || msg.contains("Forbidden"))
                {
                    eprintln!("Token rejected, refreshing...");
                    if refresh_token(remote_server, user_id, token).await.is_none() {
                        eprintln!("Token refresh failed");
                        return None;
                    }
                } else {
                    eprintln!("Failed to connect WS: {}", e);
                    return None;
                }
            }
        }
    }
    None
}
