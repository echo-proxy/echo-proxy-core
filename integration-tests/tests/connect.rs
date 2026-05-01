use integration_tests::{
    connect_via_proxy, spawn_proxy_client, spawn_proxy_server, spawn_raw_echo_upstream,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn connect_tunnel_bidirectional() {
    let upstream_addr = spawn_raw_echo_upstream().await;
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;

    let target = format!("localhost:{}", upstream_addr.port());
    let mut tunnel = connect_via_proxy(proxy_addr, &target).await;

    let payload: Vec<u8> = (0u32..8192)
        .map(|i| (i.wrapping_mul(1664525).wrapping_add(1013904223) & 0xFF) as u8)
        .collect();
    tunnel.write_all(&payload).await.unwrap();

    let mut received = vec![0u8; payload.len()];
    tunnel.read_exact(&mut received).await.unwrap();

    assert_eq!(
        payload, received,
        "echoed bytes must match sent bytes byte-for-byte"
    );
}
