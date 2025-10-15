use clap::Parser;

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
            server::server(addr, user_ids).await;
        })
    });
    let _ = server_thread.join();
}
