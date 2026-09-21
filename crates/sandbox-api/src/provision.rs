//! Local operator credential delivery. Files are never printed, overwritten, or regenerated on retry.
use anyhow::{Context, ensure};
use sandbox_protocol::{Id, ProjectId, ProjectToken};
use sandbox_store::{Store, provision::ProjectProvision};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};
use time::OffsetDateTime;

// Deliberately no Debug: this is the only durable plaintext credential copy.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    version: u8,
    project_id: ProjectId,
    name: String,
    token: String,
    created_at: i64,
    expires_at: i64,
}

/// Provision via a private operator-owned directory on Unix. An existing file resumes the same
/// intent; after any uncertain DB result the operator must retry this file, not mint another one.
pub async fn provision(store: &Store, name: &str, path: &Path) -> anyhow::Result<ProjectId> {
    ensure!(
        !name.trim().is_empty() && name.len() <= 200 && !name.chars().any(char::is_control),
        "project name must contain 1-200 bytes without control characters"
    );
    let credential = credential(name, path)?;
    ensure!(
        credential.version == 1 && credential.name == name,
        "credential file does not match this provisioning request"
    );
    let token = ProjectToken::parse(&credential.token)
        .map_err(|_| anyhow::anyhow!("invalid credential file token"))?;
    let created_at = OffsetDateTime::from_unix_timestamp(credential.created_at)
        .context("invalid credential creation time")?;
    let expires_at = OffsetDateTime::from_unix_timestamp(credential.expires_at)
        .context("invalid credential expiry time")?;
    ensure!(
        expires_at > OffsetDateTime::now_utc()
            && expires_at - created_at == time::Duration::days(30),
        "credential has expired or has invalid lifetime"
    );
    let matches = store
        .provision_project(&ProjectProvision {
            project: credential.project_id,
            name: credential.name,
            key: token.key_id().clone(),
            hash: token.hash(),
            created_at,
            expires_at,
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "project provisioning was not confirmed; retry with the same credential file"
            )
        })?;
    ensure!(
        matches,
        "existing project differs from the credential file; no existing data was changed"
    );
    Ok(credential.project_id)
}

#[cfg(unix)]
fn credential(name: &str, path: &Path) -> anyhow::Result<Credential> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = std::fs::metadata(parent).context("reading credential directory")?;
    ensure!(
        directory.is_dir() && directory.permissions().mode() & 0o077 == 0,
        "credential directory must be private (mode 0700)"
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata =
                std::fs::symlink_metadata(path).context("reading credential metadata")?;
            ensure!(
                metadata.is_file()
                    && metadata.permissions().mode() & 0o077 == 0
                    && metadata.len() <= 4096,
                "credential must be a private regular file of at most 4096 bytes"
            );
            let file = File::open(path).context("opening credential file")?;
            let mut bytes = Vec::new();
            (&file)
                .take(4097)
                .read_to_end(&mut bytes)
                .context("reading credential file")?;
            ensure!(bytes.len() <= 4096, "credential file is too large");
            file.sync_all()
                .context("syncing existing credential file")?;
            File::open(parent)
                .and_then(|dir| dir.sync_all())
                .context("syncing credential directory")?;
            return serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid credential file; it was not overwritten"));
        }
        Err(_) => anyhow::bail!("cannot create credential file"),
    };
    let created_at = OffsetDateTime::now_utc().unix_timestamp();
    let token = ProjectToken::generate()
        .map_err(|_| anyhow::anyhow!("cannot generate project credential"))?;
    let credential = Credential {
        version: 1,
        project_id: ProjectId::generate(),
        name: name.to_owned(),
        token: token.render_once(),
        created_at,
        expires_at: created_at + 30 * 86400,
    };
    let bytes = serde_json::to_vec(&credential).context("encoding credential file")?;
    file.write_all(&bytes).context("writing credential file")?;
    file.write_all(b"\n").context("finishing credential file")?;
    file.sync_all().context("syncing credential file")?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .context("syncing credential directory")?;
    Ok(credential)
}
#[cfg(not(unix))]
fn credential(_name: &str, _path: &Path) -> anyhow::Result<Credential> {
    anyhow::bail!("offline provisioning requires a Unix filesystem with private permissions")
}
