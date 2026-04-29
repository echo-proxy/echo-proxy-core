pub mod geoip;
pub mod geosite;

use geosite::GeoSiteMatcher;
use ipnet::IpNet;
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    task,
};
use wtransport::{ClientConfig, Connection, Endpoint};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;
const OPEN_ACK_TIMEOUT_SECS: u64 = 10;
const WEBTRANSPORT_KEEP_ALIVE_SECS: u64 = 10;
const WEBTRANSPORT_IDLE_TIMEOUT_SECS: u64 = 120;
const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_SECS: u64 = 30;

// ── SOCKS5 constants ──────────────────────────────────────────────────────────
const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_AUTH_NO_AUTH: u8 = 0x00;
const SOCKS5_AUTH_NO_ACCEPTABLE: u8 = 0xFF;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
const SOCKS5_REP_SUCCESS: u8 = 0x00;
const SOCKS5_REP_GENERAL_FAILURE: u8 = 0x01;
const SOCKS5_REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const SOCKS5_REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

// ── Routing rules ─────────────────────────────────────────────────────────────

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

#[derive(Debug, Default)]
pub struct RoutingConfig {
    pub proxy: Vec<HostPattern>,
    pub bypass: Vec<HostPattern>,
    pub bypass_geosite: Option<GeoSiteMatcher>,
    pub bypass_cidrs: Vec<IpNet>,
}

impl RoutingConfig {
    pub fn should_bypass(&self, host: &str, port: u16) -> bool {
        let h = host.to_lowercase();

        if self.proxy.iter().any(|r| r.matches(&h, port)) {
            return false;
        }
        if self.bypass.iter().any(|r| r.matches(&h, port)) {
            return true;
        }
        if let Some(gs) = &self.bypass_geosite
            && gs.matches(&h)
        {
            return true;
        }
        if let Ok(ip) = h.parse::<IpAddr>()
            && self.bypass_cidrs.iter().any(|n| n.contains(&ip))
        {
            return true;
        }
        false
    }
}

// ── TLS trust configuration ───────────────────────────────────────────────────

/// Describes how the client should validate the server's TLS certificate.
#[derive(Debug, Clone)]
pub enum TlsTrustConfig {
    /// Trust certificates from the platform's native certificate store (default).
    NativeCerts,
    /// Accept any certificate without validation (tests only).
    NoCertValidation,
    /// Accept a certificate with the given SHA-256 hash (self-signed servers).
    CertHash(wtransport::tls::Sha256Digest),
}

impl TlsTrustConfig {
    pub fn build_client_config(&self) -> ClientConfig {
        match self {
            TlsTrustConfig::NativeCerts => ClientConfig::builder()
                .with_bind_default()
                .with_native_certs()
                .keep_alive_interval(Some(Duration::from_secs(WEBTRANSPORT_KEEP_ALIVE_SECS)))
                .max_idle_timeout(Some(Duration::from_secs(WEBTRANSPORT_IDLE_TIMEOUT_SECS)))
                .unwrap_or_else(|_| unreachable!("valid WebTransport idle timeout"))
                .build(),
            TlsTrustConfig::NoCertValidation => ClientConfig::builder()
                .with_bind_default()
                .with_no_cert_validation()
                .keep_alive_interval(Some(Duration::from_secs(WEBTRANSPORT_KEEP_ALIVE_SECS)))
                .max_idle_timeout(Some(Duration::from_secs(WEBTRANSPORT_IDLE_TIMEOUT_SECS)))
                .unwrap_or_else(|_| unreachable!("valid WebTransport idle timeout"))
                .build(),
            TlsTrustConfig::CertHash(hash) => ClientConfig::builder()
                .with_bind_default()
                .with_server_certificate_hashes([hash.clone()])
                .keep_alive_interval(Some(Duration::from_secs(WEBTRANSPORT_KEEP_ALIVE_SECS)))
                .max_idle_timeout(Some(Duration::from_secs(WEBTRANSPORT_IDLE_TIMEOUT_SECS)))
                .unwrap_or_else(|_| unreachable!("valid WebTransport idle timeout"))
                .build(),
        }
    }
}

