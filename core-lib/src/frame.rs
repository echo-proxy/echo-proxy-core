use std::fmt;

/// Wire format: | 1 byte type | 4 byte stream_id (big-endian) | payload |
///
/// stream_id 0 is reserved for HELLO.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Client → Server. Sent once as the first frame on /mux. Payload: UTF-8 token.
    Hello(String),
    /// Client → Server. Open a new logical stream. Payload: UTF-8 "host:port".
    Open { id: u32, dest: String },
    /// Server → Client. Response to OPEN. Payload: 1 byte (0 = ok, 1 = failed).
    OpenAck { id: u32, ok: bool },
    /// Either direction. Raw TCP bytes for stream `id`.
    Data { id: u32, bytes: Vec<u8> },
    /// Either direction. Terminate stream `id`.
    Close { id: u32 },
}

#[derive(Debug)]
pub enum FrameError {
    TooShort,
    UnknownType(u8),
    InvalidUtf8,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooShort => write!(f, "frame too short"),
            FrameError::UnknownType(t) => write!(f, "unknown frame type: {:#04x}", t),
            FrameError::InvalidUtf8 => write!(f, "invalid utf-8 in frame payload"),
        }
    }
}

impl std::error::Error for FrameError {}

const TYPE_OPEN: u8 = 0x01;
const TYPE_OPEN_ACK: u8 = 0x02;
const TYPE_DATA: u8 = 0x03;
const TYPE_CLOSE: u8 = 0x04;
const TYPE_HELLO: u8 = 0x10;

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Frame::Hello(token) => {
                let mut buf = Vec::with_capacity(5 + token.len());
                buf.push(TYPE_HELLO);
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(token.as_bytes());
                buf
            }
            Frame::Open { id, dest } => {
                let mut buf = Vec::with_capacity(5 + dest.len());
                buf.push(TYPE_OPEN);
                buf.extend_from_slice(&id.to_be_bytes());
                buf.extend_from_slice(dest.as_bytes());
                buf
            }
            Frame::OpenAck { id, ok } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(TYPE_OPEN_ACK);
                buf.extend_from_slice(&id.to_be_bytes());
                buf.push(if *ok { 0 } else { 1 });
                buf
            }
            Frame::Data { id, bytes } => {
                let mut buf = Vec::with_capacity(5 + bytes.len());
                buf.push(TYPE_DATA);
                buf.extend_from_slice(&id.to_be_bytes());
                buf.extend_from_slice(bytes);
                buf
            }
            Frame::Close { id } => {
                let mut buf = Vec::with_capacity(5);
                buf.push(TYPE_CLOSE);
                buf.extend_from_slice(&id.to_be_bytes());
                buf
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Frame, FrameError> {
        if buf.len() < 5 {
            return Err(FrameError::TooShort);
        }
        let frame_type = buf[0];
        let id = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        let payload = &buf[5..];

        match frame_type {
            TYPE_HELLO => {
                let token = std::str::from_utf8(payload)
                    .map_err(|_| FrameError::InvalidUtf8)?
                    .to_string();
                Ok(Frame::Hello(token))
            }
            TYPE_OPEN => {
                let dest = std::str::from_utf8(payload)
                    .map_err(|_| FrameError::InvalidUtf8)?
                    .to_string();
                Ok(Frame::Open { id, dest })
            }
            TYPE_OPEN_ACK => {
                if payload.is_empty() {
                    return Err(FrameError::TooShort);
                }
                Ok(Frame::OpenAck {
                    id,
                    ok: payload[0] == 0,
                })
            }
            TYPE_DATA => Ok(Frame::Data {
                id,
                bytes: payload.to_vec(),
            }),
            TYPE_CLOSE => Ok(Frame::Close { id }),
            t => Err(FrameError::UnknownType(t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(frame: Frame) {
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).expect("decode failed");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn hello_round_trip() {
        round_trip(Frame::Hello("550e8400-e29b-41d4-a716-446655440000".to_string()));
    }

    #[test]
    fn open_round_trip() {
        round_trip(Frame::Open {
            id: 42,
            dest: "example.com:443".to_string(),
        });
    }

    #[test]
    fn open_ack_ok_round_trip() {
        round_trip(Frame::OpenAck { id: 42, ok: true });
    }

    #[test]
    fn open_ack_fail_round_trip() {
        round_trip(Frame::OpenAck { id: 99, ok: false });
    }

    #[test]
    fn data_round_trip() {
        round_trip(Frame::Data {
            id: 1,
            bytes: b"GET / HTTP/1.1\r\n\r\n".to_vec(),
        });
    }

    #[test]
    fn data_empty_payload_round_trip() {
        round_trip(Frame::Data {
            id: 5,
            bytes: vec![],
        });
    }

    #[test]
    fn close_round_trip() {
        round_trip(Frame::Close { id: 7 });
    }

    #[test]
    fn stream_id_preserved() {
        let id = 0xDEAD_BEEF;
        round_trip(Frame::Close { id });
        round_trip(Frame::Open {
            id,
            dest: "h:1".to_string(),
        });
    }

    #[test]
    fn too_short_returns_error() {
        assert!(matches!(Frame::decode(&[0x01, 0x00]), Err(FrameError::TooShort)));
        assert!(matches!(Frame::decode(&[]), Err(FrameError::TooShort)));
    }

    #[test]
    fn open_ack_missing_status_byte() {
        // 5 bytes minimum but no payload for OPEN_ACK
        let buf = [TYPE_OPEN_ACK, 0x00, 0x00, 0x00, 0x01];
        assert!(matches!(Frame::decode(&buf), Err(FrameError::TooShort)));
    }

    #[test]
    fn unknown_type_returns_error() {
        let buf = [0xFF, 0x00, 0x00, 0x00, 0x01];
        assert!(matches!(Frame::decode(&buf), Err(FrameError::UnknownType(0xFF))));
    }

    #[test]
    fn invalid_utf8_open() {
        let mut buf = vec![TYPE_OPEN, 0x00, 0x00, 0x00, 0x01];
        buf.push(0xFF); // invalid UTF-8
        assert!(matches!(Frame::decode(&buf), Err(FrameError::InvalidUtf8)));
    }
}
