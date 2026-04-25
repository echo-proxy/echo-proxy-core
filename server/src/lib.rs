use async_std::{
    channel::{self, Receiver},
    net::{TcpListener, TcpStream},
    task,
};
use async_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
        http::StatusCode,
    },
};
use core_lib::{Frame, FrameTx, StreamRegistry, frame_channel};
use futures::{AsyncReadExt, AsyncWriteExt, FutureExt, StreamExt};
use semver::{Version, VersionReq};
use std::{
    collections::HashSet,
    net::IpAddr,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const SUPPORTED_VERSION: &str = ">=0.1.0";

type TokenStore = Arc<Mutex<HashSet<Uuid>>>;

pub struct ServerConfig {
    pub users: Vec<String>,
}

pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

pub async fn serve(
    listener: TcpListener,
    cfg: ServerConfig,
    shutdown: Receiver<()>,
) -> std::io::Result<()> {
    println!("[mux] listening on {}", listener.local_addr().unwrap());
    let token_store: TokenStore = Arc::new(Mutex::new(HashSet::new()));

    loop {
        futures::select! {
            result = listener.accept().fuse() => {
                match result {
                    Ok((stream, _)) => {
                        let user_list = cfg.users.clone();
                        let token_store = token_store.clone();
                        task::spawn(accept_connection(stream, user_list, token_store));
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = shutdown.recv().fuse() => break,
        }
    }
    Ok(())
}

async fn accept_connection(stream: TcpStream, user_list: Vec<String>, token_store: TokenStore) {
    // Peek at the request headers to distinguish WebSocket upgrades from plain HTTP.
    // peek() does not consume bytes, so the WebSocket handshake path is unaffected.
    let mut peek_buf = vec![0u8; 4096];
    let n = match stream.peek(&mut peek_buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let header_bytes = &peek_buf[..n];

    let is_websocket = header_bytes
        .windows(9)
        .any(|w| w.eq_ignore_ascii_case(b"websocket"));

    if !is_websocket {
        let response: &[u8] = if header_bytes.starts_with(b"GET / ") {
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nI am running"
        } else {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        };
        let _ = (&stream).write_all(response).await;
        return;
    }

    let mut path = String::new();
    #[allow(clippy::result_large_err)]
    let callback = |req: &Request, response: Response| {
        path = req.uri().path().to_string();
        if path != "/control" && path != "/mux" {
            let resp = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Some("Not found".into()))
                .unwrap();
            return Err(resp);
        }
        Ok(response)
    };

    let ws_stream = match accept_hdr_async(stream, callback).await {
        Ok(s) => s,
        Err(_) => return,
    };

    if path.ends_with("control") {
        process_control(ws_stream, user_list, token_store).await;
    } else {
        run_server_mux_session(ws_stream, token_store).await;
    }
}

// ── /control ─────────────────────────────────────────────────────────────────

async fn process_control(
    mut ws_stream: WebSocketStream<TcpStream>,
    user_list: Vec<String>,
    token_store: TokenStore,
) {
    let msg = match ws_stream.next().await {
        Some(Ok(m)) if m.is_text() => m,
        _ => {
            let _ = ws_stream.close(None).await;
            return;
        }
    };

    let text = msg.to_text().unwrap_or("");
    if !text.starts_with("Hello:") {
        let _ = ws_stream.close(None).await;
        return;
    }

    let parts: Vec<&str> = text.splitn(3, ':').collect();
    if parts.len() < 3 {
        let _ = ws_stream.close(None).await;
        return;
    }
    let client_version = parts[1];
    let client_user_id = parts[2];

    let installed = match Version::parse(client_version) {
        Ok(v) => v,
        Err(_) => {
            let _ = ws_stream
                .send(Message::Text("Bye:Invalid version format".into()))
                .await;
            let _ = ws_stream.close(None).await;
            return;
        }
    };
    let required = VersionReq::parse(SUPPORTED_VERSION).unwrap();
    if !required.matches(&installed) {
        let _ = ws_stream
            .send(Message::Text(
                "Bye:Unsupported version. Download from https://github.com/echo-proxy/echo-proxy-core/releases".into(),
            ))
            .await;
        let _ = ws_stream.close(None).await;
        return;
    }

    if !user_list.contains(&client_user_id.to_string()) {
        let _ = ws_stream
            .send(Message::Text("Bye:User not allowed".into()))
            .await;
        let _ = ws_stream.close(None).await;
        return;
    }

    let token = Uuid::new_v4();
    token_store.lock().unwrap().insert(token);
    let _ = ws_stream
        .send(Message::Text(format!("Hi:{}", token).into()))
        .await;
}

// ── /mux session ─────────────────────────────────────────────────────────────

async fn run_server_mux_session(ws_stream: WebSocketStream<TcpStream>, token_store: TokenStore) {
    let (ws_sink, mut ws_src) = ws_stream.split();

    // First frame must be HELLO with a valid token.
    let token_uuid = loop {
        match ws_src.next().await {
            Some(Ok(Message::Binary(data))) => match Frame::decode(&data) {
                Ok(Frame::Hello(t)) => match t.parse::<Uuid>() {
                    Ok(u) => break u,
                    Err(_) => return,
                },
                _ => return,
            },
            Some(Ok(Message::Ping(_))) => continue,
            _ => return,
        }
    };

    // Consume the token (single-use; client must reconnect /control if ws drops).
    if !token_store.lock().unwrap().remove(&token_uuid) {
        return;
    }

    let (frame_tx, frame_rx) = frame_channel(128);
    let registry = StreamRegistry::new();

    // Writer loop: drains frame_rx and sends encoded frames over ws.
    let writer_registry = registry.clone();
    task::spawn(async move {
        while let Ok(frame) = frame_rx.recv().await {
            let bytes = frame.encode();
            if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        writer_registry.close_all();
    });

    // Reader loop: decode incoming frames and dispatch.
    while let Some(msg) = ws_src.next().await {
        let data = match msg {
            Ok(Message::Binary(d)) => d,
            Ok(Message::Ping(_)) => continue,
            _ => break,
        };
        match Frame::decode(&data) {
            Ok(Frame::Open { id, dest }) => {
                handle_open(id, dest, frame_tx.clone(), registry.clone()).await;
            }
            Ok(Frame::Data { id, bytes }) => {
                registry.route(id, bytes).await;
            }
            Ok(Frame::Close { id }) => {
                registry.close(id);
            }
            _ => {}
        }
    }
    registry.close_all();
}

async fn handle_open(id: u32, dest: String, frame_tx: FrameTx, registry: StreamRegistry) {
    if !is_dest_allowed(&dest) {
        let _ = frame_tx.send(Frame::OpenAck { id, ok: false }).await;
        return;
    }
    match TcpStream::connect(&dest).await {
        Ok(tcp) => {
            let data_rx = registry.register(id);
            let _ = frame_tx.send(Frame::OpenAck { id, ok: true }).await;
            task::spawn(async move {
                let (mut tcp_reader, mut tcp_writer) = tcp.split();

                // Incoming ws DATA → tcp writer
                let write_task = task::spawn(async move {
                    while let Ok(data) = data_rx.recv().await {
                        if tcp_writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                });

                // tcp reader → outgoing ws DATA
                let mut buf = vec![0u8; 8192];
                loop {
                    match tcp_reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if frame_tx
                                .send(Frame::Data {
                                    id,
                                    bytes: buf[..n].to_vec(),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                let _ = frame_tx.send(Frame::Close { id }).await;
                registry.close(id);
                drop(write_task);
            });
        }
        Err(_) => {
            registry.close(id);
            let _ = frame_tx.send(Frame::OpenAck { id, ok: false }).await;
        }
    }
}

// ── SSRF protection ──────────────────────────────────────────────────────────

/// Returns `false` for IP literal addresses in private/loopback/link-local
/// ranges. Hostname-based destinations pass (DNS-based SSRF is out of scope).
pub fn is_dest_allowed(dest: &str) -> bool {
    let host = match dest.rfind(':') {
        Some(colon) => dest[..colon].trim_matches(|c| c == '[' || c == ']'),
        None => dest,
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(ip);
    }
    true
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback() && !v4.is_private() && !v4.is_link_local() && !v4.is_unspecified()
        }
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast(),
    }
}

pub fn make_shutdown_channel() -> (channel::Sender<()>, Receiver<()>) {
    channel::bounded(1)
}
