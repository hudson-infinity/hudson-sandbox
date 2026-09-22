//! Local self-signed HTTPS fixture. Test credentials only; no runtime evidence.
#![allow(dead_code, clippy::unwrap_used)]
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
pub(crate) struct Server {
    pub(crate) config: PathBuf,
    pub(crate) directory: tempfile::TempDir,
    pub(crate) endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub(crate) fn private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
impl Server {
    pub(crate) async fn start(app: axum::Router) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let acceptor = sandbox_api::server::tls_acceptor(
            cert.cert.pem().as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        private(&directory.path().join("ca.pem"), cert.cert.pem().as_bytes());
        private(&directory.path().join("credential.json"), &serde_json::to_vec(&json!({
            "version":1,"project_id":"prj_test", "name":"fixture", "token":"test-private-credential",
            "created_at":1, "expires_at":4102444800_i64
        })).unwrap());
        let config = directory.path().join("client.json");
        private(&config, &serde_json::to_vec(&json!({"version":1,"endpoint":endpoint,"credential_file":"credential.json", "ca_file":"ca.pem","request_timeout_seconds":1})).unwrap());
        let task = tokio::spawn(async move {
            let _ = sandbox_api::server::serve(
                listener,
                acceptor,
                app,
                Default::default(),
                std::future::pending(),
            )
            .await;
        });
        Self {
            config,
            directory,
            endpoint,
            task,
        }
    }
    pub(crate) fn client(&self) -> sandbox_client::Client {
        sandbox_client::Client::from_config(&self.config).unwrap()
    }
    pub(crate) fn edit_config(&self, mutate: impl FnOnce(&mut Value)) {
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&self.config).unwrap()).unwrap();
        mutate(&mut config);
        private(&self.config, &serde_json::to_vec(&config).unwrap());
    }
}
