use integration_tests::{
    http_get_via_proxy, insecure_client_config, spawn_http_upstream,
};
use std::time::Duration;

/// When no proxy server is reachable, the resilient client must return 502 quickly.
#[tokio::test]
async fn returns_502_when_no_server_available() {
    let upstream_addr = spawn_http_upstream().await;
    let host = format!("localhost:{}", upstream_addr.port());

    // Pick a port that is not listening
    let dead_port = {
        let tmp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tmp.local_addr().unwrap().port()
    };
    // Give OS time to release the port
    tokio::time::sleep(Duration::from_millis(50)).await;

    let dead_endpoint = format!("https://127.0.0.1:{dead_port}/");

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = http_listener.local_addr().unwrap();
    let (_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
    let trust = insecure_client_config();

    tokio::task::spawn(async move {
        client::run_proxy_resilient(
            dead_endpoint,
            "testuser".to_string(),
            trust,
            http_listener,
            None,
            shutdown_rx,
            client::RoutingConfig::default(),
        )
        .await
        .ok();
    });

    // Give the keeper one cycle to confirm the server is unreachable
    tokio::time::sleep(Duration::from_millis(300)).await;

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        http_get_via_proxy(proxy_addr, &host, "/"),
    )
    .await;

    match result {
        Ok((status, _)) => assert_eq!(status, 502, "expected 502 when session is unavailable"),
        Err(_) => panic!("request timed out instead of returning 502"),
    }
}

/// After the proxy server restarts on the same address, the client must reconnect
/// and resume forwarding requests.
#[tokio::test]
async fn recovers_after_server_restart() {
    let upstream_addr = spawn_http_upstream().await;
    let host = format!("localhost:{}", upstream_addr.port());

    // Start first server
    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"])
        .expect("self-signed identity");
    let opts = server::ServerOptions {
        users: vec!["testuser".into()],
        identity,
    };
    let se = server::ServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), opts)
        .expect("bind server");
    let server_addr = se.local_addr();
    let (stop1_tx, stop1_rx) = tokio::sync::broadcast::channel::<()>(1);
    let server_port = server_addr.port();
    let server1_handle = tokio::task::spawn(async move {
        se.serve(stop1_rx).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let endpoint = format!("https://127.0.0.1:{server_port}/");
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = http_listener.local_addr().unwrap();
    let (_tx2, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
    let trust = insecure_client_config();

    tokio::task::spawn(async move {
        client::run_proxy_resilient(
            endpoint,
            "testuser".to_string(),
            trust,
            http_listener,
            None,
            shutdown_rx,
            client::RoutingConfig::default(),
        )
        .await
        .ok();
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    // 1. Confirm initial access works
    let (status, _) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "initial request should succeed");

    // 2. Stop the first server and wait until the UDP port is fully released
    let _ = stop1_tx.send(());
    let _ = server1_handle.await; // wait_idle inside serve() ensures port is freed

    // 3. Restart server on the same port
    let rebind_addr: std::net::SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();
    let se2 = server::ServerEndpoint::bind(
        rebind_addr,
        server::ServerOptions {
            users: vec!["testuser".into()],
            identity: wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).unwrap(),
        },
    )
    .expect("failed to rebind server port after restart");
    let (_stop2_tx, stop2_rx) = tokio::sync::broadcast::channel::<()>(1);
    tokio::task::spawn(async move {
        se2.serve(stop2_rx).await.ok();
    });

    // 4. Wait for client to detect disconnect and reconnect
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 5. Requests must succeed again
    let (status, _) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(
        status, 200,
        "request should succeed after client auto-reconnects"
    );
}
