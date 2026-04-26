pub mod geoip;
pub mod geosite;

use async_std::{
    channel::{self, Receiver},
    io::BufReader,
    net::{TcpListener, TcpStream},
    task,
};
use async_tungstenite::{
    async_std::connect_async,
    tungstenite::{
        Message,
        handshake::client::generate_key,
        http::{Request, Uri},
    },
};
use core_lib::{Frame, FrameTx, StreamRegistry, frame_channel};
use futures::{AsyncReadExt, AsyncWriteExt, FutureExt, SinkExt, StreamExt, io::AsyncBufReadExt};
use geosite::GeoSiteMatcher;
use ipnet::IpNet;
use std::{
    collections::HashMap,
    net::IpAddr,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

type PendingAcks = Arc<Mutex<HashMap<u32, channel::Sender<bool>>>>;

// ── Routing rules ─────────────────────────────────────────────────────────────

/// A single host-matching rule.
///
/// Patterns:
/// - `example.com`       — matches any port
/// - `example.com:8080`  — matches only that port
/// - `*.example.com`     — matches any subdomain (not the apex)
/// - `127.0.0.1`         — plain IP string, any port
#[derive(Debug, Clone)]
pub struct HostPattern {
    host: String,
    port: Option<u16>,
}

impl HostPattern {
    pub fn parse(s: &str) -> Self {
        let s = s.to_lowercase();
        if let Some((h, p)) = s.rsplit_once(':')
            && let Ok(port) = p.parse::<u16>()
        {
            return Self {
                host: h.to_string(),
                port: Some(port),
            };
        }
        Self {
            host: s,
            port: None,
        }
    }

    /// Match against a resolved `host` (lowercase) and numeric `port`.
    fn matches(&self, host: &str, port: u16) -> bool {
        if let Some(p) = self.port
            && p != port
        {
            return false;
        }
        if self.host.starts_with("*.") {
            let suffix = &self.host[2..];
            host.ends_with(suffix)
                && host.len() > suffix.len()
                && host[..host.len() - suffix.len()].ends_with('.')
        } else {
            self.host == host
        }
    }
}

/// Routing configuration passed to [`run_proxy`].
#[derive(Debug, Default)]
pub struct RoutingConfig {
    /// Always tunnel through the remote proxy.
    pub proxy: Vec<HostPattern>,
    /// Always connect directly (bypass the proxy).
    pub bypass: Vec<HostPattern>,
    /// Domain rules loaded from a GeoSite dat (e.g. dlc.dat).
    pub bypass_geosite: Option<GeoSiteMatcher>,
    /// Additional CIDR ranges to bypass (typically CN + private).
    pub bypass_cidrs: Vec<IpNet>,
}

impl RoutingConfig {
    /// Decide how to route a connection to `host:port`.
    ///
    /// Returns `true` if the connection should bypass the remote proxy (direct
    /// connect). Priority (highest first):
    /// 1. Explicit `proxy` rules  → tunnel
    /// 2. Explicit `bypass` rules → direct
    /// 3. GeoSite domain rules    → direct
    /// 4. GeoIP CIDR rules        → direct (IP targets only)
    /// 5. Default                 → tunnel
    pub fn should_bypass(&self, host: &str, port: u16) -> bool {
        let h = host.to_lowercase();

        // 1. Explicit proxy rules override everything.
        if self.proxy.iter().any(|r| r.matches(&h, port)) {
            return false;
        }

        // 2. Explicit bypass rules.
        if self.bypass.iter().any(|r| r.matches(&h, port)) {
            return true;
        }

        // 3. GeoSite domain bypass.
        if let Some(gs) = &self.bypass_geosite
            && gs.matches(&h)
        {
            return true;
        }

        // 4. CIDR bypass — only when the target is a bare IP address.
        if let Ok(ip) = h.parse::<IpAddr>()
            && self.bypass_cidrs.iter().any(|n| n.contains(&ip))
        {
            return true;
        }

        false
    }
}

// ── Mux / WebSocket helpers ───────────────────────────────────────────────────

pub fn make_shutdown_channel() -> (channel::Sender<()>, Receiver<()>) {
    channel::bounded(1)
}

fn build_request(
    url_str: &str,
) -> Result<Request<()>, async_tungstenite::tungstenite::http::Error> {
    let uri = Uri::from_str(url_str).unwrap();
    Request::builder()
        .uri(url_str)
        .header("Host", uri.host().unwrap())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .body(())
}

/// Handshake with /control. `version` defaults to the package version at compile time.
pub async fn obtain_token(endpoint: &str, user: &str) -> Option<String> {
    obtain_token_with_version(endpoint, user, env!("CARGO_PKG_VERSION")).await
}

/// Same as `obtain_token` but with an explicit version string (useful for testing bad versions).
pub async fn obtain_token_with_version(
    endpoint: &str,
    user: &str,
    version: &str,
) -> Option<String> {
    let mut base = endpoint.to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    let (mut ws, _) = connect_async(format!("{}control", base)).await.ok()?;

    ws.send(Message::Text(format!("Hello:{}:{}", version, user).into()))
        .await
        .ok()?;
    ws.flush().await.ok()?;

    let msg = ws.next().await?.ok()?;
    let text = msg.to_text().ok()?;
    if let Some(token) = text.strip_prefix("Hi:") {
        Some(token.to_string())
    } else {
        tracing::warn!("{}", text.strip_prefix("Bye:").unwrap_or(text));
        None
    }
}

/// Run the local HTTP proxy. Accepts connections on `local_listener` and tunnels
/// them through the mux connection at `endpoint` using the provided `token`.
/// Exits when `shutdown` is closed/sent.
pub async fn run_proxy(
    endpoint: String,
    local_listener: TcpListener,
    token: String,
    shutdown: Receiver<()>,
) -> std::io::Result<()> {
    run_proxy_with_routing(
        endpoint,
        local_listener,
        token,
        shutdown,
        RoutingConfig::default(),
    )
    .await
}

/// Like [`run_proxy`] but with explicit routing rules.
pub async fn run_proxy_with_routing(
    endpoint: String,
    local_listener: TcpListener,
    token: String,
    shutdown: Receiver<()>,
    routing: RoutingConfig,
) -> std::io::Result<()> {
    let mux_url = {
        let mut base = endpoint.clone();
        if !base.ends_with('/') {
            base.push('/');
        }
        format!("{}mux", base)
    };

    let (ws_stream, _) = connect_async(build_request(&mux_url).unwrap())
        .await
        .map_err(std::io::Error::other)?;

    let (ws_sink, ws_src) = ws_stream.split();

    // Send HELLO with token.
    ws_sink
        .send(Message::Binary(Frame::Hello(token).encode().into()))
        .await
        .map_err(std::io::Error::other)?;

    let (frame_tx, frame_rx) = frame_channel(128);
    let registry = StreamRegistry::new();
    let pending_acks: PendingAcks = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));
    let routing = Arc::new(routing);

    // Writer loop: frame_rx → ws sink
    task::spawn(async move {
        while let Ok(frame) = frame_rx.recv().await {
            let bytes = frame.encode();
            if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    // Reader loop: ws → Frame → route
    {
        let registry = registry.clone();
        let pending_acks = pending_acks.clone();
        task::spawn(async move {
            run_client_reader(ws_src, registry, pending_acks).await;
        });
    }

    tracing::info!(
        addr = %local_listener.local_addr().unwrap(),
        "http proxy listening"
    );

    loop {
        futures::select! {
            result = local_listener.accept().fuse() => {
                match result {
                    Ok((stream, _)) => {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let ftx = frame_tx.clone();
                        let reg = registry.clone();
                        let packs = pending_acks.clone();
                        let routing = routing.clone();
                        task::spawn(handle_local_connection(stream, id, ftx, reg, packs, routing));
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = shutdown.recv().fuse() => break,
        }
    }
    Ok(())
}

async fn run_client_reader<S>(mut ws_src: S, registry: StreamRegistry, pending_acks: PendingAcks)
where
    S: futures::Stream<Item = Result<Message, async_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    while let Some(msg) = ws_src.next().await {
        let data = match msg {
            Ok(Message::Binary(d)) => d,
            Ok(Message::Ping(_)) => continue,
            _ => break,
        };
        match Frame::decode(&data) {
            Ok(Frame::OpenAck { id, ok }) => {
                if let Some(tx) = pending_acks.lock().unwrap().remove(&id) {
                    let _ = tx.try_send(ok);
                }
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
    // Drop all pending ack senders so waiting OPEN_ACK futures unblock immediately.
    pending_acks.lock().unwrap().clear();
}

async fn handle_local_connection(
    tcp_stream: TcpStream,
    stream_id: u32,
    frame_tx: FrameTx,
    registry: StreamRegistry,
    pending_acks: PendingAcks,
    routing: Arc<RoutingConfig>,
) {
    let (host, headers, body) = match parse_request_header(tcp_stream.clone()).await {
        Some(r) => r,
        None => return,
    };
    let is_connect = headers.starts_with("CONNECT ");

    // Determine the bare host and port for routing.
    let (bare_host, port) = if let Some((h, p)) = host.rsplit_once(':') {
        let port = p.parse::<u16>().unwrap_or(80);
        (h.to_string(), port)
    } else {
        (host.clone(), 80)
    };

    if routing.should_bypass(&bare_host, port) {
        tracing::info!(host = %bare_host, port, method = if is_connect { "CONNECT" } else { "plain" }, route = "direct", "→");
        handle_direct(tcp_stream, &host, is_connect, headers, body).await;
        return;
    }

    tracing::info!(host = %bare_host, port, method = if is_connect { "CONNECT" } else { "plain" }, route = "proxy", "→");
    handle_via_mux(
        tcp_stream,
        MuxState {
            stream_id,
            frame_tx,
            registry,
            pending_acks,
        },
        ProxyRequest {
            host,
            is_connect,
            headers,
            body,
        },
    )
    .await;
}

/// Forward the connection directly to the target without going through the mux.
async fn handle_direct(
    mut tcp_stream: TcpStream,
    host: &str,
    is_connect: bool,
    headers: String,
    body: Vec<u8>,
) {
    let mut upstream = match TcpStream::connect(host).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(host, "direct connect failed: {e}");
            let _ = tcp_stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    if is_connect {
        if tcp_stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else {
        let mut data = headers.into_bytes();
        data.extend_from_slice(&body);
        if upstream.write_all(&data).await.is_err() {
            return;
        }
    }

    relay_tcp(tcp_stream, upstream).await;
}

/// Relay bytes bidirectionally between two TCP streams until either side closes.
async fn relay_tcp(a: TcpStream, b: TcpStream) {
    let (mut ar, mut aw) = a.split();
    let (mut br, mut bw) = b.split();

    let a_to_b = task::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match ar.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut buf = vec![0u8; 8192];
    loop {
        match br.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if aw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }

    drop(a_to_b);
}

struct MuxState {
    stream_id: u32,
    frame_tx: FrameTx,
    registry: StreamRegistry,
    pending_acks: PendingAcks,
}

struct ProxyRequest {
    host: String,
    is_connect: bool,
    headers: String,
    body: Vec<u8>,
}

/// Forward the connection through the mux WebSocket tunnel.
async fn handle_via_mux(tcp_stream: TcpStream, state: MuxState, request: ProxyRequest) {
    let MuxState {
        stream_id,
        frame_tx,
        registry,
        pending_acks,
    } = state;
    let ProxyRequest {
        host,
        is_connect,
        headers,
        body,
    } = request;

    // Register the stream BEFORE sending OPEN to avoid a DATA routing race.
    let data_rx = registry.register(stream_id);

    // Register a one-shot for OPEN_ACK.
    let (ack_tx, ack_rx) = channel::bounded::<bool>(1);
    pending_acks.lock().unwrap().insert(stream_id, ack_tx);

    if frame_tx
        .send(Frame::Open {
            id: stream_id,
            dest: host,
        })
        .await
        .is_err()
    {
        registry.close(stream_id);
        return;
    }

    let ok = ack_rx.recv().await.unwrap_or(false);
    if !ok {
        tracing::warn!(stream_id, "mux OPEN_ACK failed, closing stream");
        registry.close(stream_id);
        let _ = tcp_stream
            .clone()
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    let mut tcp_stream = tcp_stream;

    if is_connect {
        if tcp_stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            registry.close(stream_id);
            return;
        }
    } else {
        let mut request_data = headers.into_bytes();
        request_data.extend_from_slice(&body);
        if frame_tx
            .send(Frame::Data {
                id: stream_id,
                bytes: request_data,
            })
            .await
            .is_err()
        {
            registry.close(stream_id);
            return;
        }
    }

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    // ws DATA → tcp write
    let write_task = task::spawn(async move {
        while let Ok(data) = data_rx.recv().await {
            if tcp_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    // tcp read → ws DATA
    let mut buf = vec![0u8; 8192];
    loop {
        match tcp_reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if frame_tx
                    .send(Frame::Data {
                        id: stream_id,
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
    let _ = frame_tx.send(Frame::Close { id: stream_id }).await;
    registry.close(stream_id);
    drop(write_task);
}

/// Read HTTP request headers and optional body from a cloned TcpStream.
/// Returns None on malformed input or if limits are exceeded.
async fn parse_request_header(tcp_stream: TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut headers = String::new();
    let mut host = String::new();
    let mut content_length: usize = 0;
    let mut reader = BufReader::new(tcp_stream);

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            break;
        }
        if headers.len() + line.len() > MAX_HEADER_BYTES {
            return None;
        }
        headers.push_str(&line);

        let lower = line.to_ascii_lowercase();
        if lower.starts_with("host:") {
            host = line["host:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .to_string();
            if !host.contains(':') {
                host.push_str(":80");
            }
        }
        if lower.starts_with("content-length:") {
            content_length = line["content-length:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .parse::<usize>()
                .unwrap_or(0);
            if content_length > MAX_CONTENT_LENGTH {
                return None;
            }
        }
        if line == "\r\n" {
            break;
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }

    Some((host, headers, body))
}

// ── Resilient proxy (mux auto-reconnect) ─────────────────────────────────────

const OPEN_ACK_TIMEOUT_SECS: u64 = 10;
const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_SECS: u64 = 30;

struct MuxConn {
    frame_tx: FrameTx,
    registry: StreamRegistry,
    pending_acks: PendingAcks,
}

impl MuxConn {
    /// Drop all pending ack senders and close all streams so in-flight requests
    /// unblock immediately instead of waiting for the OPEN_ACK timeout.
    fn close(&self) {
        self.pending_acks.lock().unwrap().clear();
        self.registry.close_all();
    }
}

type SharedMux = Arc<Mutex<Option<Arc<MuxConn>>>>;

/// Like [`run_proxy_with_routing`] but reconnects the remote WebSocket
/// automatically when the connection drops (Nginx restart, idle timeout, etc.).
///
/// Token acquisition and re-authentication are handled internally; the caller
/// only needs to supply the `user` identifier used for `/control`.
pub async fn run_proxy_resilient(
    endpoint: String,
    user: String,
    local_listener: TcpListener,
    shutdown: Receiver<()>,
    routing: RoutingConfig,
) -> std::io::Result<()> {
    let shared: SharedMux = Arc::new(Mutex::new(None));
    let next_id = Arc::new(AtomicU32::new(1));
    let routing = Arc::new(routing);

    // Keeper task: maintains the /mux WebSocket and reconnects when it drops.
    let (keeper_stop_tx, keeper_stop_rx) = channel::bounded::<()>(1);
    {
        let shared = shared.clone();
        let endpoint = endpoint.clone();
        task::spawn(async move {
            run_mux_keeper(endpoint, user, shared, keeper_stop_rx).await;
        });
    }

    tracing::info!(
        addr = %local_listener.local_addr().unwrap(),
        "http proxy listening"
    );

    loop {
        futures::select! {
            result = local_listener.accept().fuse() => {
                match result {
                    Ok((stream, _)) => {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let shared = shared.clone();
                        let routing = routing.clone();
                        task::spawn(handle_local_connection_resilient(
                            stream, id, shared, routing,
                        ));
                    }
                    Err(e) => {
                        let _ = keeper_stop_tx.try_send(());
                        return Err(e);
                    }
                }
            }
            _ = shutdown.recv().fuse() => {
                let _ = keeper_stop_tx.try_send(());
                break;
            }
        }
    }
    Ok(())
}

async fn run_mux_keeper(endpoint: String, user: String, shared: SharedMux, shutdown: Receiver<()>) {
    let mut backoff = Duration::from_millis(RECONNECT_INITIAL_MS);
    let max_backoff = Duration::from_secs(RECONNECT_MAX_SECS);

    loop {
        // Obtain a session token; retry with exponential backoff on failure.
        let token = loop {
            match obtain_token(&endpoint, &user).await {
                Some(t) => {
                    backoff = Duration::from_millis(RECONNECT_INITIAL_MS);
                    break t;
                }
                None => {
                    tracing::warn!(delay = ?backoff, "control handshake failed, retrying");
                    futures::select! {
                        _ = task::sleep(backoff).fuse() => {}
                        _ = shutdown.recv().fuse() => return,
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        };

        // Connect to the /mux endpoint.
        let mux_url = {
            let mut base = endpoint.clone();
            if !base.ends_with('/') {
                base.push('/');
            }
            format!("{}mux", base)
        };

        let ws_stream = match connect_async(build_request(&mux_url).unwrap()).await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!(delay = ?backoff, "mux connect failed: {e}, retrying");
                futures::select! {
                    _ = task::sleep(backoff).fuse() => {}
                    _ = shutdown.recv().fuse() => return,
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        let (ws_sink, ws_src) = ws_stream.split();

        if ws_sink
            .send(Message::Binary(Frame::Hello(token).encode().into()))
            .await
            .is_err()
        {
            tracing::warn!("mux HELLO send failed, retrying");
            continue;
        }

        backoff = Duration::from_millis(RECONNECT_INITIAL_MS);

        let (frame_tx, frame_rx) = frame_channel(128);
        let registry = StreamRegistry::new();
        let pending_acks: PendingAcks = Arc::new(Mutex::new(HashMap::new()));

        // Publish the new connection so request handlers can use it.
        *shared.lock().unwrap() = Some(Arc::new(MuxConn {
            frame_tx,
            registry: registry.clone(),
            pending_acks: pending_acks.clone(),
        }));
        tracing::info!("mux session established");

        // Each task sends () when it exits; the first signal wakes the keeper.
        let (disc_tx, disc_rx) = channel::bounded::<()>(2);

        // Writer loop: frame_rx → ws sink.
        {
            let disc_tx = disc_tx.clone();
            let reg = registry.clone();
            task::spawn(async move {
                while let Ok(frame) = frame_rx.recv().await {
                    let bytes = frame.encode();
                    if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                reg.close_all();
                let _ = disc_tx.try_send(());
            });
        }

        // Reader loop: ws → Frame → dispatch.
        {
            let disc_tx = disc_tx.clone();
            task::spawn(async move {
                run_client_reader(ws_src, registry, pending_acks).await;
                let _ = disc_tx.try_send(());
            });
        }

        // Wait for a disconnect signal or a graceful shutdown request.
        futures::select! {
            _ = disc_rx.recv().fuse() => {
                tracing::warn!("mux disconnected, will reconnect");
                if let Some(c) = shared.lock().unwrap().take() {
                    c.close();
                }
            }
            _ = shutdown.recv().fuse() => {
                if let Some(c) = shared.lock().unwrap().take() {
                    c.close();
                }
                return;
            }
        }
    }
}

async fn handle_local_connection_resilient(
    tcp_stream: TcpStream,
    stream_id: u32,
    shared_mux: SharedMux,
    routing: Arc<RoutingConfig>,
) {
    let (host, headers, body) = match parse_request_header(tcp_stream.clone()).await {
        Some(r) => r,
        None => return,
    };
    let is_connect = headers.starts_with("CONNECT ");

    let (bare_host, port) = if let Some((h, p)) = host.rsplit_once(':') {
        let port = p.parse::<u16>().unwrap_or(80);
        (h.to_string(), port)
    } else {
        (host.clone(), 80)
    };

    if routing.should_bypass(&bare_host, port) {
        tracing::info!(
            host = %bare_host, port,
            method = if is_connect { "CONNECT" } else { "plain" },
            route = "direct", "→"
        );
        handle_direct(tcp_stream, &host, is_connect, headers, body).await;
        return;
    }

    tracing::info!(
        host = %bare_host, port,
        method = if is_connect { "CONNECT" } else { "plain" },
        route = "proxy", "→"
    );

    // Snapshot the current mux connection; return 502 immediately if unavailable.
    let conn = shared_mux.lock().unwrap().clone();
    let conn = match conn {
        Some(c) => c,
        None => {
            tracing::warn!(host = %bare_host, "mux unavailable (reconnecting), returning 502");
            let _ = tcp_stream
                .clone()
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    handle_via_mux_with_timeout(tcp_stream, stream_id, conn, host, is_connect, headers, body).await;
}

/// Like [`handle_via_mux`] but takes an [`Arc<MuxConn>`] snapshot and enforces
/// an [`OPEN_ACK_TIMEOUT_SECS`] deadline so the browser never hangs indefinitely
/// when the tunnel is dead.
async fn handle_via_mux_with_timeout(
    tcp_stream: TcpStream,
    stream_id: u32,
    conn: Arc<MuxConn>,
    host: String,
    is_connect: bool,
    headers: String,
    body: Vec<u8>,
) {
    use async_std::future::timeout;

    let data_rx = conn.registry.register(stream_id);
    let (ack_tx, ack_rx) = channel::bounded::<bool>(1);
    conn.pending_acks.lock().unwrap().insert(stream_id, ack_tx);

    if conn
        .frame_tx
        .send(Frame::Open {
            id: stream_id,
            dest: host,
        })
        .await
        .is_err()
    {
        conn.registry.close(stream_id);
        return;
    }

    let ok = match timeout(Duration::from_secs(OPEN_ACK_TIMEOUT_SECS), ack_rx.recv()).await {
        Ok(Ok(v)) => v,
        _ => false,
    };

    if !ok {
        tracing::warn!(stream_id, "OPEN_ACK failed or timed out, returning 502");
        conn.registry.close(stream_id);
        let _ = tcp_stream
            .clone()
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    let mut tcp_stream = tcp_stream;

    if is_connect {
        if tcp_stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            conn.registry.close(stream_id);
            return;
        }
    } else {
        let mut request_data = headers.into_bytes();
        request_data.extend_from_slice(&body);
        if conn
            .frame_tx
            .send(Frame::Data {
                id: stream_id,
                bytes: request_data,
            })
            .await
            .is_err()
        {
            conn.registry.close(stream_id);
            return;
        }
    }

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    let write_task = task::spawn(async move {
        while let Ok(data) = data_rx.recv().await {
            if tcp_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    let mut buf = vec![0u8; 8192];
    loop {
        match tcp_reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if conn
                    .frame_tx
                    .send(Frame::Data {
                        id: stream_id,
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
    let _ = conn.frame_tx.send(Frame::Close { id: stream_id }).await;
    conn.registry.close(stream_id);
    drop(write_task);
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> HostPattern {
        HostPattern::parse(s)
    }

    fn config(proxy: &[&str], bypass: &[&str]) -> RoutingConfig {
        RoutingConfig {
            proxy: proxy.iter().map(|s| pat(s)).collect(),
            bypass: bypass.iter().map(|s| pat(s)).collect(),
            bypass_geosite: None,
            bypass_cidrs: vec![],
        }
    }

    #[test]
    fn wildcard_subdomain() {
        let r = pat("*.example.com");
        assert!(r.matches("foo.example.com", 80));
        assert!(r.matches("foo.example.com", 443));
        assert!(!r.matches("example.com", 80));
        assert!(!r.matches("notexample.com", 80));
    }

    #[test]
    fn port_specific_rule() {
        let r = pat("example.com:8080");
        assert!(r.matches("example.com", 8080));
        assert!(!r.matches("example.com", 80));
    }

    #[test]
    fn proxy_overrides_bypass() {
        let cfg = config(&["github.com"], &["github.com"]);
        assert!(!cfg.should_bypass("github.com", 443));
    }

    #[test]
    fn bypass_host_rule() {
        let cfg = config(&[], &["localhost", "127.0.0.1"]);
        assert!(cfg.should_bypass("localhost", 80));
        assert!(cfg.should_bypass("127.0.0.1", 80));
        assert!(!cfg.should_bypass("example.com", 80));
    }

    #[test]
    fn cidr_bypass() {
        use std::str::FromStr;
        let mut cfg = config(&[], &[]);
        cfg.bypass_cidrs
            .push(IpNet::from_str("192.168.0.0/16").unwrap());
        assert!(cfg.should_bypass("192.168.1.1", 80));
        assert!(!cfg.should_bypass("1.2.3.4", 80));
    }

    #[test]
    fn proxy_overrides_cidr() {
        use std::str::FromStr;
        let mut cfg = config(&["192.168.1.1"], &[]);
        cfg.bypass_cidrs
            .push(IpNet::from_str("192.168.0.0/16").unwrap());
        assert!(!cfg.should_bypass("192.168.1.1", 80));
    }

    #[test]
    fn default_routing_proxies_everything() {
        let cfg = RoutingConfig::default();
        assert!(!cfg.should_bypass("example.com", 443));
        assert!(!cfg.should_bypass("1.2.3.4", 80));
    }

    fn geosite_config(domains: &[&str]) -> RoutingConfig {
        use crate::geosite::GeoSiteMatcher;
        RoutingConfig {
            proxy: vec![],
            bypass: vec![],
            bypass_geosite: Some(GeoSiteMatcher {
                domains: domains.iter().map(|s| format!(".{s}")).collect(),
                fulls: vec![],
                keywords: vec![],
                regexes: vec![],
            }),
            bypass_cidrs: vec![],
        }
    }

    #[test]
    fn geosite_domain_bypassed() {
        let cfg = geosite_config(&["baidu.com", "alicdn.com"]);
        assert!(cfg.should_bypass("baidu.com", 443));
        assert!(cfg.should_bypass("www.baidu.com", 443));
        assert!(cfg.should_bypass("g.alicdn.com", 443));
        assert!(!cfg.should_bypass("google.com", 443));
    }

    #[test]
    fn proxy_rule_overrides_geosite() {
        use crate::geosite::GeoSiteMatcher;
        // Exact-host proxy rule overrides geosite for that exact host only.
        let cfg = RoutingConfig {
            proxy: vec![pat("baidu.com")],
            bypass: vec![],
            bypass_geosite: Some(GeoSiteMatcher {
                domains: vec![".baidu.com".to_string()],
                fulls: vec![],
                keywords: vec![],
                regexes: vec![],
            }),
            bypass_cidrs: vec![],
        };
        // Exact match: proxy wins.
        assert!(!cfg.should_bypass("baidu.com", 443));
        // Subdomain: no proxy rule covers it, geosite wins.
        assert!(cfg.should_bypass("www.baidu.com", 443));

        // Wildcard proxy rule covers all subdomains.
        let cfg2 = RoutingConfig {
            proxy: vec![pat("baidu.com"), pat("*.baidu.com")],
            bypass: vec![],
            bypass_geosite: Some(GeoSiteMatcher {
                domains: vec![".baidu.com".to_string()],
                fulls: vec![],
                keywords: vec![],
                regexes: vec![],
            }),
            bypass_cidrs: vec![],
        };
        assert!(!cfg2.should_bypass("baidu.com", 443));
        assert!(!cfg2.should_bypass("www.baidu.com", 443));
    }
}
