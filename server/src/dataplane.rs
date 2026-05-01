//! Protocol and relay logic independent of HTTP/3 / WebTransport transport types.

use std::net::IpAddr;

use semver::{Version, VersionReq};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task,
};

pub const SUPPORTED_VERSION: &str = ">=0.1.0";

/// Read a single short message (up to 1 KiB) terminated by `\n` or stream EOF.
pub async fn read_line_message(recv: &mut (impl AsyncReadExt + Unpin)) -> Option<String> {
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0u8; 1];
    loop {
        match recv.read(&mut tmp).await {
            Ok(1) => {
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

#[derive(Debug)]
pub enum AuthReply {
    Ok,
    RejectLine(&'static [u8]),
}

/// Validate `Hello:<version>:<user>` and allowed user list.
pub fn check_auth_handshake(auth_line: &str, users: &[String]) -> AuthReply {
    if !auth_line.starts_with("Hello:") {
        return AuthReply::RejectLine(b"Bye:Invalid auth message\n");
    }
    let parts: Vec<&str> = auth_line.splitn(3, ':').collect();
    if parts.len() < 3 {
        return AuthReply::RejectLine(b"Bye:Malformed auth message\n");
    }
    let client_version = parts[1];
    let client_user = parts[2];

    let installed = match Version::parse(client_version) {
        Ok(v) => v,
        Err(_) => return AuthReply::RejectLine(b"Bye:Invalid version format\n"),
    };

    let required = VersionReq::parse(SUPPORTED_VERSION).unwrap();
    if !required.matches(&installed) {
        return AuthReply::RejectLine(
            b"Bye:Unsupported version. Download from https://github.com/echo-proxy/echo-proxy-core/releases\n",
        );
    }

    if !users.contains(&client_user.to_string()) {
        return AuthReply::RejectLine(b"Bye:User not allowed\n");
    }

    AuthReply::Ok
}

pub async fn run_auth<W, R>(mut ctrl_send: W, mut ctrl_recv: R, users: &[String])
where
    W: AsyncWriteExt + Unpin,
    R: AsyncReadExt + Unpin,
{
    let msg = match read_line_message(&mut ctrl_recv).await {
        Some(m) => m,
        None => return,
    };

    match check_auth_handshake(&msg, users) {
        AuthReply::Ok => {
            let user = msg.splitn(3, ':').nth(2).unwrap_or("?");
            tracing::info!(user, "auth ok");
            let _ = ctrl_send.write_all(b"Hi").await;
        }
        AuthReply::RejectLine(line) => {
            let _ = ctrl_send.write_all(line).await;
        }
    }
}

pub async fn handle_proxy_stream<W, RR>(mut wt_send: W, mut wt_recv: RR)
where
    W: AsyncWriteExt + Send + Unpin + 'static,
    RR: AsyncReadExt + Send + Unpin + 'static,
{
    let dest = match read_line_message(&mut wt_recv).await {
        Some(d) => d,
        None => return,
    };

    if !is_dest_allowed(&dest) {
        tracing::warn!(dest = %dest, "SSRF: destination blocked");
        let _ = wt_send.write_all(&[0x01]).await;
        return;
    }

    match TcpStream::connect(&dest).await {
        Ok(tcp) => {
            let _ = wt_send.write_all(&[0x00]).await;
            tracing::debug!(dest = %dest, "proxy stream opened");
            relay_tcp_bidir(wt_send, wt_recv, tcp).await;
        }
        Err(e) => {
            tracing::warn!(dest = %dest, "connect failed: {e}");
            let _ = wt_send.write_all(&[0x01]).await;
        }
    }
}

pub async fn relay_tcp_bidir<W, RR>(mut wt_send: W, mut wt_recv: RR, tcp: TcpStream)
where
    W: AsyncWriteExt + Unpin + Send + 'static,
    RR: AsyncReadExt + Unpin + Send + 'static,
{
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
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tcp_send.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    drop(tcp_to_wt);
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
