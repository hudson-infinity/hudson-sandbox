//! Test-only subprocess bridge; no optional skip when SDK setup is absent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
pub(crate) async fn run(language: &str, config: &Path, case: &Value) -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut input = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut input, &serde_json::to_vec(case).unwrap()).unwrap();
    let mut command = if language == "python" {
        let python = std::env::var_os("HUDSON_SDK_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".venv-sdk/bin/python"));
        let mut cmd = tokio::process::Command::new(python);
        cmd.arg(root.join("sdk/tests/python_runner.py"));
        cmd.env("PYTHONPATH", root.join("sdk/python/src"));
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("node");
        cmd.arg(root.join("sdk/tests/typescript_runner.mjs"));
        cmd
    };
    command.arg(config).arg(input.path()).kill_on_drop(true);
    command
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .env("NODE_USE_ENV_PROXY", "1")
        .env("NODE_TLS_REJECT_UNAUTHORIZED", "0");
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("SDK subprocess hung")
        .expect("run make sdk-setup first; SDK conformance may not silently skip");
    assert!(
        output.status.success(),
        "{language}: {}: {}",
        case["name"],
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{language}: {}: invalid output: {}",
            case["name"],
            String::from_utf8_lossy(&output.stdout)
        )
    })
}
