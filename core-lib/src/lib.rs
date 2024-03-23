use base64::prelude::*;
use rand::Rng;

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
