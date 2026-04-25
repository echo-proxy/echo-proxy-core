use async_std::net::TcpStream;
use futures::{AsyncReadExt, AsyncWriteExt};
use integration_tests::{
    connect_via_proxy, http_get_via_proxy, spawn_http_upstream, spawn_proxy_client,
    spawn_proxy_server, spawn_raw_echo_upstream, spawn_upstream_close_immediately,
};

// ── HTTP concurrency ──────────────────────────────────────────────────────────

/// 100 concurrent HTTP GET requests through a single mux WebSocket connection.
/// All must succeed with status 200 and the expected body.
#[async_std::test]
async fn concurrent_100_http_streams() {
    let upstream_addr = spawn_http_upstream().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let host = format!("localhost:{}", upstream_addr.port());

    let futs: Vec<_> = (0..100)
        .map(|_| {
            let host = host.clone();
            async move { http_get_via_proxy(proxy_addr, &host, "/").await }
        })
        .collect();

    let results = futures::future::join_all(futs).await;

    let failures: Vec<_> = results
        .iter()
        .enumerate()
        .filter(|(_, (s, _))| *s != 200)
        .collect();
    assert!(
        failures.is_empty(),
        "{}/{} streams returned non-200: {:?}",
        failures.len(),
        results.len(),
        failures
            .iter()
            .map(|(i, (s, _))| format!("#{i}={s}"))
            .collect::<Vec<_>>()
    );

    for (i, (_, body)) in results.iter().enumerate() {
        assert!(
            body.windows(b"hello-upstream".len())
                .any(|w| w == b"hello-upstream"),
            "stream {i}: body missing 'hello-upstream'"
        );
    }
}

// ── CONNECT tunnel concurrency ────────────────────────────────────────────────

/// Open 20 concurrent CONNECT tunnels, each with a unique 1 KiB payload.
/// Every tunnel must echo back the exact bytes that were sent.
#[async_std::test]
async fn concurrent_connect_tunnels() {
    const N: usize = 20;
    let upstream_addr = spawn_raw_echo_upstream().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let target = format!("localhost:{}", upstream_addr.port());

    let futs: Vec<_> = (0..N as u32)
        .map(|i| {
            let target = target.clone();
            async move {
                let mut tunnel = connect_via_proxy(proxy_addr, &target).await;
                // Each tunnel gets a distinct 1 KiB payload (same LCG as connect.rs).
                let payload: Vec<u8> = (0..1024_u32)
                    .map(|j| {
                        ((i.wrapping_mul(31).wrapping_add(j))
                            .wrapping_mul(1664525)
                            .wrapping_add(1013904223)
                            & 0xFF) as u8
                    })
                    .collect();
                tunnel.write_all(&payload).await.unwrap();
                let mut received = vec![0u8; payload.len()];
                tunnel.read_exact(&mut received).await.unwrap();
                (payload, received)
            }
        })
        .collect();

    let results = futures::future::join_all(futs).await;
    for (i, (sent, received)) in results.iter().enumerate() {
        assert_eq!(sent, received, "tunnel {i}: echo mismatch");
    }
}

// ── Large payload ─────────────────────────────────────────────────────────────

/// Send 4 MiB through a CONNECT tunnel in a single write_all and verify
/// byte-for-byte echo via read_exact.
///
/// This is a regression test for the StreamRegistry::route try_send bug:
/// before the fix, route used try_send which silently dropped frames when the
/// per-stream channel was full (32 × 8 KiB = 256 KiB), causing read_exact to
/// hang indefinitely.  With the fix (send().await), back-pressure propagates
/// correctly and the full 4 MiB round-trips intact.
///
/// Write and read run concurrently to avoid the OS-level TCP buffer deadlock
/// that would occur with a sequential write-then-read: both halves share the
/// same socket so concurrent I/O is required for full-duplex echo.
#[async_std::test]
async fn large_payload_via_connect() {
    let upstream_addr = spawn_raw_echo_upstream().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let target = format!("localhost:{}", upstream_addr.port());

    let tunnel = connect_via_proxy(proxy_addr, &target).await;

    const SIZE: usize = 4 * 1024 * 1024;
    let payload = std::sync::Arc::new(
        (0..SIZE as u32)
            .map(|i| (i.wrapping_mul(1664525).wrapping_add(1013904223) & 0xFF) as u8)
            .collect::<Vec<u8>>(),
    );

    // Spawn writer on a clone so read and write run concurrently.
    let p = payload.clone();
    let write_task = async_std::task::spawn({
        let mut writer = tunnel.clone();
        async move {
            writer.write_all(&p).await.unwrap();
        }
    });

    let mut received = vec![0u8; SIZE];
    let mut reader = tunnel;
    reader.read_exact(&mut received).await.unwrap();
    write_task.await;

    assert_eq!(*payload, received, "4 MiB CONNECT echo: byte mismatch");
}

// ── Short-connection churn ────────────────────────────────────────────────────

/// 200 sequential short-lived HTTP connections.
/// After each connection closes, the mux stream ID is retired and the
/// StreamRegistry entry must be cleaned up.  No failures expected.
#[async_std::test]
async fn short_connection_churn() {
    const ROUNDS: usize = 200;
    let upstream_addr = spawn_http_upstream().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let host = format!("localhost:{}", upstream_addr.port());

    for i in 0..ROUNDS {
        let (status, _) = http_get_via_proxy(proxy_addr, &host, "/").await;
        assert_eq!(status, 200, "churn round {i}: expected 200");
    }
}

// ── Upstream closes immediately ───────────────────────────────────────────────

/// Upstream drops the TCP connection right after accept.
///
/// Current proxy behaviour: when upstream closes with no response, the proxy
/// does not close the local TCP connection (it waits for the local client to
/// close first).  To avoid hanging, we send an HTTP request and read with a
/// short timeout — either outcome (response or timeout) is acceptable.  The
/// critical assertion is that the proxy is still alive afterwards.
#[async_std::test]
async fn upstream_closes_immediately() {
    use async_std::future::timeout;
    use std::time::Duration;

    let upstream_addr = spawn_upstream_close_immediately().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;
    let host = format!("localhost:{}", upstream_addr.port());

    // Issue the request with a timeout; timeout is expected because the proxy
    // waits for the local client to EOF before cleaning up the handler task.
    let _result = timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            host
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 512];
        let _ = stream.read(&mut buf).await;
    })
    .await;

    // Proxy must still accept and serve new connections.
    let healthy_addr = spawn_http_upstream().await;
    let healthy_host = format!("localhost:{}", healthy_addr.port());
    let (status, _) = http_get_via_proxy(proxy_addr, &healthy_host, "/").await;
    assert_eq!(
        status, 200,
        "proxy should still be operational after upstream closed immediately"
    );
}
