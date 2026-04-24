pub mod frame;
pub mod session;

pub use frame::{Frame, FrameError};
pub use session::{DataRx, DataTx, FrameRx, FrameTx, StreamRegistry, frame_channel};

/// Encode `host:port` with a single obfuscation character inserted at the
/// golden-ratio position. Not cryptographic — TLS is the actual security layer.
#[deprecated(note = "replaced by mux Frame protocol; will be removed")]
pub fn encode_host(host: &str) -> String {
    use base64::prelude::*;
    use rand::Rng;
    let mut encoded = BASE64_STANDARD.encode(host.as_bytes());
    let idx = (encoded.len() as f64 * 0.618).floor() as usize;
    let c = {
        let charset = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let i = rand::rng().random_range(0..charset.len());
        charset.chars().nth(i).unwrap()
    };
    encoded.insert(idx, c);
    encoded
}

/// Decode a host string produced by [`encode_host`].
#[deprecated(note = "replaced by mux Frame protocol; will be removed")]
pub fn decode_host(encoded_host: String) -> String {
    use base64::prelude::*;
    let mut s = encoded_host;
    let i = ((s.len() - 1) as f64 * 0.618).floor() as usize;
    s.remove(i);
    let bytes = BASE64_STANDARD.decode(s.as_bytes()).unwrap();
    String::from_utf8(bytes).unwrap()
}
