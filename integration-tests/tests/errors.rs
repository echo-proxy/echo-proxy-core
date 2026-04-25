use async_std::net::TcpStream;
use async_tungstenite::async_std::connect_async;
use async_tungstenite::tungstenite::Message;
use core_lib::Frame;
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use integration_tests::{endpoint_url, spawn_proxy_client, spawn_proxy_server};

/// An unknown user must get `None` back from `obtain_token`.
#[async_std::test]
async fn unauthorized_user_rejected() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["allowed".into()]).await;
    let endpoint = endpoint_url(server_addr);
    let result = client::obtain_token(&endpoint, "ghost").await;
    assert!(result.is_none(), "expected None for unknown user");
}

/// Requesting a private IP as destination must receive HTTP 502 from the proxy.
#[async_std::test]
async fn ssrf_destination_blocked() {
    // Use a port that is almost certainly not listening so the test does not
    // accidentally succeed if something does happen to be on that port.
    let (server_addr, _server_shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let (proxy_addr, _client_shutdown) = spawn_proxy_client(server_addr, "testuser").await;

    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    // Host is a loopback IP; is_dest_allowed will reject it.
    let request = "GET / HTTP/1.1\r\nHost: 127.0.0.1:19999\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();

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
        "expected 502 Bad Gateway, got: {}",
        text
    );
}

/// A session token issued by /control must be rejected on a second /mux use.
#[async_std::test]
async fn token_is_single_use() {
    let (server_addr, _shutdown) = spawn_proxy_server(vec!["testuser".into()]).await;
    let endpoint = endpoint_url(server_addr);

    let token = client::obtain_token(&endpoint, "testuser")
        .await
        .expect("should get a token");

    // First use — must succeed (server removes the token).
    {
        let url = format!("{}mux", endpoint);
        let (mut ws, _) = connect_async(url.as_str()).await.unwrap();
        ws.send(Message::Binary(Frame::Hello(token.clone()).encode().into()))
            .await
            .unwrap();
        // Give the server a moment to consume the token.
        async_std::task::sleep(std::time::Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }

    // Second use — server must reject (token already consumed).
    {
        let url = format!("{}mux", endpoint);
        let (mut ws, _) = connect_async(url.as_str()).await.unwrap();
        ws.send(Message::Binary(Frame::Hello(token).encode().into()))
            .await
            .unwrap();
        // Server should close the connection promptly.
        let next = ws.next().await;
        // Either None (closed) or a Close frame is acceptable.
        let rejected = matches!(next, None | Some(Ok(Message::Close(_))) | Some(Err(_)));
        assert!(rejected, "server should have rejected duplicate token");
    }
}
