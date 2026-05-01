//! HTTP/3 routing and WebTransport sessions on Quinn (same UDP port).

use std::{sync::Arc, time::Duration};

use bytes::{Buf, Bytes};
use h3::ext::Protocol;
use h3::quic::BidiStream as H3QuicBidi;
use h3::server::RequestStream;
use h3_webtransport::server::{AcceptedBi, WebTransportSession};
use h3_webtransport::stream::BidiStream as WtBidiStream;
use http::{HeaderValue, Method, Response, StatusCode};
use tokio::task;

use crate::dataplane;

pub const HEALTH_PATH: &str = "/health";
pub const WT_PATH: &str = "/wt";

/// Quinn bidirectional stream carrier for HTTP/3 request/response bodies (`B` = [`Bytes`]).
pub type QuinnBidiBytes = h3_quinn::BidiStream<Bytes>;

/// Incoming WebTransport data stream wrapping a Quinn [`QuinnBidiBytes`].
pub type WtBidiBytes = WtBidiStream<QuinnBidiBytes, Bytes>;

pub type H3RequestStreamBytes = RequestStream<QuinnBidiBytes, Bytes>;

fn wt_protocol_ok(req: &http::Request<()>) -> bool {
    matches!(
        req.extensions().get::<Protocol>(),
        Some(p) if *p == Protocol::WEB_TRANSPORT
    )
}

pub fn quinn_server_config(identity: wtransport::Identity) -> std::io::Result<quinn::ServerConfig> {
    let mut tls_config = wtransport::tls::server::build_default_tls_config(identity);

    tls_config.max_early_data_size = u32::MAX;

    let crypto = match quinn::crypto::rustls::QuicServerConfig::try_from(tls_config) {
        Ok(c) => c,
        Err(e) => return Err(std::io::Error::other(e)),
    };
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(
        crate::WEBTRANSPORT_KEEP_ALIVE_SECS,
    )));

    match quinn::IdleTimeout::try_from(Duration::from_secs(crate::WEBTRANSPORT_IDLE_TIMEOUT_SECS)) {
        Ok(timeout) => {
            transport_config.max_idle_timeout(Some(timeout));
        }
        Err(_) => {
            tracing::warn!("invalid QUIC idle timeout, using QUIC default");
        }
    }

    server_config.transport = Arc::new(transport_config);
    Ok(server_config)
}

pub async fn run_h3_on_quic_connection(conn: quinn::Connection, users: Arc<Vec<String>>) {
    let h3_quinn_conn = h3_quinn::Connection::new(conn);
    let mut h3_conn = match h3::server::builder()
        .enable_webtransport(true)
        .enable_extended_connect(true)
        .enable_datagram(true)
        .max_webtransport_sessions(64)
        .send_grease(true)
        .build(h3_quinn_conn)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("h3 handshake failed: {e}");
            return;
        }
    };

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let resolved = resolver.resolve_request().await;
                let Ok((req, mut req_stream)) = resolved else {
                    tracing::debug!("resolve_request failed {:?}", resolved.err());
                    continue;
                };

                match (req.method().clone(), req.uri().path().to_owned()) {
                    (Method::GET, ref p) if p == HEALTH_PATH => {
                        match send_health_ok(&mut req_stream).await {
                            Ok(()) => {}
                            Err(e) => tracing::debug!("health reply failed: {e}"),
                        }
                    }
                    (Method::CONNECT, ref p) if p == WT_PATH && wt_protocol_ok(&req) => {
                        match WebTransportSession::accept(req, req_stream, h3_conn).await {
                            Ok(session) => {
                                if let Err(e) =
                                    run_webtransport_session(session, users.clone()).await
                                {
                                    tracing::debug!("webtransport session error: {:?}", e);
                                }
                            }
                            Err(e) => tracing::debug!("WebTransportSession::accept error: {:?}", e),
                        }
                        break;
                    }
                    _ => {
                        let _ = respond_status_text(
                            &mut req_stream,
                            StatusCode::NOT_FOUND,
                            b"Not Found",
                        )
                        .await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("accept on h3 connection: {e}");
                break;
            }
        }
    }
}

/// Read and discard the entire request body so the QUIC receive side reaches a clean EOF.
/// Without this, dropping `RequestStream` after `finish()` can reset the stream and confuse
/// clients such as curl (`HTTP/3 stream reset by server`).
async fn drain_request_body(stream: &mut H3RequestStreamBytes) -> Result<(), h3::error::StreamError> {
    while let Some(mut chunk) = stream.recv_data().await? {
        let n = chunk.remaining();
        chunk.advance(n);
    }
    Ok(())
}

async fn send_health_ok(
    stream: &mut H3RequestStreamBytes,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    drain_request_body(stream).await?;

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        )
        .body(())?;

    stream.send_response(resp).await?;

    stream.send_data(Bytes::from_static(b"ok\n")).await?;

    stream.finish().await?;

    Ok(())
}

async fn respond_status_text(stream: &mut H3RequestStreamBytes, status: StatusCode, msg: &[u8]) {
    if drain_request_body(stream).await.is_err() {
        return;
    }
    if let Ok(resp) = Response::builder().status(status).body(()) {
        if stream.send_response(resp).await.is_err() {
            return;
        }
        let _ = stream.send_data(Bytes::copy_from_slice(msg)).await;
        let _ = stream.finish().await;
    }
}

async fn accept_first_bidi(
    session: &WebTransportSession<h3_quinn::Connection, Bytes>,
) -> Result<Option<WtBidiBytes>, h3::error::StreamError> {
    loop {
        match session.accept_bi().await {
            Ok(Some(AcceptedBi::BidiStream(_sid, bs))) => return Ok(Some(bs)),
            Ok(Some(AcceptedBi::Request(..))) => {
                tracing::trace!("skipped non-bidi AcceptedBi variant");
            }
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

async fn run_webtransport_session(
    session: WebTransportSession<h3_quinn::Connection, Bytes>,
    users: Arc<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let first_bidi = match accept_first_bidi(&session).await {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(()),
        Err(e) => return Err(Box::new(e)),
    };

    let (mut ctrl_send, mut ctrl_recv) = H3QuicBidi::split(first_bidi);

    dataplane::run_auth(&mut ctrl_send, &mut ctrl_recv, &users).await;
    drop((ctrl_send, ctrl_recv));

    loop {
        match session.accept_bi().await {
            Ok(Some(AcceptedBi::BidiStream(_sid, stream))) => {
                let (s, r) = H3QuicBidi::split(stream);
                task::spawn(dataplane::handle_proxy_stream(s, r));
            }
            Ok(Some(AcceptedBi::Request(..))) => {
                tracing::trace!("ignored AcceptedBi request in transport session loop");
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("WT accept_bi ended: {:?}", e);
                break;
            }
        }
    }
    Ok(())
}
