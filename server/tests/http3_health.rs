//! HTTP/3 `GET /health` against [`server::ServerEndpoint`] using Quinn + `h3` client stack.

use std::sync::{Arc, Once};

use futures::future::poll_fn;

use bytes::Buf;
use rustls::{
    ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto,
    pki_types::{CertificateDer, ServerName, UnixTime},
};

static INSTALL_CRYPTO_PROVIDER: Once = Once::new();

fn ensure_crypto_provider() {
    INSTALL_CRYPTO_PROVIDER.call_once(|| {
        crypto::ring::default_provider()
            .install_default()
            .expect("TLS CryptoProvider"); // pragma: allow panics — test bootstrap
    });
}

#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_health_returns_200_via_h3_client() {
    ensure_crypto_provider();

    let identity = wtransport::Identity::self_signed(["localhost", "127.0.0.1"])
        .expect("self-signed identity");

    let se = server::ServerEndpoint::bind(
        "127.0.0.1:0".parse().expect("sockaddr"),
        server::ServerOptions {
            users: vec!["u".into()],
            identity,
        },
    )
    .expect("bind");
    let port = se.local_addr().port();

    let (stop_tx, stop_rx) = tokio::sync::broadcast::channel::<()>(1);
    tokio::task::spawn(async move {
        se.serve(stop_rx).await.ok();
    });

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let tls = Arc::new({
        let mut tls =
            ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
                .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        tls
    });

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("UDP client");

    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QuicClientConfig"),
    )));

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let conn = endpoint
        .connect(addr, "localhost")
        .expect("connect")
        .await
        .expect("QUIC");

    let h3_sock = h3_quinn::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(h3_sock).await.expect("h3 client");

    let uri: http::Uri = format!("https://localhost:{port}/health")
        .parse()
        .expect("URI");

    let req = http::Request::get(uri.clone())
        .body(())
        .expect("HTTP request");

    {
        let mut stream = send_request.send_request(req).await.expect("send_request");
        stream.finish().await.expect("finish");
        let resp = stream.recv_response().await.expect("recv_response");
        assert_eq!(
            resp.status(),
            http::StatusCode::OK,
            "unexpected status {:?} for {}",
            resp.status(),
            uri
        );
        let mut body = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.expect("recv_data_option") {
            let n = chunk.remaining();
            body.extend_from_slice(chunk.chunk());
            chunk.advance(n);
        }
        assert_eq!(body, b"ok\n");
    }

    drop(send_request);

    let _ = poll_fn(|cx| std::pin::Pin::new(&mut driver).poll_close(cx)).await;

    let _ = stop_tx.send(());
    endpoint.wait_idle().await;
}
