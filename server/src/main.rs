use async_std::{
    io::WriteExt,
    net::{TcpListener, TcpStream},
    task,
};
use async_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        http::StatusCode,
        Message,
    },
    WebSocketStream,
};
use clap::Parser;
use futures::AsyncReadExt;
use futures_util::{StreamExt};
use semver::{Version, VersionReq};
use uuid::Uuid;

const SUPPORTED_VERSION: &str = ">=0.1.0";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// User id list.
    #[arg(long)]
    users: Vec<String>,

    /// Local server address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Local server port
    #[arg(long, default_value = "9001")]
    port: u16,
}

fn main() {
    let cli = Cli::parse();

    let server_thread = std::thread::spawn(move || {
        let addr = format!("{}:{}", cli.host, cli.port);
        let user_ids = cli.users.to_vec();
        async_std::task::block_on(async {
            server(addr, user_ids).await;
        })
    });
    let _ = server_thread.join();
}

async fn server(addr: String, user_list: Vec<String>) {
    let server = TcpListener::bind(addr).await.unwrap();
    println!("Listening on: {}", server.local_addr().unwrap());
    while let Ok((stream, _)) = server.accept().await {
        let user_list = user_list.to_vec();
        task::spawn(accept_connection(stream, user_list));
    }
}

async fn accept_connection(stream: TcpStream, user_list: Vec<String>) {
    let mut dist = String::new();
    let mut is_control_request = false;
    let callback = |req: &Request, response: Response| {
        let query = req.uri().query().unwrap_or("");
        if !query.is_empty() {
            let encoded_host = req.uri().query().unwrap().replace("q=", "");
            dist = core_lib::decode_host(encoded_host);
        }

        if req.uri().path().ends_with("control") {
            is_control_request = true;
        } else {
            let token = req.headers().get("x-token");
            if token.is_none() {
                let resp = Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Some("Access denied".into()))
                    .unwrap();
                return Err(resp);
            }
        }

        Ok(response)
    };
    let ws_stream = accept_hdr_async(stream, callback)
        .await
        .expect("Error during the websocket handshake occurred");

    if is_control_request {
        process_control(ws_stream, user_list).await;
    } else {
        exchange_data(ws_stream, dist).await;
    }
}

async fn process_control(mut ws_stream: WebSocketStream<TcpStream>, user_list: Vec<String>) {
    let msg = ws_stream.next().await.unwrap().unwrap();
    let allow_str = "Hi:";
    let disallow_str = "Bye:";
    if msg.is_text() {
        let msg = msg.to_text().unwrap();
        if msg.starts_with("Hello:") {
            let ms: Vec<&str> = msg.split(":").collect();
            let client_app_version = ms[1];
            let client_user_id = ms[2].to_string();
            let download_url = "https://github.com/echo-proxy/echo-proxy-core/releases";

            let installed_semver = Version::parse(client_app_version).unwrap();
            let required_semver = VersionReq::parse(SUPPORTED_VERSION).unwrap();

            if !required_semver.matches(&installed_semver) {
                // 不支持的版本
                let msg = format!("The installed version is not supported, please download the latest version from {}", download_url);

                ws_stream
                    .send(Message::Text(format!("{}{}", disallow_str, msg).into()))
                    .await
                    .unwrap();
                ws_stream.close(None).await.unwrap();
                return;
            }

            if !user_list.contains(&client_user_id) {
                // 不支持的用户
                ws_stream
                    .send(Message::Text(format!(
                        "{}{}",
                        disallow_str, "User not allowed"
                    ).into()))
                    .await
                    .unwrap();
                ws_stream.close(None).await.unwrap();
                return;
            }
            let token = Uuid::new_v4();

            ws_stream
                .send(Message::Text(format!("{}{}", allow_str, token).into()))
                .await
                .unwrap();
        }
    } else {
        ws_stream.close(None).await.unwrap();
        return;
    }
}

async fn exchange_data(ws_stream: WebSocketStream<TcpStream>, dist: String) {
    let tcp_stream = TcpStream::connect(dist).await.unwrap();

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.split();
    let (ws_writer, mut ws_reader) = ws_stream.split();

    std::thread::spawn(move || {
        task::block_on(async {
            while let Some(msg) = ws_reader.next().await {
                let msg = msg;
                if msg.is_err() {
                    break;
                }
                let msg = msg.unwrap();
                if msg.is_binary() {
                    tcp_writer.write_all(&msg.into_data()).await.unwrap();
                }
            }
        });
    });

    std::thread::spawn(move || {
        task::block_on(async {
            loop {
                let mut buf = vec![0; 1024];
                let n = tcp_reader.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                let data = &buf[0..n];
                // debug!("t -> s {:?}", data);
                let data = data.to_vec();
                let res = ws_writer.send(Message::Binary(data.into())).await;
                if res.is_err() {
                    break;
                }
            }
        });
    });
}