// ── Shared WebTransport connection ────────────────────────────────────────────

type SharedConn = Arc<Mutex<Option<Arc<Connection>>>>;

pub fn make_shutdown_channel() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
    broadcast::channel(1)
}

// ── Resilient proxy ───────────────────────────────────────────────────────────

/// Run HTTP and optional SOCKS5 proxies backed by a WebTransport session that
/// automatically reconnects when the server drops.
pub async fn run_proxy_resilient(
    endpoint: String,
    user: String,
    trust: TlsTrustConfig,
    http_listener: TcpListener,
    socks_listener: Option<TcpListener>,
    mut shutdown: broadcast::Receiver<()>,
    routing: RoutingConfig,
) -> std::io::Result<()> {
    let shared: SharedConn = Arc::new(Mutex::new(None));
    let routing = Arc::new(routing);

    // Session keeper task
    let (keeper_stop_tx, keeper_stop_rx) = broadcast::channel::<()>(1);
    {
        let shared = shared.clone();
        let endpoint = endpoint.clone();
        let user = user.clone();
        let trust = trust.clone();
        task::spawn(run_session_keeper(
            endpoint,
            user,
            trust,
            shared,
            keeper_stop_rx,
        ));
    }

    tracing::info!(
        addr = %http_listener.local_addr().unwrap(),
        "http proxy listening"
    );

    if let Some(sl) = socks_listener {
        tracing::info!(addr = %sl.local_addr().unwrap(), "socks5 proxy listening");
        let shared = shared.clone();
        let routing = routing.clone();
        task::spawn(async move {
            loop {
                match sl.accept().await {
                    Ok((stream, _)) => {
                        task::spawn(handle_socks_connection(stream, shared.clone(), routing.clone()));
                    }
                    Err(_) => break,
                }
            }
        });
    }

    loop {
        tokio::select! {
            result = http_listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        task::spawn(handle_http_connection(stream, shared.clone(), routing.clone()));
                    }
                    Err(e) => {
                        let _ = keeper_stop_tx.send(());
                        return Err(e);
                    }
                }
            }
            _ = shutdown.recv() => {
                let _ = keeper_stop_tx.send(());
                break;
            }
        }
    }
    Ok(())
}

async fn run_session_keeper(
    endpoint: String,
    user: String,
    trust: TlsTrustConfig,
    shared: SharedConn,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut backoff = Duration::from_millis(RECONNECT_INITIAL_MS);
    let max_backoff = Duration::from_secs(RECONNECT_MAX_SECS);

    loop {
        let conn = loop {
            let config = trust.build_client_config();
            match connect_and_auth(&endpoint, &user, config).await {
                Some(c) => {
                    backoff = Duration::from_millis(RECONNECT_INITIAL_MS);
                    break c;
                }
                None => {
                    tracing::warn!(delay = ?backoff, "session connect/auth failed, retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown.recv() => return,
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        };

        tracing::info!("WebTransport session established");
        let conn = Arc::new(conn);
        *shared.lock().unwrap() = Some(conn.clone());

        // Wait for connection to drop
        let closed = conn.closed().await;
        tracing::warn!("session closed ({:?}), reconnecting", closed);
        *shared.lock().unwrap() = None;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(RECONNECT_INITIAL_MS)) => {}
            _ = shutdown.recv() => return,
        }
    }
}

/// Connect to the WebTransport endpoint and complete the auth handshake.
/// Returns the authenticated `Connection` on success.
pub async fn connect_and_auth(
    endpoint: &str,
    user: &str,
    config: ClientConfig,
) -> Option<Connection> {
    let ep = match Endpoint::client(config) {
        Ok(ep) => ep,
        Err(e) => {
            tracing::debug!("create client endpoint failed: {e}");
            return None;
        }
    };
    let conn = match ep.connect(endpoint).await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::debug!("connect failed: {e}");
            return None;
        }
    };

    // Open the auth control stream
    let mut ctrl = match conn.open_bi().await {
        Ok(opening) => match opening.await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("open auth stream failed: {e}");
                return None;
            }
        },
        Err(e) => {
            tracing::debug!("open_bi failed: {e}");
            return None;
        }
    };

    let msg = format!("Hello:{}:{}\n", env!("CARGO_PKG_VERSION"), user);
    if ctrl.0.write_all(msg.as_bytes()).await.is_err() {
        return None;
    }

    let resp = read_wt_message(&mut ctrl.1).await?;
    if resp.starts_with("Hi") {
        tracing::info!("auth ok");
        Some(conn)
    } else {
        let reason = resp.strip_prefix("Bye:").unwrap_or(&resp);
        tracing::warn!("{reason}");
        None
    }
}

