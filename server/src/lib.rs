use semver::{Version, VersionReq};
use std::{net::IpAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    task,
};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, endpoint::endpoint_side::Server};

const SUPPORTED_VERSION: &str = ">=0.1.0";

pub struct ServerOptions {
    pub users: Vec<String>,
    pub identity: Identity,
}

/// A bound WebTransport server endpoint.
///
/// Use [`ServerEndpoint::bind`] to create one, inspect [`ServerEndpoint::local_addr`]
/// for the actual bound address (useful when binding to port 0), then call
/// [`ServerEndpoint::serve`] to start accepting sessions.
pub struct ServerEndpoint {
    endpoint: Endpoint<Server>,
    users: Arc<Vec<String>>,
}

impl ServerEndpoint {
    pub fn bind(addr: std::net::SocketAddr, opts: ServerOptions) -> std::io::Result<Self> {
        let config = ServerConfig::builder()
            .with_bind_address(addr)
            .with_identity(opts.identity)
            .build();
        let endpoint = Endpoint::server(config).map_err(std::io::Error::other)?;
        Ok(Self {
            endpoint,
            users: Arc::new(opts.users),
        })
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.endpoint.local_addr().expect("local_addr")
    }

    pub async fn serve(self, mut shutdown: broadcast::Receiver<()>) -> std::io::Result<()> {
        tracing::info!(addr = %self.local_addr(), "WebTransport server listening");
        loop {
            tokio::select! {
                incoming = self.endpoint.accept() => {
                    let users = self.users.clone();
                    task::spawn(handle_incoming_session(incoming, users));
                }
                _ = shutdown.recv() => break,
            }
        }
        // Close the endpoint cleanly so the UDP port is released promptly.
        self.endpoint.close(0u32.into(), b"shutdown");
        self.endpoint.wait_idle().await;
        Ok(())
    }
}

pub async fn serve(
    addr: std::net::SocketAddr,
    opts: ServerOptions,
    shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    ServerEndpoint::bind(addr, opts)?.serve(shutdown).await
}

async fn handle_incoming_session(
    incoming: wtransport::endpoint::IncomingSession,
    users: Arc<Vec<String>>,
) {
    let session_request = match incoming.await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("session accept error: {e}");
            return;
        }
    };

    tracing::debug!(
        authority = %session_request.authority(),
        path = %session_request.path(),
        "incoming session"
    );

    let connection = match session_request.accept().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("session accept error: {e}");
            return;
        }
    };

    // First bidirectional stream is the auth control stream.
    let mut ctrl = match connection.accept_bi().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("accept control stream error: {e}");
            return;
        }
    };

    let msg = match read_message(&mut ctrl.1).await {
        Some(m) => m,
        None => return,
    };

    if !msg.starts_with("Hello:") {
        let _ = ctrl.0.write_all(b"Bye:Invalid auth message").await;
        return;
    }

    let parts: Vec<&str> = msg.splitn(3, ':').collect();
    if parts.len() < 3 {
        let _ = ctrl.0.write_all(b"Bye:Malformed auth message").await;
        return;
    }

    let client_version = parts[1];
    let client_user = parts[2];

    let installed = match Version::parse(client_version) {
        Ok(v) => v,
        Err(_) => {
            let _ = ctrl.0.write_all(b"Bye:Invalid version format").await;
            return;
        }
    };

    let required = VersionReq::parse(SUPPORTED_VERSION).unwrap();
    if !required.matches(&installed) {
        tracing::warn!(user = client_user, version = client_version, "unsupported version");
        let _ = ctrl
            .0
            .write_all(
                b"Bye:Unsupported version. Download from https://github.com/echo-proxy/echo-proxy-core/releases",
            )
            .await;
        return;
    }

    if !users.contains(&client_user.to_string()) {
        tracing::warn!(user = client_user, "user not allowed");
        let _ = ctrl.0.write_all(b"Bye:User not allowed").await;
        return;
    }

    tracing::info!(user = client_user, "auth ok");
    let _ = ctrl.0.write_all(b"Hi").await;
    drop(ctrl);

    run_session(connection).await;
}

async fn run_session(connection: Connection) {
    loop {
        let stream = match connection.accept_bi().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("session ended: {e}");
                break;
            }
        };
        task::spawn(handle_proxy_stream(stream.0, stream.1));
    }
}

async fn handle_proxy_stream(
    mut send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
) {
    let dest = match read_message(&mut recv).await {
        Some(d) => d,
        None => return,
    };

    if !is_dest_allowed(&dest) {
        tracing::warn!(dest = %dest, "SSRF: destination blocked");
        let _ = send.write_all(&[0x01]).await;
        return;
    }

    match TcpStream::connect(&dest).await {
        Ok(tcp) => {
            let _ = send.write_all(&[0x00]).await;
            tracing::debug!(dest = %dest, "proxy stream opened");
            relay_wt_tcp(send, recv, tcp).await;
        }
        Err(e) => {
            tracing::warn!(dest = %dest, "connect failed: {e}");
            let _ = send.write_all(&[0x01]).await;
        }
    }
}

async fn relay_wt_tcp(
    mut wt_send: wtransport::SendStream,
    mut wt_recv: wtransport::RecvStream,
    tcp: TcpStream,
) {
    let (mut tcp_recv, mut tcp_send) = tcp.into_split();

    let tcp_to_wt = task::spawn(async move {
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
    });

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
    drop(tcp_to_wt);
}

/// Read a single short message (up to 1 KiB) terminated by `\n` or stream EOF.
async fn read_message(recv: &mut wtransport::RecvStream) -> Option<String> {
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
                // EOF — use whatever we have so far
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

/// Returns `false` for IP-literal addresses in private/loopback/link-local ranges.
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

pub fn make_shutdown_channel() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
    broadcast::channel(1)
}
