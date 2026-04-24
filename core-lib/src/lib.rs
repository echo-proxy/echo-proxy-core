use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use base64::prelude::*;
use rand::Rng;

// ── Protocol constants ────────────────────────────────────────────────────────

pub mod protocol {
    pub const HELLO_PREFIX: &str = "Hello:";
    pub const HI_PREFIX: &str = "Hi:";
    pub const BYE_PREFIX: &str = "Bye:";
    pub const CONTROL_PATH: &str = "control";
    pub const HEADER_TOKEN: &str = "x-token";
    pub const QUERY_KEY: &str = "q";
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ProxyError {
    DecodeBase64(base64::DecodeError),
    DecodeUtf8(std::string::FromUtf8Error),
    BadProtocol(String),
    Forbidden(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::DecodeBase64(e) => write!(f, "base64 decode error: {}", e),
            ProxyError::DecodeUtf8(e) => write!(f, "utf8 decode error: {}", e),
            ProxyError::BadProtocol(s) => write!(f, "bad protocol: {}", s),
            ProxyError::Forbidden(s) => write!(f, "forbidden: {}", s),
            ProxyError::Io(e) => write!(f, "io error: {}", e),
        }
    }
}

impl std::error::Error for ProxyError {}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        ProxyError::Io(e)
    }
}

// ── Host encoding / decoding ──────────────────────────────────────────────────

fn generate_random_character() -> char {
    const CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let idx = rng.random_range(0..CHARS.len());
    CHARS.chars().nth(idx).unwrap()
}

pub fn encode_host(host: &str) -> String {
    let mut encoded = BASE64_STANDARD.encode(host.as_bytes());
    let idx = (encoded.len() as f64 * 0.618).floor() as usize;
    encoded.insert(idx, generate_random_character());
    encoded
}

pub fn decode_host(encoded_host: &str) -> Result<String, ProxyError> {
    let mut s = encoded_host.to_string();
    if s.is_empty() {
        return Err(ProxyError::BadProtocol("empty host".into()));
    }
    let i = ((s.len() - 1) as f64 * 0.618).floor() as usize;
    s.remove(i);
    let bytes = BASE64_STANDARD
        .decode(s.as_bytes())
        .map_err(ProxyError::DecodeBase64)?;
    String::from_utf8(bytes).map_err(ProxyError::DecodeUtf8)
}

// ── SSRF blacklist ────────────────────────────────────────────────────────────

/// Returns true if the given address should be blocked to prevent SSRF.
/// Blocks: loopback, private, link-local, multicast, unspecified, broadcast,
/// and the cloud metadata IP 169.254.169.254.
pub fn is_blocked_addr(addr: &SocketAddr) -> bool {
    is_blocked_ip(addr.ip())
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_broadcast()
        // Cloud metadata endpoint
        || ip == Ipv4Addr::new(169, 254, 169, 254)
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unspecified()
        // Unique-local (fc00::/7)
        || matches!(ip.segments()[0], 0xfc00..=0xfdff)
        // Link-local (fe80::/10)
        || (ip.segments()[0] & 0xffc0) == 0xfe80
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        for host in ["example.com:443", "192.0.2.1:80", "a:1"] {
            let encoded = encode_host(host);
            let decoded = decode_host(&encoded).unwrap();
            assert_eq!(decoded, host);
        }
    }

    #[test]
    fn decode_invalid_base64_returns_err() {
        assert!(decode_host("!!!invalid!!!").is_err());
    }

    #[test]
    fn blocked_loopback() {
        let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        assert!(is_blocked_addr(&addr));
    }

    #[test]
    fn blocked_private() {
        let addr: SocketAddr = "10.0.0.1:80".parse().unwrap();
        assert!(is_blocked_addr(&addr));
        let addr2: SocketAddr = "192.168.1.1:80".parse().unwrap();
        assert!(is_blocked_addr(&addr2));
    }

    #[test]
    fn blocked_metadata() {
        let addr: SocketAddr = "169.254.169.254:80".parse().unwrap();
        assert!(is_blocked_addr(&addr));
    }

    #[test]
    fn allowed_public() {
        let addr: SocketAddr = "8.8.8.8:53".parse().unwrap();
        assert!(!is_blocked_addr(&addr));
    }
}
