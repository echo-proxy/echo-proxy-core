use integration_tests::{
    http_get_direct, http_get_via_proxy, spawn_http_upstream, spawn_proxy_client,
    spawn_proxy_client_with_routing, spawn_proxy_server,
};

/// A plain `GET /` directly to a standard HTTP server must return 200.
#[tokio::test]
async fn server_root_returns_200() {
    let upstream_addr = spawn_http_upstream().await;
    let host = format!("127.0.0.1:{}", upstream_addr.port());
    let (status, body) = http_get_direct(upstream_addr).await;
    assert_eq!(status, 200, "expected HTTP 200");
    assert!(
        body.contains("hello-upstream"),
        "expected 'hello-upstream', got: {body:?}"
    );
    let _ = host;
}

async fn setup() -> (
    String,
    std::net::SocketAddr,
    Vec<tokio::sync::broadcast::Sender<()>>,
) {
    let upstream_addr = spawn_http_upstream().await;
    let (server_addr, server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let host = format!("localhost:{}", upstream_addr.port());
    (host, proxy_addr, vec![server_shutdown, client_shutdown])
}

#[tokio::test]
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

#[tokio::test]
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

/// A request to a host in the bypass list should be served by direct connection.
#[tokio::test]
async fn bypass_rule_connects_directly() {
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
    let (proxy_addr, _client_shutdown) =
        spawn_proxy_client_with_routing(server_addr, "testuser", routing).await;

    let (status, body) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "expected 200 from direct bypass path");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "direct bypass body missing 'hello-upstream'"
    );
}

/// A CIDR bypass rule must direct-connect when the target IP is in range.
#[tokio::test]
async fn cidr_bypass_connects_directly() {
    use client::RoutingConfig;
    use ipnet::IpNet;
    use std::str::FromStr;

    let upstream_addr = spawn_http_upstream().await;
    let host = format!("127.0.0.1:{}", upstream_addr.port());

    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;

    let routing = RoutingConfig {
        proxy: vec![],
        bypass: vec![],
        bypass_geosite: None,
        bypass_cidrs: vec![IpNet::from_str("127.0.0.0/8").unwrap()],
    };
    let (proxy_addr, _client_shutdown) =
        spawn_proxy_client_with_routing(server_addr, "testuser", routing).await;

    let (status, body) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "expected 200 from CIDR bypass path");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "CIDR bypass body missing 'hello-upstream'"
    );
}

/// A proxy rule must override an overlapping bypass rule.
#[tokio::test]
async fn proxy_rule_overrides_bypass() {
    use client::{HostPattern, RoutingConfig};

    let upstream_addr = spawn_http_upstream().await;
    let host = format!("localhost:{}", upstream_addr.port());

    let (server_addr, server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;

    let routing = RoutingConfig {
        proxy: vec![HostPattern::parse(&host)],
        bypass: vec![HostPattern::parse(&host)],
        bypass_geosite: None,
        bypass_cidrs: vec![],
    };
    let (proxy_addr, _client_shutdown) =
        spawn_proxy_client_with_routing(server_addr, "testuser", routing).await;

    let (status, body) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "proxy-rule override: expected 200 via tunnel");
    assert!(
        body.windows(b"hello-upstream".len())
            .any(|w| w == b"hello-upstream"),
        "proxy-rule override: body missing 'hello-upstream'"
    );
    drop(server_shutdown);
}
