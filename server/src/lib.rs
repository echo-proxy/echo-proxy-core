//! WebTransport proxy server: HTTP/3 on Quinn with `/health` and WebTransport `/wt`.

mod dataplane;
mod h3_listen;

pub use dataplane::{is_dest_allowed, is_public_ip};

use std::{net::SocketAddr, sync::Arc};
use tokio::{sync::broadcast, task};
use wtransport::Identity;

pub(crate) const WEBTRANSPORT_KEEP_ALIVE_SECS: u64 = 10;
pub(crate) const WEBTRANSPORT_IDLE_TIMEOUT_SECS: u64 = 120;

pub struct ServerOptions {
    pub users: Vec<String>,
    pub identity: Identity,
}

/// Bound HTTP/3 + WebTransport server (Quinn, single UDP port).
pub struct ServerEndpoint {
    endpoint: quinn::Endpoint,
    users: Arc<Vec<String>>,
}

impl ServerEndpoint {
    pub fn bind(addr: SocketAddr, opts: ServerOptions) -> std::io::Result<Self> {
        let config = h3_listen::quinn_server_config(opts.identity)?;

        let endpoint = quinn::Endpoint::server(config, addr)?;
        Ok(Self {
            endpoint,
            users: Arc::new(opts.users),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("local_addr")
    }

    pub async fn serve(self, mut shutdown: broadcast::Receiver<()>) -> std::io::Result<()> {
        tracing::info!(addr = %self.local_addr(), "HTTP/3 + WebTransport server listening");
        loop {
            tokio::select! {
                incoming = self.endpoint.accept() => {
                    let Some(connecting) = incoming else { continue };
                    let users = self.users.clone();
                    task::spawn(async move {
                        match connecting.await {
                            Ok(conn) => {
                                h3_listen::run_h3_on_quic_connection(conn, users).await;
                            }
                            Err(e) => tracing::debug!("QUIC handshake failed: {e}"),
                        }
                    });
                }
                _ = shutdown.recv() => break,
            }
        }
        self.endpoint.close(0u32.into(), b"shutdown");
        self.endpoint.wait_idle().await;
        Ok(())
    }
}

pub async fn serve(
    addr: SocketAddr,
    opts: ServerOptions,
    shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    ServerEndpoint::bind(addr, opts)?.serve(shutdown).await
}

pub fn make_shutdown_channel() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
    broadcast::channel(1)
}
