use base64::prelude::*;
use rand::Rng;
use async_std::net::TcpStream;
use futures::{prelude::*};

fn generate_random_character() -> char {
    let res = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let ascii_start = 0; // start of printable ascii characters
    let ascii_end = res.len() - 1; // end of printable ascii characters
    let idx = rng.gen_range(ascii_start..=ascii_end);
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




#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_host() {
        let host = "example.com";
        let encoded_host = encode_host(host);
        let decoded_host = decode_host(encoded_host.to_string());
        assert_eq!(decoded_host, host);
    }

}
