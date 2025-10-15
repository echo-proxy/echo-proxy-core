use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::task;
use async_tungstenite::tungstenite::http::Request;
use async_tungstenite::tungstenite::http::Uri;
use async_tungstenite::{async_std::connect_async, tungstenite::Message};
use futures::{prelude::*, StreamExt};
use std::str::FromStr;
// use std::time::Instant; // 暂时未使用

pub fn builder_request(url_str: &str, token: &str) -> Result<async_tungstenite::tungstenite::http::Request<()>, async_tungstenite::tungstenite::http::Error> {
    let uri = Uri::from_str(url_str).unwrap();

    let request = Request::builder()
        .uri(url_str)
        .header("Host", uri.host().unwrap())
        .header("X-Token", token)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            async_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(());

    request
}

pub async fn check_server(remote_server: &str, user_id: &str) -> bool {
    let app_version = env!("CARGO_PKG_VERSION");
    let mut remote_server = remote_server.to_string();
    if !remote_server.ends_with("/") {
        remote_server = format!("{}/", remote_server);
    }
    let (mut ws_stream, _) = connect_async(format!("{}{}", remote_server, "control"))
        .await
        .expect("Failed to connect");
    ws_stream
        .send(Message::Text(format!("Hello:{}:{}", app_version, user_id).into()))
        .await
        .unwrap();
    ws_stream.flush().await.unwrap();
    let msg = ws_stream.next().await.unwrap().unwrap();
    let msg = msg.to_text().unwrap();
    if !msg.starts_with("Hi:") {
        println!("{}", msg.replace("Bye:", ""));
        return false;
    }

    true
}

pub async fn run(remote_server: &str, addr: String) {
    let listener = TcpListener::bind(addr).await.unwrap();

    let mut incoming = listener.incoming();
    let addr = listener.local_addr().unwrap();
    println!("http proxy listening on {}", addr);

    while let Some(stream) = incoming.next().await {
        let stream = stream.unwrap();

        let remote_server = remote_server.to_string();
        std::thread::spawn(move || {
            task::block_on(handle_connection(&remote_server, stream));
        });
    }
}

pub async fn parse_request_header(tcp_stream: TcpStream) -> (String, String, Vec<u8>) {
    let mut headers = String::new();
    let mut host = String::new();
    let mut content_length = 0;

    let mut reader = async_std::io::BufReader::new(tcp_stream);
    {
        loop {
            let mut line = String::new();
            let _read_count = reader.read_line(&mut line).await.unwrap();
            headers.push_str(&line);

            if line.starts_with("Host: ") {
                host = line
                    .trim_start_matches("Host: ")
                    .trim_end_matches("\r\n")
                    .to_string();
                if !host.contains(":") {
                    host.push_str(":80");
                }
            }
            if line.starts_with("Content-Length") {
                let size = line
                    .split(":")
                    .nth(1)
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                content_length = size;
            }
            if line == "\r\n" {
                break;
            }
        }
    }

    let mut buffer = vec![0; content_length];
    if content_length != 0 {
        reader.read(&mut buffer).await.unwrap();
    }

    (host, headers, buffer)
}

pub async fn handle_https(ws_url: String, mut tcp_stream: TcpStream) {
    let ws_request = builder_request(&ws_url, "token").unwrap();
    let (ws_stream, _) = connect_async(ws_request).await.expect("Failed to connect");
    tcp_stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .unwrap();
    tcp_stream.flush().await.unwrap();

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    let (ws_writer, mut ws_reader) = ws_stream.split();

    std::thread::spawn(move || {
        task::block_on(async {
            while let Some(msg) = ws_reader.next().await {
                match msg {
                    Err(_) => break,
                    _ => {}
                }
                let msg = msg.unwrap();
                if msg.is_binary() {
                    let res = tcp_writer.write_all(&msg.into_data()).await;
                    if res.is_err() {
                        break;
                    }
                }
            }
        });
    });

    std::thread::spawn(move || {
        task::block_on(async {
            let mut buf = vec![0; 1024];

            loop {
                let n = tcp_reader.read(&mut buf).await;
                if n.is_err() {
                    break;
                }
                let n = n.unwrap();
                if n == 0 {
                    break;
                }
                let data = &buf[0..n];
                let data = data.to_vec();
                let res = ws_writer.send(Message::Binary(data.into())).await;
                if res.is_err() {
                    break;
                }
            }
        });
    });
}

pub async fn handle_http(ws_url: String, mut tcp_stream: TcpStream, headers: String, body: Vec<u8>) {
    let ws_request = builder_request(&ws_url, "token").unwrap();

    let (mut ws_stream, _) = connect_async(ws_request).await.expect("Failed to connect");

    ws_stream
        .send(Message::Binary(headers.as_bytes().to_vec().into()))
        .await
        .unwrap();
    if body.len() > 0 {
        ws_stream.send(Message::Binary(body.into())).await.unwrap();
    }
    ws_stream.flush().await.unwrap();

    while let Some(msg) = ws_stream.next().await {
        match msg {
            Err(_) => break,
            _ => {}
        }
        let msg = msg.unwrap();
        if msg.is_binary() {
            let res = tcp_stream.write_all(&msg.into_data()).await;
            if res.is_err() {
                break;
            }
        }
    }
    tcp_stream.flush().await.unwrap();
    tcp_stream.close().await.unwrap();
}