/// Open a proxy stream to `dest` and get back the bidirectional stream halves.
async fn open_proxy_stream(
    conn: &Connection,
    dest: &str,
) -> Option<(wtransport::SendStream, wtransport::RecvStream)> {
    let mut stream = match conn.open_bi().await {
        Ok(opening) => opening.await.ok()?,
        Err(_) => return None,
    };

    let msg = format!("{dest}\n");
    stream.0.write_all(msg.as_bytes()).await.ok()?;

    let mut status = [0u8; 1];
    stream.1.read_exact(&mut status).await.ok()?;
    if status[0] == 0x00 {
        Some((stream.0, stream.1))
    } else {
        None
    }
}

// ── HTTP connection handler ───────────────────────────────────────────────────

async fn handle_http_connection(
    tcp: TcpStream,
    shared: SharedConn,
    routing: Arc<RoutingConfig>,
) {
    let (tcp, host, headers, body) = match parse_http_request(tcp).await {
        Some(r) => r,
        None => return,
    };
    let is_connect = headers.starts_with("CONNECT ");

    let (bare_host, port) = split_host_port(&host);

    if routing.should_bypass(&bare_host, port) {
        tracing::info!(host = %bare_host, port, method = if is_connect { "CONNECT" } else { "plain" }, route = "direct", "→");
        handle_direct(tcp, &host, is_connect, headers, body).await;
        return;
    }

    tracing::info!(host = %bare_host, port, method = if is_connect { "CONNECT" } else { "plain" }, route = "proxy", "→");

    let conn = {
        let guard = shared.lock().unwrap();
        guard.clone()
    };
    let conn = match conn {
        Some(c) => c,
        None => {
            tracing::warn!(host = %bare_host, "mux unavailable (reconnecting), returning 502");
            let _ = send_502(tcp).await;
            return;
        }
    };

    let result = tokio::time::timeout(
        Duration::from_secs(OPEN_ACK_TIMEOUT_SECS),
        open_proxy_stream(&conn, &host),
    )
    .await;

    let (wt_send, wt_recv) = match result {
        Ok(Some(s)) => s,
        _ => {
            tracing::warn!(host = %bare_host, "proxy stream open failed");
            let _ = send_502(tcp).await;
            return;
        }
    };

    finish_http_relay(tcp, wt_send, wt_recv, is_connect, headers, body).await;
}

