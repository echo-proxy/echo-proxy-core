use async_std::{
    channel,
    io::BufReader,
    net::{TcpListener, TcpStream},
    task,
};
use async_tungstenite::{
    async_std::connect_async,
    tungstenite::{
        handshake::client::generate_key,
        http::{Request, Uri},
        Message,
    },
};
use clap::Parser;
use core_lib::{frame_channel, Frame, FrameTx, StreamRegistry};
use futures::{io::AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, SinkExt, StreamExt};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

type PendingAcks = Arc<Mutex<HashMap<u32, channel::Sender<bool>>>>;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Remote server address
    #[arg(long)]
    endpoint: String,
    /// User id
    #[arg(long)]
    user: String,

    /// Local server address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Local server port
    #[arg(long, default_value = "9002")]
    port: u16,
}

fn main() {
    let cli = Cli::parse();

    let start = Instant::now();
    let token = task::block_on(check_server(&cli.endpoint, &cli.user));
    println!("[mux] check_server took {:?}", start.elapsed());

    let token = match token {
        Some(t) => t,
        None => return,
    };

    let addr = format!("{}:{}", cli.host, cli.port);
    task::block_on(run(cli.endpoint, addr, token));
}

fn build_request(url_str: &str) -> Result<Request<()>, async_tungstenite::tungstenite::http::Error> {
    let uri = Uri::from_str(url_str).unwrap();
    Request::builder()
        .uri(url_str)
        .header("Host", uri.host().unwrap())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .body(())
}

/// Handshake with /control, verify version and user, return the session token.
async fn check_server(endpoint: &str, user_id: &str) -> Option<String> {
    let app_version = env!("CARGO_PKG_VERSION");
    let mut base = endpoint.to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    let (mut ws, _) = connect_async(format!("{}control", base))
        .await
        .expect("[mux] failed to connect to /control");

    ws.send(Message::Text(
        format!("Hello:{}:{}", app_version, user_id).into(),
    ))
    .await
    .unwrap();
    ws.flush().await.unwrap();

    let msg = ws.next().await?.ok()?;
    let text = msg.to_text().ok()?;
    if let Some(token) = text.strip_prefix("Hi:") {
        Some(token.to_string())
    } else {
        println!("{}", text.strip_prefix("Bye:").unwrap_or(text));
        None
    }
}

