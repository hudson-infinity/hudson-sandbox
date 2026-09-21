//! Real TCP/TLS transport, provisioning recovery, and authenticated HTTP flows.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    Router,
    body::{Body, to_bytes},
};
use http::{Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use sandbox_api::{
    AppState,
    provision::provision,
    router,
    server::{ServerLimits, serve, tls_acceptor},
};
use sandbox_protocol::{Id, ProjectId, images::ImageAllowlist};
use sandbox_store::Store;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{self, pki_types::ServerName},
};

struct Server {
    address: SocketAddr,
    client: TlsConnector,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(app: Router, limits: ServerLimits) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let acceptor = tls_acceptor(
            cert.cert.pem().as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let client = connector(roots);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, signal) = oneshot::channel();
        let task = tokio::spawn(serve(listener, acceptor, app, limits, async {
            let _ = signal.await;
        }));
        Self {
            address,
            client,
            stop: Some(stop),
            task,
        }
    }
    async fn tls(&self) -> TlsStream<TcpStream> {
        timeout(
            Duration::from_secs(2),
            self.client.connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(self.address).await.unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }
    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        key: Option<&str>,
        body: String,
    ) -> (StatusCode, http::HeaderMap, Vec<u8>) {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(self.tls().await))
                .await
                .unwrap();
        let task = tokio::spawn(connection);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost")
            .header(header::CONNECTION, "close")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let response = sender
            .send_request(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(Body::new(response.into_body()), 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        drop(sender);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (status, headers, bytes)
    }
    async fn shutdown(&mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(Duration::from_secs(2), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(TcpStream::connect(self.address).await.is_err());
    }
}
fn connector(roots: rustls::RootCertStore) -> TlsConnector {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}
fn app(pool: PgPool) -> Router {
    router(AppState {
        store: Store::from_pool(pool),
        images: ImageAllowlist::new([format!("sha256:{}", "a".repeat(64))]).unwrap(),
    })
}
fn body(response: &(StatusCode, http::HeaderMap, Vec<u8>)) -> Value {
    serde_json::from_slice(&response.2).unwrap()
}
fn token(path: &std::path::Path) -> (String, ProjectId) {
    let data: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    (
        data["token"].as_str().unwrap().to_owned(),
        serde_json::from_value(data["project_id"].clone()).unwrap(),
    )
}

#[sqlx::test(migrations = "../../migrations")]
async fn https_provision_create_retry_list_destroy_and_revoke(pool: PgPool) {
    let dir = private_dir();
    let file = dir.path().join("project.json");
    let store = Store::from_pool(pool.clone());
    let project = provision(&store, "transport", &file).await.unwrap();
    let initial = std::fs::read(&file).unwrap();
    assert_eq!(
        provision(&store, "transport", &file).await.unwrap(),
        project
    );
    assert_eq!(initial, std::fs::read(&file).unwrap());
    let (token, saved_project) = token(&file);
    assert_eq!(saved_project, project);
    let metadata: String = sqlx::query_scalar("SELECT api_tokens::text FROM projects WHERE id=$1")
        .bind(project.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!metadata.contains(&token));
    let mut server = Server::start(app(pool.clone()), ServerLimits::default()).await;
    assert_eq!(
        server
            .send("GET", "/v1/sandboxes", None, None, String::new())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let request=json!({"image_digest":format!("sha256:{}","a".repeat(64)),"resources":{"vcpu":1,"memory_mib":512,"disk_mib":1024}}).to_string();
    let created = server
        .send(
            "POST",
            "/v1/sandboxes",
            Some(&token),
            Some("https-create-key-01"),
            request.clone(),
        )
        .await;
    assert_eq!(created.0, StatusCode::ACCEPTED);
    let retry = server
        .send(
            "POST",
            "/v1/sandboxes",
            Some(&token),
            Some("https-create-key-01"),
            request,
        )
        .await;
    assert_eq!(body(&created)["operation_id"], body(&retry)["operation_id"]);
    let sandbox = body(&created)["sandbox_id"].as_str().unwrap().to_owned();
    let status_url = body(&created)["status_url"].as_str().unwrap().to_owned();
    assert_eq!(
        server
            .send("GET", &status_url, Some(&token), None, String::new())
            .await
            .0,
        StatusCode::OK
    );
    let listed = server
        .send("GET", "/v1/sandboxes", Some(&token), None, String::new())
        .await;
    assert_eq!(listed.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(body(&listed)["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        server
            .send(
                "POST",
                &format!("/v1/sandboxes/{sandbox}/destroy"),
                Some(&token),
                Some("https-destroy-key-1"),
                "{}".into()
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Seed an uncertain create to exercise cleanup admission without pretending a VM ran.
    sqlx::query("UPDATE operations SET status='unknown' WHERE project_id=$1 AND kind='create'")
        .bind(project.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let destroyed = server
        .send(
            "POST",
            &format!("/v1/sandboxes/{sandbox}/destroy"),
            Some(&token),
            Some("https-destroy-key-1"),
            "{}".into(),
        )
        .await;
    assert_eq!(destroyed.0, StatusCode::ACCEPTED);
    let destroy_retry = server
        .send(
            "POST",
            &format!("/v1/sandboxes/{sandbox}/destroy"),
            Some(&token),
            Some("https-destroy-key-1"),
            "{}".into(),
        )
        .await;
    assert_eq!(
        body(&destroyed)["operation_id"],
        body(&destroy_retry)["operation_id"]
    );
    sqlx::query("UPDATE projects SET api_tokens=jsonb_set(api_tokens,'{0,revoked_at}',to_jsonb(now())) WHERE id=$1").bind(project.uuid()).execute(&pool).await.unwrap();
    assert!(provision(&store, "transport", &file).await.is_err());
    assert_eq!(
        server
            .send("GET", "/v1/operations", Some(&token), None, String::new())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    server.shutdown().await;
}

#[tokio::test]
async fn tls_rejects_plaintext_untrusted_ca_and_wrong_hostname() {
    let mut server = Server::start(
        Router::new().route("/", axum::routing::get(|| async { "ok" })),
        ServerLimits::default(),
    )
    .await;
    let mut plain = TcpStream::connect(server.address).await.unwrap();
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut bytes = Vec::new();
    let _ = timeout(Duration::from_secs(1), plain.read_to_end(&mut bytes))
        .await
        .unwrap();
    assert!(!bytes.starts_with(b"HTTP/"));
    let empty = connector(rustls::RootCertStore::empty());
    assert!(
        empty
            .connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(server.address).await.unwrap()
            )
            .await
            .is_err()
    );
    assert!(
        server
            .client
            .connect(
                ServerName::try_from("wrong.example").unwrap(),
                TcpStream::connect(server.address).await.unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(
        server.send("GET", "/", None, None, String::new()).await.0,
        StatusCode::OK
    );
    server.shutdown().await;
}

#[test]
fn invalid_tls_material_is_rejected_without_disclosing_key_bytes() {
    let one = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let two = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    for (cert, key) in [
        (Vec::new(), Vec::new()),
        (
            one.cert.pem().into_bytes(),
            b"secret malformed key".to_vec(),
        ),
        (
            one.cert.pem().into_bytes(),
            two.signing_key.serialize_pem().into_bytes(),
        ),
    ] {
        let error = tls_acceptor(&cert, &key).err().unwrap().to_string();
        assert!(!error.contains("secret malformed key"));
        assert!(!error.contains("BEGIN PRIVATE KEY"));
    }
}

#[tokio::test]
async fn connection_slots_include_tls_handshakes_and_are_released() {
    let limits = ServerLimits {
        connections: 1,
        handshake: Duration::from_millis(200),
        ..Default::default()
    };
    let mut server = Server::start(
        Router::new().route("/", axum::routing::get(|| async { "ok" })),
        limits,
    )
    .await;
    let occupied = server.tls().await;
    let overflow = server
        .client
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(server.address).await.unwrap(),
        )
        .await;
    assert!(overflow.is_err());
    drop(occupied);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut pending = TcpStream::connect(server.address).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        server
            .client
            .connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(server.address).await.unwrap()
            )
            .await
            .is_err()
    );
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(1), pending.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        server.send("GET", "/", None, None, String::new()).await.0,
        StatusCode::OK
    );
    server.shutdown().await;
}

#[tokio::test]
async fn header_request_and_connection_deadlines_hold() {
    let limits = ServerLimits {
        headers: Duration::from_millis(100),
        request: Duration::from_millis(100),
        connection: Duration::from_millis(400),
        ..Default::default()
    };
    let mut server = Server::start(
        Router::new().route(
            "/",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "late"
            }),
        ),
        limits,
    )
    .await;
    let mut slow = server.tls().await;
    slow.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
    let mut bytes = Vec::new();
    let _ = timeout(Duration::from_secs(1), slow.read_to_end(&mut bytes))
        .await
        .unwrap();
    assert!(!bytes.windows(3).any(|v| v == b"200"));
    let response = server.send("GET", "/", None, None, String::new()).await;
    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
    // A stalled response body is outside the handler future; the connection lifetime bounds it.
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_finishes_an_inflight_request_and_bounds_stalled_handshake() {
    let (started, mut started_rx) = tokio::sync::mpsc::channel(1);
    let route = axum::routing::get(move || {
        let started = started.clone();
        async move {
            started.send(()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            "done"
        }
    });
    let mut server = Server::start(
        Router::new().route("/", route),
        ServerLimits {
            drain: Duration::from_millis(500),
            ..Default::default()
        },
    )
    .await;
    let mut stream = server.tls().await;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    started_rx.recv().await.unwrap();
    let _pending = TcpStream::connect(server.address).await.unwrap();
    server.stop.take().unwrap().send(()).unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(1), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.ends_with(b"done"));
    timeout(Duration::from_secs(1), &mut server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn oversized_json_is_not_admitted(pool: PgPool) {
    let dir = private_dir();
    let path = dir.path().join("project.json");
    provision(&Store::from_pool(pool.clone()), "body limit", &path)
        .await
        .unwrap();
    let (token, _) = token(&path);
    let mut server = Server::start(app(pool.clone()), ServerLimits::default()).await;
    let oversized=json!({"image_digest":format!("sha256:{}","a".repeat(64)),"resources":{"vcpu":1,"memory_mib":512,"disk_mib":1024},"name":"x".repeat(70*1024)}).to_string();
    let response = server
        .send(
            "POST",
            "/v1/sandboxes",
            Some(&token),
            Some("oversized-body-key"),
            oversized,
        )
        .await;
    assert_eq!(response.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body(&response)["code"], "payload_too_large");
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server.shutdown().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn provisioning_recovers_database_failure_and_refuses_changed_or_unsafe_files(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir();
    let path = dir.path().join("project.json");
    let offline = Store::connect("postgres://unused@127.0.0.1:1/unused", 1)
        .await
        .unwrap();
    assert!(provision(&offline, "recovery", &path).await.is_err());
    let original = std::fs::read(&path).unwrap();
    let store = Store::from_pool(pool.clone());
    let id = provision(&store, "recovery", &path).await.unwrap();
    assert_eq!(original, std::fs::read(&path).unwrap());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(provision(&store, "different", &path).await.is_err());
    sqlx::query("UPDATE projects SET limits='{}' WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(provision(&store, "recovery", &path).await.is_err());
    assert_eq!(original, std::fs::read(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(provision(&store, "recovery", &path).await.is_err());
    let partial = dir.path().join("partial.json");
    std::fs::write(&partial, b"{").unwrap();
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(provision(&store, "recovery", &partial).await.is_err());
    assert_eq!(std::fs::read(&partial).unwrap(), b"{");
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(provision(&store, "recovery", &link).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[tokio::test]
async fn stalled_response_body_and_stuck_handler_cannot_hold_connections_indefinitely() {
    let route = axum::routing::get(|| async {
        Body::from_stream(tokio_stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >())
    });
    let mut server = Server::start(
        Router::new().route("/", route),
        ServerLimits {
            connection: Duration::from_millis(150),
            ..Default::default()
        },
    )
    .await;
    let mut tls = server.tls().await;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut bytes = Vec::new();
    let _ = timeout(Duration::from_secs(1), tls.read_to_end(&mut bytes))
        .await
        .unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"));
    server.shutdown().await;

    let (started, mut received) = tokio::sync::mpsc::channel(1);
    let route = axum::routing::get(move || {
        let started = started.clone();
        async move {
            started.send(()).await.unwrap();
            std::future::pending::<&'static str>().await
        }
    });
    let mut server = Server::start(
        Router::new().route("/", route),
        ServerLimits {
            drain: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .await;
    let mut tls = server.tls().await;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    received.recv().await.unwrap();
    server.shutdown().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn chunked_and_stalled_bodies_are_bounded(pool: PgPool) {
    let dir = private_dir();
    let path = dir.path().join("project.json");
    provision(&Store::from_pool(pool.clone()), "chunked", &path)
        .await
        .unwrap();
    let (token, _) = token(&path);
    let mut server = Server::start(
        app(pool.clone()),
        ServerLimits {
            request: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .await;
    let mut tls = server.tls().await;
    let headers = format!(
        "POST /v1/sandboxes HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: chunked-body-key-1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(headers.as_bytes()).await.unwrap();
    let oversized = " ".repeat(70 * 1024);
    let _ = tls
        .write_all(format!("{:x}\r\n{oversized}\r\n0\r\n\r\n", oversized.len()).as_bytes())
        .await;
    let mut response = Vec::new();
    let _ = timeout(Duration::from_secs(1), tls.read_to_end(&mut response))
        .await
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 413"));
    let mut stalled = server.tls().await;
    stalled.write_all(headers.as_bytes()).await.unwrap();
    stalled.write_all(b"100\r\n{").await.unwrap();
    response.clear();
    let _ = timeout(Duration::from_secs(1), stalled.read_to_end(&mut response))
        .await
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 503"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server.shutdown().await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn binary_provisions_serves_and_stops_on_sigterm(pool: PgPool) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = private_dir();
    let path = dir.path().join("project.json");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.path().join("server.pem");
    let key_path = dir.path().join("server.key");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
    let original = std::env::var("DATABASE_URL").unwrap();
    let database_url = format!(
        "{}/{}",
        original.rsplit_once('/').unwrap().0,
        pool.connect_options().get_database().unwrap()
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sandbox-api"))
        .args(["provision-project", "--name", "binary", "--credential-file"])
        .arg(&path)
        .env("DATABASE_URL", &database_url)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (token, _) = token(&path);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&token));
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sandbox-api"))
        .args(["serve", "--bind", "127.0.0.1:0", "--tls-cert"])
        .arg(&cert_path)
        .arg("--tls-key")
        .arg(&key_path)
        .arg("--image-digest")
        .arg(format!("sha256:{}", "a".repeat(64)))
        .env("DATABASE_URL", &database_url)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let address = timeout(Duration::from_secs(5), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("listener startup log");
            if let Some((_, address)) = line.split_once("address=") {
                break address.trim().parse::<SocketAddr>().unwrap();
            }
        }
    })
    .await
    .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut tls = connector(roots)
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    tls.write_all(format!("GET /v1/sandboxes HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), tls.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let status = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().unwrap().to_string())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(TcpStream::connect(address).await.is_err());
}

#[test]
fn binary_configuration_fails_closed_and_redacts_database_errors() {
    let exe = env!("CARGO_BIN_EXE_sandbox-api");
    for args in [
        vec!["serve"],
        vec!["serve", "--bind", "0.0.0.0:8443"],
        vec!["serve", "--no-auth"],
        vec!["serve", "--http"],
    ] {
        assert!(
            !std::process::Command::new(exe)
                .args(args)
                .env_remove("DATABASE_URL")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let output = std::process::Command::new(exe)
        .arg("migrate")
        .env("DATABASE_URL", "not-a-url-DO_NOT_LOG_TEST_SECRET")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("DO_NOT_LOG_TEST_SECRET"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn provisioning_retry_is_independent_of_database_timezone_and_cannot_unsuspend(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let directory = private_dir();
    let path = directory.path().join("project.json");
    let store = Store::from_pool(pool.clone());
    let id = provision(&store, "timezone", &path).await.unwrap();
    let other_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    sqlx::query("SET TIME ZONE 'Pacific/Auckland'")
        .execute(&other_pool)
        .await
        .unwrap();
    let other = Store::from_pool(other_pool.clone());
    let (a, b) = tokio::join!(
        provision(&store, "timezone", &path),
        provision(&other, "timezone", &path)
    );
    assert_eq!(a.unwrap(), id);
    assert_eq!(b.unwrap(), id);
    sqlx::query("UPDATE projects SET status='suspended' WHERE id=$1")
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(provision(&other, "timezone", &path).await.is_err());
    let status: String = sqlx::query_scalar("SELECT status FROM projects WHERE id=$1")
        .bind(id.uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "suspended");
    let mut expired: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    expired["created_at"] = json!(0);
    expired["expires_at"] = json!(30 * 86400);
    std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
    assert!(provision(&store, "timezone", &path).await.is_err());
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let rejected = directory.path().join("public-parent.json");
    assert!(provision(&store, "unsafe parent", &rejected).await.is_err());
    assert!(!rejected.exists());
    other_pool.close().await;
}

#[path = "support/output.rs"]
mod output_fixture;
#[sqlx::test(migrator = "sandbox_store::MIGRATOR")]
#[ignore = "requires HUDSON_TEST_S3_* private MinIO and PostgreSQL"]
async fn output_minio_https_binary_read_revocation_missing_and_corrupt(pool: PgPool) {
    use object_store::ObjectStoreExt;
    use sandbox_protocol::output::OutputRefs;
    let f = output_fixture::Fixture::new(&pool).await;
    f.finish().await;
    let (claim, work) = f.work().await;
    let plans = output_fixture::plans(&work);
    f.store
        .save_output_plans(&claim, &plans, true)
        .await
        .unwrap();
    let env = |key| std::env::var(key).unwrap();
    let artifacts = sandbox_artifacts::S3Config {
        endpoint: env("HUDSON_TEST_S3_ENDPOINT"),
        region: "us-east-1".into(),
        bucket: env("HUDSON_TEST_S3_BUCKET"),
        access_key: env("HUDSON_TEST_S3_ACCESS_KEY"),
        secret_key: env("HUDSON_TEST_S3_SECRET_KEY"),
        session_token: None,
        allow_loopback_http: true,
    }
    .build()
    .unwrap();
    let now = || (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let refs = OutputRefs {
        stdout: artifacts
            .upload(&plans.stdout, &work.ticket.owner, now(), b"a\x00b\xff")
            .await
            .unwrap(),
        stderr: artifacts
            .upload(&plans.stderr, &work.ticket.owner, now(), b"err")
            .await
            .unwrap(),
    };
    f.store.publish_output(&claim, &refs, true).await.unwrap();
    let mut server = Server::start(f.app(Some(Arc::new(artifacts))), ServerLimits::default()).await;
    let path = format!("/v1/operations/{}/outputs/stdout", f.operation);
    let r = server
        .send("GET", &path, Some(&f.token), None, String::new())
        .await;
    assert_eq!(r.0, StatusCode::OK);
    assert_eq!(r.2, b"a\x00b\xff");
    assert_eq!(r.1["cache-control"], "no-store");
    assert_eq!(r.1["x-output-truncated"], "true");
    let r = server
        .send(
            "GET",
            &format!("{path}?offset=2&limit=1"),
            Some(&f.token),
            None,
            String::new(),
        )
        .await;
    assert_eq!(r.2, b"b");
    assert_eq!(r.1["x-output-next-offset"], "3");
    assert_eq!(r.1["x-output-eof"], "false");
    let r = server.send("GET", &path, None, None, String::new()).await;
    assert_eq!(r.0, StatusCode::UNAUTHORIZED);
    // Test-only destructive client, scoped to this test's random object keys.
    let raw = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(env("HUDSON_TEST_S3_ENDPOINT"))
        .with_region("us-east-1")
        .with_bucket_name(env("HUDSON_TEST_S3_BUCKET"))
        .with_access_key_id(env("HUDSON_TEST_S3_ACCESS_KEY"))
        .with_secret_access_key(env("HUDSON_TEST_S3_SECRET_KEY"))
        .with_allow_http(true)
        .build()
        .unwrap();
    let key = object_store::path::Path::from(plans.stdout.object_key().unwrap());
    raw.put(&key, b"corrupt".to_vec().into()).await.unwrap();
    let r = server
        .send("GET", &path, Some(&f.token), None, String::new())
        .await;
    assert_eq!(r.0, StatusCode::BAD_GATEWAY);
    assert_eq!(body(&r)["code"], "output_corrupt");
    raw.delete(&key).await.unwrap();
    let r = server
        .send("GET", &path, Some(&f.token), None, String::new())
        .await;
    assert_eq!(r.0, StatusCode::GONE);
    assert_eq!(body(&r)["code"], "output_missing");
    sqlx::query("UPDATE projects SET api_tokens='[]' WHERE id=$1")
        .bind(f.project.uuid())
        .execute(&pool)
        .await
        .unwrap();
    let r = server
        .send("GET", &path, Some(&f.token), None, String::new())
        .await;
    assert_eq!(r.0, StatusCode::UNAUTHORIZED);
    raw.delete(&object_store::path::Path::from(
        plans.stderr.object_key().unwrap(),
    ))
    .await
    .unwrap();
    server.shutdown().await;
}
