use async_std::{future::timeout, task};
use integration_tests::{http_get_via_proxy, spawn_http_upstream, spawn_resilient_proxy_client};
use std::time::Duration;

/// When no proxy server is reachable, the resilient client must return 502
/// quickly rather than hanging indefinitely.
#[async_std::test]
async fn returns_502_when_no_server_available() {
    let upstream_addr = spawn_http_upstream().await;
    let host = format!("localhost:{}", upstream_addr.port());

    // Bind to a free port then immediately drop the listener so nothing is
    // reachable on that address.
    let dummy = server::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dummy.local_addr().unwrap();
    drop(dummy);
    task::sleep(Duration::from_millis(100)).await;

    // Start a resilient client pointed at the dead address.
    // spawn_resilient_proxy_client waits 100 ms for the mux to establish;
    // since the server is dead, shared_mux will remain None.
    let (proxy_addr, _client_shutdown) =
        spawn_resilient_proxy_client(dead_addr, "testuser").await;

    // Give the keeper one extra cycle to confirm the server is unreachable.
    task::sleep(Duration::from_millis(300)).await;

    // Request must return 502 quickly, not hang.
    let result = timeout(
        Duration::from_secs(5),
        http_get_via_proxy(proxy_addr, &host, "/"),
    )
    .await;

    match result {
        Ok((status, _)) => assert_eq!(status, 502, "expected 502 when mux is unavailable"),
        Err(_) => panic!("request timed out instead of returning 502"),
    }
}

/// After the proxy server restarts on the same address, the resilient client
/// must reconnect automatically and resume forwarding requests.
#[async_std::test]
async fn recovers_after_server_restart() {
    let upstream_addr = spawn_http_upstream().await;
    let host = format!("localhost:{}", upstream_addr.port());

    // Start the first server and record its port so we can rebind to it.
    let first_listener = server::bind("127.0.0.1:0").await.unwrap();
    let server_port = first_listener.local_addr().unwrap().port();
    let server_addr = first_listener.local_addr().unwrap();
    let (stop1_tx, stop1_rx) = async_std::channel::bounded::<()>(1);
    task::spawn(async move {
        server::serve(
            first_listener,
            server::ServerConfig {
                users: vec!["testuser".into()],
            },
            stop1_rx,
        )
        .await
        .ok();
    });

    let (proxy_addr, _client_shutdown) =
        spawn_resilient_proxy_client(server_addr, "testuser").await;

    // 1. Confirm initial access works.
    let (status, _) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(status, 200, "initial request should succeed");

    // 2. Stop the first server.
    drop(stop1_tx);
    // Give OS time to release the port.
    task::sleep(Duration::from_millis(500)).await;

    // 3. Restart server on the same port.
    let second_listener = server::bind(&format!("127.0.0.1:{}", server_port))
        .await
        .expect("failed to rebind server port after restart");
    let (_stop2_tx, stop2_rx) = async_std::channel::bounded::<()>(1);
    task::spawn(async move {
        server::serve(
            second_listener,
            server::ServerConfig {
                users: vec!["testuser".into()],
            },
            stop2_rx,
        )
        .await
        .ok();
    });

    // 4. Wait for the client to detect the disconnect and reconnect.
    task::sleep(Duration::from_secs(3)).await;

    // 5. Requests must succeed again without restarting the client.
    let (status, _) = http_get_via_proxy(proxy_addr, &host, "/").await;
    assert_eq!(
        status, 200,
        "request should succeed after client auto-reconnects"
    );
}
