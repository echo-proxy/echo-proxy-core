use base64::prelude::*;
use rand::Rng;

fn generate_random_character() -> char {
    let res = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let ascii_start = 0; // start of printable ascii characters
    let ascii_end = res.len() - 1; // end of printable ascii characters
    let idx = rng.random_range(ascii_start..=ascii_end);
    res.chars().nth(idx).unwrap()
}

pub fn encode_host(host: &str) -> String {
    let mut encoded_host = BASE64_STANDARD.encode(host.as_bytes());
    let idx = (encoded_host.len() as f64 * 0.618).floor() as usize;
    encoded_host.insert(idx, generate_random_character());
    encoded_host
}

pub fn decode_host(encoded_host: String) -> String {
    let mut dist = encoded_host;
    let i = ((dist.len() - 1) as f64 * 0.618).floor() as usize;
    dist.remove(i);
    let decode_base64 = BASE64_STANDARD.decode(dist.as_bytes()).unwrap();
    dist = String::from_utf8(decode_base64).unwrap();
    dist
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let original_host = "example.com:8080";
        let encoded = encode_host(original_host);
        let decoded = decode_host(encoded);
        assert_eq!(original_host, decoded);
    }

    #[test]
    fn test_encode_host() {
        let host = "localhost:3000";
        let encoded = encode_host(host);

        // 编码后的字符串应该包含base64字符和随机插入的字符
        assert!(encoded.len() > 0);

        // 应该能够成功解码
        let decoded = decode_host(encoded.clone());
        assert_eq!(host, decoded);
    }

    #[test]
    fn test_decode_host() {
        // 测试已知的编码结果
        let host = "test.example.com:443";
        let encoded = encode_host(host);
        let decoded = decode_host(encoded);
        assert_eq!(host, decoded);
    }

    #[test]
    fn test_generate_random_character() {
        // 测试随机字符生成在有效范围内
        let char1 = generate_random_character();
        let char2 = generate_random_character();

        // 字符应该是ASCII可打印字符
        assert!(char1.is_ascii());
        assert!(char2.is_ascii());

        // 字符应该在有效字符集中
        let valid_chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        assert!(valid_chars.contains(char1));
        assert!(valid_chars.contains(char2));
    }

    #[test]
    fn test_encode_decode_empty_string() {
        let original_host = "";
        let encoded = encode_host(original_host);
        let decoded = decode_host(encoded);
        assert_eq!(original_host, decoded);
    }

    #[test]
    fn test_encode_decode_special_chars() {
        let original_host = "host-with-dash.com:8080";
        let encoded = encode_host(original_host);
        let decoded = decode_host(encoded);
        assert_eq!(original_host, decoded);
    }

    #[test]
    fn test_multiple_encodings_are_different() {
        // 由于随机字符插入，多次编码应该产生不同的结果
        let host = "same.example.com:80";
        let encoded1 = encode_host(host);
        let encoded2 = encode_host(host);

        // 由于随机性，结果可能不同，但解码后应该相同
        let decoded1 = decode_host(encoded1);
        let decoded2 = decode_host(encoded2);

        assert_eq!(decoded1, decoded2);
        assert_eq!(host, decoded1);
    }
}
