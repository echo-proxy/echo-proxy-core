use std::{
    fs,
    net::UdpSocket,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("echo-proxy-server-cert-test-{unique}"));
        fs::create_dir(&path).expect("create temp cert dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn stderr(&mut self) -> String {
        let Some(stderr) = self.child.stderr.take() else {
            return String::new();
        };

        std::io::read_to_string(stderr).unwrap_or_default()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_binary_starts_with_pem_certificate() {
    let cert_dir = TempDir::new();
    let key_path = cert_dir.path().join("server.key");
    let cert_path = cert_dir.path().join("server.crt");
    let config_path = cert_dir.path().join("config.toml");
    let cert_hash = generate_test_certificate(&key_path, &cert_path);
    fs::write(&config_path, "[server]\n").expect("write server config");

    let port = unused_udp_port();
    let mut server = ChildGuard::new(
        Command::new(env!("CARGO_BIN_EXE_server"))
            .args([
                "--host",
                "127.0.0.1",
                "--config",
                config_path.to_str().expect("config path is utf-8"),
                "--port",
                &port.to_string(),
                "--users",
                "testuser",
                "--cert",
                cert_path.to_str().expect("cert path is utf-8"),
                "--key",
                key_path.to_str().expect("key path is utf-8"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn server binary"),
    );

    let endpoint = format!("https://127.0.0.1:{port}/");
    let trust = client::TlsTrustConfig::CertHash(cert_hash);
    let connected = wait_for_auth(&endpoint, trust, &mut server).await;
    assert!(connected, "server did not accept auth with PEM certificate");
}

fn generate_test_certificate(key_path: &Path, cert_path: &Path) -> wtransport::tls::Sha256Digest {
    let identity =
        wtransport::Identity::self_signed(["localhost", "127.0.0.1"]).expect("test identity");
    let cert_hash = identity.certificate_chain().as_slice()[0].hash();
    let cert_pem = identity
        .certificate_chain()
        .as_slice()
        .iter()
        .map(|cert| cert.to_pem())
        .collect::<String>();
    let key_pem = identity.private_key().to_secret_pem();

    fs::write(cert_path, cert_pem).expect("write test certificate");
    fs::write(key_path, key_pem).expect("write test private key");

    cert_hash
}

fn unused_udp_port() -> u16 {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind free udp port");
    socket.local_addr().expect("read local addr").port()
}

async fn wait_for_auth(
    endpoint: &str,
    trust: client::TlsTrustConfig,
    server: &mut ChildGuard,
) -> bool {
    for _ in 0..10 {
        if let Some(status) = server.try_wait().expect("check server status") {
            panic!("server exited early with {status}: {}", server.stderr());
        }

        let config = trust.build_client_config();
        let auth = tokio::time::timeout(
            Duration::from_secs(1),
            client::connect_and_auth(endpoint, "testuser", config),
        )
        .await;

        if matches!(auth, Ok(Some(_))) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    false
}
