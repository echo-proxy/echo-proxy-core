use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::task;
use async_tungstenite::tungstenite::http::Request;
use async_tungstenite::tungstenite::http::Uri;
use async_tungstenite::{async_std::connect_async, tungstenite::Message};
use futures::{prelude::*, StreamExt};
use std::str::FromStr;
use std::time::Instant;
use clap::Parser;

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
    let is_server_ok = task::block_on(check_server(&cli.endpoint.as_str(), &cli.user.as_str()));
    let duration = start.elapsed();

    println!("Check server use time: {:?}", duration);
    if !is_server_ok {
        // println!("Failed to connect to the server, please check the server configuration.");
        return;
    }
    let addr = format!("{}:{}", cli.host, cli.port);

    task::block_on(run(cli.endpoint.as_str(), addr));
}


fn builder_request(url_str:&str, token:&str)  -> Result<async_tungstenite::tungstenite::http::Request<()>, async_tungstenite::tungstenite::http::Error>{
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

async fn check_server(remote_server: &str, user_id: &str) -> bool {
    let app_version = env!("CARGO_PKG_VERSION");
    let mut remote_server = remote_server.to_string();
    if !remote_server.ends_with("/") {
        remote_server = format!("{}/", remote_server);
    }
    let (mut ws_stream, _) = connect_async(format!("{}{}", remote_server, "control"))
        .await
        .expect("Failed to connect");
    ws_stream
        .send(Message::Text(format!("Hello:{}:{}", app_version, user_id)))
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

async fn run(remote_server: &str, addr: String) {
    let listener = TcpListener::bind(addr).await.unwrap();

    let mut incoming = listener.incoming();
    let addr = listener.local_addr().unwrap();
    println!("http proxy listening on {}", addr);

    while let Some(stream) = incoming.next().await {
        let stream = stream;
        if stream.is_err() {
            println!("failed to accept connection: {:?}", stream.err().unwrap());
            continue;
        }
        let stream = stream.unwrap();

        let remote_server = remote_server.to_string();
        std::thread::spawn(move || {
            task::block_on(handle_connection(&remote_server, stream));
        });
    }
}

async fn parse_request_header(tcp_stream: TcpStream) -> (String, String, Vec<u8>) {
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



async fn handle_https(ws_url: String, mut tcp_stream: TcpStream) {
    let ws_request = builder_request(&ws_url, "token").unwrap();
    let (ws_stream, _) = connect_async(ws_request).await.expect("Failed to connect");
    tcp_stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .unwrap();
    tcp_stream.flush().await.unwrap();

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();

    let (mut ws_writer, mut ws_reader) = ws_stream.split();

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
                let res = ws_writer.send(Message::Binary(data)).await;
                if res.is_err() {
                    break;
                }
            }
        });
    });
}

async fn handle_http(ws_url: String, mut tcp_stream: TcpStream, headers: String, body: Vec<u8>) {
    let ws_request = builder_request(&ws_url, "token").unwrap();

    let (mut ws_stream, _) = connect_async(ws_request).await.expect("Failed to connect");

    ws_stream
        .send(Message::Binary(headers.as_bytes().to_vec()))
        .await
        .unwrap();
    if body.len() > 0 {
        ws_stream.send(Message::Binary(body)).await.unwrap();
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

async fn handle_connection(remote_server: &str, tcp_stream: TcpStream) {
    let (host, headers, _body) = parse_request_header(tcp_stream.clone()).await;
    let ws_url = format!("{}?q={}", remote_server, core_lib:: encode_host(host.as_str()));
    println!("accepted {}", host);

    if headers.starts_with("CONNECT ") {
        handle_https(ws_url, tcp_stream).await;
    } else {
        handle_http(ws_url, tcp_stream, headers, _body).await;
    }
}