async fn finish_http_relay(
    mut tcp: TcpStream,
    wt_send: wtransport::SendStream,
    wt_recv: wtransport::RecvStream,
    is_connect: bool,
    headers: String,
    body: Vec<u8>,
) {
    if is_connect {
        if tcp
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else {
        let mut data = headers.into_bytes();
        data.extend_from_slice(&body);
        // Forward initial HTTP request through the proxy stream
        relay_wt_stream(tcp, wt_send, wt_recv, Some(data)).await;
        return;
    }
    relay_wt_stream(tcp, wt_send, wt_recv, None).await;
}

// ── SOCKS5 connection handler ─────────────────────────────────────────────────

async fn handle_socks_connection(
    mut tcp: TcpStream,
    shared: SharedConn,
    routing: Arc<RoutingConfig>,
) {
    let host = match parse_socks5_request(&mut tcp).await {
        Some(h) => h,
        None => return,
    };

    let (bare_host, port) = split_host_port(&host);

    if routing.should_bypass(&bare_host, port) {
        tracing::info!(host = %bare_host, port, route = "direct", "→ socks5");
        handle_socks_direct(tcp, &host).await;
        return;
    }

    tracing::info!(host = %bare_host, port, route = "proxy", "→ socks5");

    let conn = {
        let guard = shared.lock().unwrap();
        guard.clone()
    };
    let conn = match conn {
        Some(c) => c,
        None => {
            let _ = tcp.write_all(&socks5_reply(SOCKS5_REP_GENERAL_FAILURE)).await;
            return;
        }
    };

    let result = tokio::time::timeout(
        Duration::from_secs(OPEN_ACK_TIMEOUT_SECS),
        open_proxy_stream(&conn, &host),
    )
    .await;

    let (wt_send, wt_recv) = match result {
        Ok(Some(s)) => s,
        _ => {
            let _ = tcp.write_all(&socks5_reply(SOCKS5_REP_GENERAL_FAILURE)).await;
            return;
        }
    };

    if tcp
        .write_all(&socks5_reply(SOCKS5_REP_SUCCESS))
        .await
        .is_err()
    {
        return;
    }

    relay_wt_stream(tcp, wt_send, wt_recv, None).await;
}

// ── Relay helpers ─────────────────────────────────────────────────────────────

/// Relay bytes between a local TCP stream and a WebTransport bidirectional stream.
/// `initial_data` is forwarded into the WT send stream before the relay loop starts.
async fn relay_wt_stream(
    tcp: TcpStream,
    mut wt_send: wtransport::SendStream,
    mut wt_recv: wtransport::RecvStream,
    initial_data: Option<Vec<u8>>,
) {
    if let Some(data) = initial_data {
        if wt_send.write_all(&data).await.is_err() {
            return;
        }
    }

    let (mut tcp_recv, mut tcp_send) = tcp.into_split();

    let wt_to_tcp = task::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match wt_recv.read(&mut buf).await {
                Ok(Some(0)) | Ok(None) | Err(_) => break,
                Ok(Some(n)) => {
                    if tcp_send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut buf = vec![0u8; 8192];
    loop {
        match tcp_recv.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if wt_send.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    drop(wt_to_tcp);
}

/// Relay bytes between two TCP streams until either side closes.
async fn relay_tcp(a: TcpStream, b: TcpStream) {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();

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

async fn handle_direct(
    mut tcp: TcpStream,
    host: &str,
    is_connect: bool,
    headers: String,
    body: Vec<u8>,
) {
    let mut upstream = match TcpStream::connect(host).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(host, "direct connect failed: {e}");
            let _ = send_502(tcp).await;
            return;
        }
    };

    if is_connect {
        if tcp
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

    relay_tcp(tcp, upstream).await;
}

async fn handle_socks_direct(mut tcp: TcpStream, host: &str) {
    let upstream = match TcpStream::connect(host).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(host, "socks5 direct connect failed: {e}");
            let _ = tcp.write_all(&socks5_reply(SOCKS5_REP_GENERAL_FAILURE)).await;
            return;
        }
    };
    let _ = tcp.write_all(&socks5_reply(SOCKS5_REP_SUCCESS)).await;
    relay_tcp(tcp, upstream).await;
}

// ── HTTP request parsing ──────────────────────────────────────────────────────

async fn parse_http_request(tcp: TcpStream) -> Option<(TcpStream, String, String, Vec<u8>)> {
    let mut headers = String::new();
    let mut host = String::new();
    let mut content_length: usize = 0;
    let mut reader = BufReader::new(tcp);

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

    let tcp = reader.into_inner();
    Some((tcp, host, headers, body))
}

// ── SOCKS5 helpers ────────────────────────────────────────────────────────────

fn socks5_reply(rep: u8) -> [u8; 10] {
    [SOCKS5_VERSION, rep, 0x00, SOCKS5_ATYP_IPV4, 0, 0, 0, 0, 0, 0]
}

async fn parse_socks5_request(stream: &mut TcpStream) -> Option<String> {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await.ok()?;
    if hdr[0] != SOCKS5_VERSION {
        return None;
    }
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await.ok()?;

    if methods.contains(&SOCKS5_AUTH_NO_AUTH) {
        stream
            .write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NO_AUTH])
            .await
            .ok()?;
    } else {
        let _ = stream
            .write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NO_ACCEPTABLE])
            .await;
        return None;
    }

    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await.ok()?;
    if req[0] != SOCKS5_VERSION {
        return None;
    }
    let cmd = req[1];
    let atyp = req[3];

    if cmd != SOCKS5_CMD_CONNECT {
        let _ = stream.write_all(&socks5_reply(SOCKS5_REP_CMD_NOT_SUPPORTED)).await;
        return None;
    }

    let host = match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await.ok()?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        SOCKS5_ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await.ok()?;
            format!("[{}]", std::net::Ipv6Addr::from(addr))
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.ok()?;
            let mut domain = vec![0u8; len[0] as usize];
            stream.read_exact(&mut domain).await.ok()?;
            String::from_utf8(domain).ok()?
        }
        _ => {
            let _ = stream
                .write_all(&socks5_reply(SOCKS5_REP_ATYP_NOT_SUPPORTED))
                .await;
            return None;
        }
    };

    let mut port_bytes = [0u8; 2];
    stream.read_exact(&mut port_bytes).await.ok()?;
    let port = u16::from_be_bytes(port_bytes);

    Some(format!("{}:{}", host, port))
}

