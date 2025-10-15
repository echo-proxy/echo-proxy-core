//! 集成测试包
//! 这个包包含项目的集成测试

#[cfg(test)]
mod tests {
    use core_lib;
    use semver::{Version, VersionReq};

    #[tokio::test]
    async fn test_encode_decode_integration() {
        // 测试core-lib的编码解码功能
        let original_host = "example.com:8080";
        let encoded = core_lib::encode_host(original_host);
        let decoded = core_lib::decode_host(encoded);

        assert_eq!(original_host, decoded);
    }

    #[tokio::test]
    async fn test_hello_message_format() {
        // 测试Hello消息格式
        let app_version = "0.1.0";
        let user_id = "test-user";
        let message = format!("Hello:{}:{}", app_version, user_id);

        assert_eq!(message, "Hello:0.1.0:test-user");
    }

    #[tokio::test]
    async fn test_url_normalization() {
        // 测试URL规范化逻辑
        let mut remote_server = "ws://example.com/proxy".to_string();
        if !remote_server.ends_with("/") {
            remote_server = format!("{}/", remote_server);
        }
        assert_eq!(remote_server, "ws://example.com/proxy/");
    }

    #[tokio::test]
    async fn test_http_header_parsing() {
        // 测试HTTP头解析逻辑
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

    #[tokio::test]
    async fn test_http_vs_https_detection() {
        // 测试HTTP和HTTPS请求检测
        let connect_request = "CONNECT example.com:443 HTTP/1.1\r\n";
        let http_request = "GET / HTTP/1.1\r\n";

        assert!(connect_request.starts_with("CONNECT "));
        assert!(!http_request.starts_with("CONNECT "));
    }

    #[tokio::test]
    async fn test_websocket_url_construction() {
        // 测试WebSocket URL构建
        let remote_server = "ws://example.com/proxy";
        let host = "target.com:443";
        let encoded_host = core_lib::encode_host(host);
        let ws_url = format!("{}?q={}", remote_server, encoded_host);

        assert!(ws_url.starts_with("ws://example.com/proxy?q="));
        assert!(ws_url.len() > remote_server.len() + 4); // +4 for "?q="
    }

    #[tokio::test]
    async fn test_version_compatibility() {
        // 测试版本兼容性检查
        let required_semver = VersionReq::parse(">=0.1.0").unwrap();
        let supported_version = Version::parse("0.1.0").unwrap();
        let unsupported_version = Version::parse("0.0.9").unwrap();

        assert!(required_semver.matches(&supported_version));
        assert!(!required_semver.matches(&unsupported_version));
    }

    #[tokio::test]
    async fn test_user_authentication() {
        // 测试用户认证逻辑
        let user_list = vec!["user1".to_string(), "user2".to_string()];
        let valid_user = "user1".to_string();
        let invalid_user = "user3".to_string();

        assert!(user_list.contains(&valid_user));
        assert!(!user_list.contains(&invalid_user));
    }
}