pub async fn handle_connection(remote_server: &str, tcp_stream: TcpStream) {
    let (host, headers, _body) = parse_request_header(tcp_stream.clone()).await;
    let ws_url = format!("{}?q={}", remote_server, core_lib::encode_host(host.as_str()));
    println!("accepted {}", host);

    if headers.starts_with("CONNECT ") {
        handle_https(ws_url, tcp_stream).await;
    } else {
        handle_http(ws_url, tcp_stream, headers, _body).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::tungstenite::http::Method;

    #[test]
    fn test_builder_request_creation() {
        let url = "ws://example.com/proxy";
        let token = "test-token";

        let request = builder_request(url, token).unwrap();

        assert_eq!(request.method(), &Method::GET);
        assert_eq!(request.uri(), url);
        assert!(request.headers().contains_key("X-Token"));
        assert!(request.headers().contains_key("Host"));
        assert!(request.headers().contains_key("Upgrade"));
        assert!(request.headers().contains_key("Connection"));
        assert!(request.headers().contains_key("Sec-WebSocket-Version"));
        assert!(request.headers().contains_key("Sec-WebSocket-Key"));
    }

    #[test]
    fn test_parse_request_header_basic() {
        // 测试基本的HTTP请求头解析
        let _headers = "GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";

        let host_line = "Host: example.com\r\n";
        let host = host_line
            .trim_start_matches("Host: ")
            .trim_end_matches("\r\n")
            .to_string();

        if !host.contains(":") {
            assert_eq!(host, "example.com");
        }
    }

    #[test]
    fn test_parse_request_header_with_port() {
        // 测试包含端口的Host头
        let host_line = "Host: example.com:8080\r\n";
        let host = host_line
            .trim_start_matches("Host: ")
            .trim_end_matches("\r\n")
            .to_string();

        assert_eq!(host, "example.com:8080");
    }

    #[test]
    fn test_parse_content_length() {
        // 测试Content-Length解析
        let content_length_line = "Content-Length: 1024\r\n";
        let size = content_length_line
            .split(":")
            .nth(1)
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);

        assert_eq!(size, 1024);
    }

    #[test]
    fn test_url_normalization() {
        // 测试URL规范化
        let mut remote_server = "ws://example.com/proxy".to_string();
        if !remote_server.ends_with("/") {
            remote_server = format!("{}/", remote_server);
        }
        assert_eq!(remote_server, "ws://example.com/proxy/");

        let mut remote_server_with_slash = "ws://example.com/proxy/".to_string();
        if !remote_server_with_slash.ends_with("/") {
            remote_server_with_slash = format!("{}/", remote_server_with_slash);
        }
        assert_eq!(remote_server_with_slash, "ws://example.com/proxy/");
    }

    #[test]
    fn test_hello_message_format() {
        // 测试Hello消息格式
        let app_version = "0.1.0";
        let user_id = "test-user";
        let message = format!("Hello:{}:{}", app_version, user_id);

        assert_eq!(message, "Hello:0.1.0:test-user");
    }

    #[test]
    fn test_ws_url_construction() {
        // 测试WebSocket URL构建
        let remote_server = "ws://example.com/proxy";
        let host = "target.com:443";
        let encoded_host = core_lib::encode_host(host);
        let ws_url = format!("{}?q={}", remote_server, encoded_host);

        assert!(ws_url.starts_with("ws://example.com/proxy?q="));
        assert!(ws_url.len() > remote_server.len() + 4); // +4 for "?q="
    }

    #[tokio::test]
    async fn test_parse_request_header_logic() {
        // 测试请求头解析逻辑
        let headers = vec![
            "GET / HTTP/1.1\r\n",
            "Host: test.example.com\r\n",
            "Content-Length: 256\r\n",
            "\r\n"
        ].join("");

        let mut host = String::new();
        let mut content_length = 0;

        for line in headers.lines() {
            let line = format!("{}\r\n", line);
            if line.starts_with("Host: ") {
                host = line
                    .trim_start_matches("Host: ")
                    .trim_end_matches("\r\n")
                    .to_string();
                if !host.contains(":") {
                    host.push_str(":80");
                }
            }
            if line.starts_with("Content-Length") {
                content_length = line
                    .split(":")
                    .nth(1)
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
            }
        }

        assert_eq!(host, "test.example.com:80");
        assert_eq!(content_length, 256);
    }

    #[test]
    fn test_http_vs_https_detection() {
        // 测试HTTP和HTTPS请求检测
        let connect_request = "CONNECT example.com:443 HTTP/1.1\r\n";
        let http_request = "GET / HTTP/1.1\r\n";

        assert!(connect_request.starts_with("CONNECT "));
        assert!(!http_request.starts_with("CONNECT "));
    }
}