fn split_host_port(addr: &str) -> (String, u16) {
    if let Some((h, p)) = addr.rsplit_once(':') {
        let port = p.parse::<u16>().unwrap_or(80);
        let bare = h.trim_matches(|c| c == '[' || c == ']').to_string();
        (bare, port)
    } else {
        (addr.to_string(), 80)
    }
}

async fn send_502(mut tcp: TcpStream) -> std::io::Result<()> {
    tcp.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
        .await
}

// ── WebTransport message helper ───────────────────────────────────────────────

/// Read a short newline- or EOF-terminated message from a WebTransport recv stream.
async fn read_wt_message(recv: &mut wtransport::RecvStream) -> Option<String> {
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 1];
    loop {
        match recv.read(&mut tmp).await {
            Ok(Some(1)) => {
                if tmp[0] == b'\n' {
                    break;
                }
                buf.push(tmp[0]);
                if buf.len() > 1024 {
                    return None;
                }
            }
            Ok(_) => {
                if buf.is_empty() {
                    return None;
                }
                break;
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
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
    fn bypass_rule() {
        let cfg = config(&[], &["example.com"]);
        assert!(cfg.should_bypass("example.com", 80));
        assert!(!cfg.should_bypass("other.com", 80));
    }

    #[test]
    fn cidr_bypass() {
        use std::str::FromStr;
        let cfg = RoutingConfig {
            proxy: vec![],
            bypass: vec![],
            bypass_geosite: None,
            bypass_cidrs: vec![IpNet::from_str("10.0.0.0/8").unwrap()],
        };
        assert!(cfg.should_bypass("10.1.2.3", 80));
        assert!(!cfg.should_bypass("8.8.8.8", 80));
    }
}
