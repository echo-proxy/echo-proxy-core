use integration_tests::{endpoint_url, insecure_client_config, spawn_proxy_server};

/// Auth must succeed for a known user with a valid semver version.
#[tokio::test]
async fn auth_hello_hi_flow() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let endpoint = endpoint_url(server_addr);
    let conn = client::connect_and_auth(
        &endpoint,
        "testuser",
        insecure_client_config().build_client_config(),
    )
    .await;
    assert!(conn.is_some(), "expected successful auth");
}

/// Auth must fail for an unknown user.
#[tokio::test]
async fn auth_unknown_user_returns_bye() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["allowed".into()]).await;
    let endpoint = endpoint_url(server_addr);
    let conn = client::connect_and_auth(
        &endpoint,
        "stranger",
        insecure_client_config().build_client_config(),
    )
    .await;
    assert!(conn.is_none(), "expected auth failure for unknown user");
}

/// Auth must fail when the client sends an invalid version string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_invalid_version_returns_bye() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wtransport::Endpoint;

    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let endpoint = endpoint_url(server_addr);

    let config = insecure_client_config().build_client_config();
    let ep = Endpoint::client(config).unwrap();
    let conn = ep.connect(&endpoint).await.unwrap();
    let mut ctrl = conn.open_bi().await.unwrap().await.unwrap();

    ctrl.0
        .write_all(b"Hello:not-a-semver:testuser\n")
        .await
        .unwrap();

    let mut buf = vec![0u8; 256];
    let n = ctrl.1.read(&mut buf).await.unwrap().unwrap_or(0);
    let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
    assert!(
        resp.starts_with("Bye:"),
        "expected Bye response, got: {resp}"
    );
}

/// A non-Hello first message must cause the server to drop the stream/session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_missing_hello_prefix_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wtransport::Endpoint;

    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let endpoint = endpoint_url(server_addr);

    let config = insecure_client_config().build_client_config();
    let ep = Endpoint::client(config).unwrap();
    let conn = ep.connect(&endpoint).await.unwrap();
    let mut ctrl = conn.open_bi().await.unwrap().await.unwrap();

    ctrl.0.write_all(b"garbage\n").await.unwrap();

    let mut buf = vec![0u8; 256];
    let n = ctrl.1.read(&mut buf).await.unwrap().unwrap_or(0);
    let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
    assert!(
        resp.starts_with("Bye:") || n == 0,
        "expected Bye or empty response, got: {resp}"
    );
}
