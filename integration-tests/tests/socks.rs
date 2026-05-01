use integration_tests::{
    connect_via_socks5, http_get_via_proxy, spawn_http_upstream, spawn_proxy_client_with_socks,
    spawn_proxy_server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_http_response(stream: &mut tokio::net::TcpStream) -> (u16, Vec<u8>) {
    use tokio::io::AsyncBufReadExt;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut status: u16 = 0;
    let mut content_length: usize = 0;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" {
            break;
        }
        if status == 0 {
            if let Some(code) = line.split_whitespace().nth(1) {
                status = code.parse().unwrap_or(0);
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
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.unwrap();
    }
    (status, body)
}

async fn setup() -> (
    String,
    std::net::SocketAddr,
    std::net::SocketAddr,
    Vec<tokio::sync::broadcast::Sender<()>>,
) {
    let upstream_addr = spawn_http_upstream().await;
    let (server_addr, server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let ((http_addr, socks_addr), client_shutdown) =
        spawn_proxy_client_with_socks(server_addr, "testuser", client::RoutingConfig::default())
            .await;
    let host = format!("localhost:{}", upstream_addr.port());
    (
        host,
        http_addr,
        socks_addr,
        vec![server_shutdown, client_shutdown],
    )
}

/// SOCKS5 CONNECT via tunnel must successfully forward to the upstream.
#[tokio::test]
async fn socks5_connect_via_tunnel() {
    let (host, _http_addr, socks_addr, _shutdown) = setup().await;

    let mut stream = connect_via_socks5(socks_addr, &host).await;

    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host
    );
    stream.write_all(req.as_bytes()).await.unwrap();

    let (status, body) = read_http_response(&mut stream).await;
    assert_eq!(status, 200, "expected 200, got {status}");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "expected 'hello-upstream' in response body"
    );
}

/// SOCKS5 CONNECT to a bypassed host should direct-connect.
#[tokio::test]
async fn socks5_connect_direct_bypass() {
    use client::{HostPattern, RoutingConfig};

    let upstream_addr = spawn_http_upstream().await;
    let host = format!("127.0.0.1:{}", upstream_addr.port());

    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;

    let routing = RoutingConfig {
        proxy: vec![],
        bypass: vec![HostPattern::parse(&host)],
        bypass_geosite: None,
        bypass_cidrs: vec![],
    };
    let ((_http_addr, socks_addr), _client_shutdown) =
        spawn_proxy_client_with_socks(server_addr, "testuser", routing).await;

    let mut stream = connect_via_socks5(socks_addr, &host).await;

    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host
    );
    stream.write_all(req.as_bytes()).await.unwrap();

    let (status, body) = read_http_response(&mut stream).await;
    assert_eq!(status, 200, "direct bypass: expected 200, got {status}");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "direct bypass body missing 'hello-upstream'"
    );
}

/// HTTP proxy and SOCKS5 proxy share the same tunnel: both must work concurrently.
#[tokio::test]
async fn http_and_socks5_share_tunnel() {
    let (host, http_addr, socks_addr, _shutdown) = setup().await;

    let (status, body) = http_get_via_proxy(http_addr, &host, "/").await;
    assert_eq!(status, 200, "http proxy: expected 200");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "http proxy body missing 'hello-upstream'"
    );

    let mut socks_stream = connect_via_socks5(socks_addr, &host).await;
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host
    );
    socks_stream.write_all(req.as_bytes()).await.unwrap();

    let (status2, body2) = read_http_response(&mut socks_stream).await;
    assert_eq!(status2, 200, "socks5: expected 200, got {status2}");
    assert!(
        body2
            .windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "socks5 body missing 'hello-upstream'"
    );
}

/// Server must reject an unsupported SOCKS5 CMD (BIND = 2).
#[tokio::test]
async fn socks5_unsupported_command_rejected() {
    let (host, _http_addr, socks_addr, _shutdown) = setup().await;
    let (target_host, port_str) = host.rsplit_once(':').unwrap();
    let port: u16 = port_str.parse().unwrap();

    let mut stream = tokio::net::TcpStream::connect(socks_addr).await.unwrap();

    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut sel = [0u8; 2];
    stream.read_exact(&mut sel).await.unwrap();
    assert_eq!(sel[1], 0x00);

    let domain = target_host.as_bytes();
    let mut req = vec![0x05, 0x02, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await.unwrap();

    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        reply[1], 0x07,
        "expected CMD_NOT_SUPPORTED(0x07), got {}",
        reply[1]
    );
}
