use integration_tests::{
    http_get_direct, http_get_via_proxy, spawn_http_upstream, spawn_proxy_client,
    spawn_proxy_server,
};

/// A plain `GET /` directly to the server port must return 200 with body `I am running`.
#[async_std::test]
async fn server_root_returns_i_am_running() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (status, body) = http_get_direct(server_addr).await;
    assert_eq!(status, 200, "expected HTTP 200");
    assert_eq!(body, "I am running", "expected body 'I am running', got: {:?}", body);
}

/// Helper: stand up a full server+client+upstream stack.
/// Returns (upstream_host_str, proxy_addr, shutdown_handles).
async fn setup() -> (
    String,                     // "localhost:PORT"
    async_std::net::SocketAddr, // proxy client local port
    Vec<async_std::channel::Sender<()>>,
) {
    let upstream_addr = spawn_http_upstream().await;
    let (server_addr, server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let host = format!("localhost:{}", upstream_addr.port());
    (host, proxy_addr, vec![server_shutdown, client_shutdown])
}

#[async_std::test]
async fn plain_http_get_through_proxy() {
    let (host, proxy_addr, _shutdown) = setup().await;
    let (status, body) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "expected HTTP 200");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "body should contain 'hello-upstream', got: {:?}",
        String::from_utf8_lossy(&body)
    );
}

#[async_std::test]
async fn multiple_concurrent_streams() {
    let (host, proxy_addr, _shutdown) = setup().await;

    let futs: Vec<_> = (0..10)
        .map(|_| {
            let host = host.clone();
            async move { http_get_via_proxy(proxy_addr, &host, "/").await }
        })
        .collect();

    let results = futures::future::join_all(futs).await;
    for (i, (status, body)) in results.iter().enumerate() {
        assert_eq!(*status, 200, "stream {i}: expected 200");
        assert!(
            body.windows(b"hello-upstream".len())
                .any(|w| w == b"hello-upstream"),
            "stream {i}: body missing 'hello-upstream'"
        );
    }
}
