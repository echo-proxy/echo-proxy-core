//! 集成测试 - 完整代理工作流
//! 这些测试模拟完整的代理流程

use core_lib;

#[tokio::test]
async fn test_complete_proxy_workflow() {
    // 测试完整的代理工作流

    // 1. 客户端编码主机地址
    let target_host = "example.com:443";
    let encoded_host = core_lib::encode_host(target_host);

    // 2. 服务器解码主机地址
    let decoded_host = core_lib::decode_host(encoded_host.clone());

    // 验证编码/解码的正确性
    assert_eq!(target_host, decoded_host);

    // 3. 模拟WebSocket URL构建
    let remote_server = "ws://proxy-server.com/proxy";
    let ws_url = format!("{}?q={}", remote_server, encoded_host);

    assert!(ws_url.contains("?q="));
    assert!(ws_url.len() > remote_server.len() + 4);
}

#[tokio::test]
async fn test_http_protocol_parsing() {
    // 测试HTTP协议解析的完整流程

    // 模拟HTTP请求
    let http_request = vec![
        "GET /api/data HTTP/1.1\r\n",
        "Host: api.example.com\r\n",
        "User-Agent: test-client\r\n",
        "Content-Type: application/json\r\n",
        "Content-Length: 128\r\n",
        "\r\n"
    ].join("");

    // 解析过程
    let mut host = String::new();
    let mut content_length = 0;

    for line in http_request.lines() {
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

    assert_eq!(host, "api.example.com:80");
    assert_eq!(content_length, 128);
}

#[tokio::test]
async fn test_https_tunnel_workflow() {
    // 测试HTTPS隧道工作流

    // 模拟CONNECT请求
    let connect_request = "CONNECT secure.example.com:443 HTTP/1.1\r\n";

    // 验证CONNECT请求检测
    assert!(connect_request.starts_with("CONNECT "));

    // 提取目标主机
    let target_host = connect_request
        .trim_start_matches("CONNECT ")
        .split_whitespace()
        .next()
        .unwrap();

    assert_eq!(target_host, "secure.example.com:443");
}

#[tokio::test]
async fn test_multiple_encoding_consistency() {
    // 测试多次编码的一致性

    let test_hosts = [
        "localhost:3000",
        "api.example.com:443",
        "192.168.1.1:8080",
        "test-service.internal:9000"
    ];

    for &host in &test_hosts {
        // 多次编码应该都能正确解码
        for _ in 0..5 {
            let encoded = core_lib::encode_host(host);
            let decoded = core_lib::decode_host(encoded);
            assert_eq!(host, decoded, "编码解码失败: {}", host);
        }
    }
}

#[tokio::test]
async fn test_error_scenarios() {
    // 测试错误场景处理

    // 测试空字符串
    let empty_encoded = core_lib::encode_host("");
    let empty_decoded = core_lib::decode_host(empty_encoded);
    assert_eq!("", empty_decoded);

    // 测试特殊字符
    let special_host = "host-with-dash.com:8080";
    let special_encoded = core_lib::encode_host(special_host);
    let special_decoded = core_lib::decode_host(special_encoded);
    assert_eq!(special_host, special_decoded);
}