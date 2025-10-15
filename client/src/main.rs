use clap::Parser;
use std::time::Instant;

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
    let is_server_ok = async_std::task::block_on(client::check_server(&cli.endpoint.as_str(), &cli.user.as_str()));
    let duration = start.elapsed();

    println!("Check server use time: {:?}", duration);
    if !is_server_ok {
        // println!("Failed to connect to the server, please check the server configuration.");
        return;
    }
    let addr = format!("{}:{}", cli.host, cli.port);

    async_std::task::block_on(client::run(cli.endpoint.as_str(), addr));
}
