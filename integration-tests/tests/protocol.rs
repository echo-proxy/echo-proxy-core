use async_tungstenite::async_std::connect_async;
use async_tungstenite::tungstenite::Message;
use core_lib::Frame;
use futures::StreamExt;
use integration_tests::{endpoint_url, spawn_proxy_server};

/// /control must reject a client that sends an invalid semver string.
#[async_std::test]
async fn control_invalid_version() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["user1".into()]).await;
    let url = format!("{}control", endpoint_url(server_addr));
    let (mut ws, _) = connect_async(url.as_str()).await.unwrap();

    ws.send(Message::Text("Hello:abc:user1".into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    assert!(
        text.starts_with("Bye:Invalid version format"),
        "expected 'Bye:Invalid version format', got: {text}"
    );
}

/// /control must close the connection when the message does not start with "Hello:".
#[async_std::test]
async fn control_missing_hello_prefix() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["user1".into()]).await;
    let url = format!("{}control", endpoint_url(server_addr));
    let (mut ws, _) = connect_async(url.as_str()).await.unwrap();

    ws.send(Message::Text("garbage".into())).await.unwrap();

    // Server should close without sending a proper reply.
    let next = ws.next().await;
    let closed = matches!(next, None | Some(Ok(Message::Close(_))) | Some(Err(_)));
    assert!(closed, "expected connection close, got: {:?}", next);
}

/// The first frame on /mux must be HELLO; any other frame must cause the server
/// to close the connection immediately.
#[async_std::test]
async fn mux_first_frame_must_be_hello() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["user1".into()]).await;

    // First obtain a valid token so /mux accepts the WS upgrade.
    let endpoint = endpoint_url(server_addr);
    let _token = client::obtain_token(&endpoint, "user1")
        .await
        .expect("should get token");

    let mux_url = format!("{}mux", endpoint);
    let (mut ws, _) = connect_async(mux_url.as_str()).await.unwrap();

    // Send OPEN instead of HELLO as the first frame.
    let bad_frame = Frame::Open {
        id: 1,
        dest: "example.com:80".into(),
    };
    ws.send(Message::Binary(bad_frame.encode().into()))
        .await
        .unwrap();

    let next = ws.next().await;
    let closed = matches!(next, None | Some(Ok(Message::Close(_))) | Some(Err(_)));
    assert!(
        closed,
        "server should close connection when first /mux frame is not HELLO"
    );
}
