use integration_tests::{
    endpoint_url, insecure_client_config, spawn_proxy_client, spawn_proxy_server,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
/// An unknown user must fail the auth handshake.
#[tokio::test]
async fn unauthorized_user_rejected() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["allowed".into()]).await;
    let endpoint = endpoint_url(server_addr);
    let result = client::connect_and_auth(
        &endpoint,
        "ghost",
        insecure_client_config().build_client_config(),
    )
    .await;
    assert!(result.is_none(), "expected None for unknown user");
}

/// A version string that is not valid semver must be rejected.
#[tokio::test]
async fn invalid_version_rejected() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let endpoint = endpoint_url(server_addr);
    let result = connect_and_auth_test_version(&endpoint, "testuser", "not-semver").await;
    assert!(result.is_none(), "expected None for invalid version");
}

async fn connect_and_auth_test_version(endpoint: &str, user: &str, version: &str) -> Option<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wtransport::Endpoint;
    let config = insecure_client_config().build_client_config();
    let ep = Endpoint::client(config).ok()?;
    let conn = ep.connect(endpoint).await.ok()?;
    let mut ctrl = conn.open_bi().await.ok()?.await.ok()?;

    let msg = format!("Hello:{}:{}\n", version, user);
    ctrl.0.write_all(msg.as_bytes()).await.ok()?;

    let mut buf = vec![0u8; 256];
    let n = ctrl.1.read(&mut buf).await.ok()??;
    let resp = std::str::from_utf8(&buf[..n]).ok()?;
    if resp.starts_with("Hi") {
        Some(())
    } else {
        None
    }
}

/// Requesting a private IP destination must result in HTTP 502.
#[tokio::test]
async fn ssrf_destination_blocked() {
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;

    let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:19999\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let mut response = Vec::new();
    let mut buf = vec![0u8; 512];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("502"),
        "expected 502 Bad Gateway, got: {text}"
    );
}
