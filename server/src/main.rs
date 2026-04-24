use async_std::task;
use clap::Parser;
use server::{ServerConfig, bind, make_shutdown_channel, serve};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Allowed user IDs.
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
    let addr = format!("{}:{}", cli.host, cli.port);
    task::block_on(async {
        let listener = bind(&addr).await.unwrap();
        let (_tx, rx) = make_shutdown_channel();
        serve(listener, ServerConfig { users: cli.users }, rx)
            .await
            .unwrap();
    });
}