async fn run(endpoint: String, addr: String, token: String) {
    let mux_url = {
        let mut base = endpoint.clone();
        if !base.ends_with('/') {
            base.push('/');
        }
        format!("{}mux", base)
    };

    let (ws_stream, _) = connect_async(build_request(&mux_url).unwrap())
        .await
        .expect("[mux] failed to connect to /mux");

    let (ws_sink, ws_src) = ws_stream.split();

    // Send HELLO with token.
    ws_sink
        .send(Message::Binary(Frame::Hello(token).encode().into()))
        .await
        .expect("[mux] failed to send HELLO");

    let (frame_tx, frame_rx) = frame_channel(128);
    let registry = StreamRegistry::new();
    let pending_acks: PendingAcks = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));

    // Writer loop: frame_rx → ws sink
    task::spawn(async move {
        while let Ok(frame) = frame_rx.recv().await {
            let bytes = frame.encode();
            if ws_sink
                .send(Message::Binary(bytes.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Reader loop: ws → Frame → route
    {
        let registry = registry.clone();
        let pending_acks = pending_acks.clone();
        task::spawn(async move {
            run_client_reader(ws_src, registry, pending_acks).await;
        });
    }

    // Accept local HTTP proxy connections.
    let listener = TcpListener::bind(&addr).await.unwrap();
    println!("[mux] http proxy listening on {}", listener.local_addr().unwrap());

    while let Ok((stream, _)) = listener.accept().await {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let ftx = frame_tx.clone();
        let reg = registry.clone();
        let packs = pending_acks.clone();
        task::spawn(handle_local_connection(stream, id, ftx, reg, packs));
    }
}

async fn run_client_reader<S>(
    mut ws_src: S,
    registry: StreamRegistry,
    pending_acks: PendingAcks,
) where
    S: futures::Stream<Item = Result<Message, async_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    while let Some(msg) = ws_src.next().await {
        let data = match msg {
            Ok(Message::Binary(d)) => d,
            Ok(Message::Ping(_)) => continue,
            _ => break,
        };
        match Frame::decode(&data) {
            Ok(Frame::OpenAck { id, ok }) => {
                if let Some(tx) = pending_acks.lock().unwrap().remove(&id) {
                    let _ = tx.try_send(ok);
                }
            }
            Ok(Frame::Data { id, bytes }) => {
                registry.route(id, bytes);
            }
            Ok(Frame::Close { id }) => {
                registry.close(id);
            }
            _ => {}
        }
    }
    registry.close_all();
}

async fn handle_local_connection(
    tcp_stream: TcpStream,
    stream_id: u32,
    frame_tx: FrameTx,
    registry: StreamRegistry,
    pending_acks: PendingAcks,
) {
    let (host, headers, body) =
        match parse_request_header(tcp_stream.clone()).await {
            Some(r) => r,
            None => return,
        };
    let is_connect = headers.starts_with("CONNECT ");

    // Register the stream BEFORE sending OPEN to avoid a DATA routing race.
    let data_rx = registry.register(stream_id);

    // Register a one-shot for OPEN_ACK.
    let (ack_tx, ack_rx) = channel::bounded::<bool>(1);
    pending_acks.lock().unwrap().insert(stream_id, ack_tx);

    if frame_tx
        .send(Frame::Open {
            id: stream_id,
            dest: host,
        })
        .await
        .is_err()
    {
        registry.close(stream_id);
        return;
    }

    let ok = ack_rx.recv().await.unwrap_or(false);
    if !ok {
        registry.close(stream_id);
        let _ = tcp_stream
            .clone()
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    let mut tcp_stream = tcp_stream;

    if is_connect {
        if tcp_stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            registry.close(stream_id);
            return;
        }
    } else {
        // Forward the buffered HTTP request as the first DATA frame.
        let mut request_data = headers.into_bytes();
        request_data.extend_from_slice(&body);
        if frame_tx
            .send(Frame::Data {
                id: stream_id,
                bytes: request_data,
            })
            .await
            .is_err()
        {
            registry.close(stream_id);
            return;
        }
    }

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    // ws DATA → tcp write
    let write_task = task::spawn(async move {
        while let Ok(data) = data_rx.recv().await {
            if tcp_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    // tcp read → ws DATA
    let mut buf = vec![0u8; 8192];
    loop {
        match tcp_reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if frame_tx
                    .send(Frame::Data {
                        id: stream_id,
                        bytes: buf[..n].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    let _ = frame_tx.send(Frame::Close { id: stream_id }).await;
    registry.close(stream_id);
    drop(write_task);
}

/// Read HTTP request headers and optional body from a cloned TcpStream.
/// Returns None on malformed input or if limits are exceeded.
async fn parse_request_header(tcp_stream: TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut headers = String::new();
    let mut host = String::new();
    let mut content_length: usize = 0;
    let mut reader = BufReader::new(tcp_stream);

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            break;
        }
        if headers.len() + line.len() > MAX_HEADER_BYTES {
            return None;
        }
        headers.push_str(&line);

        // Case-insensitive header matching
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("host:") {
            host = line["host:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .to_string();
            if !host.contains(':') {
                host.push_str(":80");
            }
        }
        if lower.starts_with("content-length:") {
            content_length = line["content-length:".len()..]
                .trim()
                .trim_end_matches("\r\n")
                .trim_end_matches('\n')
                .parse::<usize>()
                .unwrap_or(0);
            if content_length > MAX_CONTENT_LENGTH {
                return None;
            }
        }
        if line == "\r\n" {
            break;
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }

    Some((host, headers, body))
}
