# Running the HTTPS API

Status: implemented for create, execute, cancel, destroy, status, collections, outputs, streams, file uploads and captured downloads. The [API binary](../crates/sandbox-api/src/main.rs) now accepts real HTTPS connections. The [controller](controller.md) remains a separate process. An installer, SDK, public client CLI and management API/UI remain unfinished. This setup is not a production isolation guarantee.

## Transport contract

`sandbox-api serve` requires a PEM certificate chain, a matching PEM private key, `DATABASE_URL` in its environment, and one or more `--image-digest` arguments. The [image policy](api-contract.md#implemented-image-admission) is mandatory and immutable until restart. TLS material and image configuration are checked before database access; migrations must finish before the socket is bound. Bad configuration or unavailable storage exits with a nonzero status. No plaintext listener or authentication bypass exists.

The default bind address is `127.0.0.1:8443`. `--bind` can select a different IP address and port. A non-loopback bind is an explicit operator choice; use a certificate valid for the service hostname and configure host access controls before exposing it. The server supports HTTP/1.1 over TLS 1.2 or 1.3. It does not trust forwarded identity headers or accept client certificates as project credentials: the existing bearer-token checks run on every request.

The HTTPS transport has fixed bounds; SSE adds its own body/session limits:

| Resource | Bound and behavior |
| --- | --- |
| Connections, including incomplete TLS handshakes | 128; additional sockets are closed without queuing tasks |
| TLS handshake | 5 seconds |
| HTTP request headers | 10 seconds; at most 64 headers and a 32 KiB parser buffer |
| JSON body | 64 KiB, including chunked bodies; `413 payload_too_large` |
| Handler, including authentication and reading the body | 30 seconds; `503 unavailable` on expiry |
| Whole connection, including a stalled response writer | 120 seconds |
| Shutdown drain | Stop accepting on SIGINT/SIGTERM; finish active requests for up to 10 seconds, then close remaining connections |

The [transport implementation](../crates/sandbox-api/src/server.rs) uses Rustls and [Hyper's HTTP/1 connection builder](https://docs.rs/hyper/1.11.1/hyper/server/conn/http1/struct.Builder.html). These bounds are not per-project rate limits, admission quotas, a load benchmark, or protection against every denial-of-service attack. Streaming endpoints will require a separate lifetime and revocation policy.

A timeout, disconnect, or forced shutdown can occur after a database commit. Recover a mutation using its original idempotency key and payload; do not infer cancellation from a transport failure. JSON extraction failures return generic `400 bad_request` problems; body-limit failures return `413` problems. Protocol-level failures such as malformed HTTP headers can close the connection or use Hyper's plain protocol error response. API responses do not include raw parser input. Logs omit request headers, bodies, query strings, and database configuration; the binary deliberately does not enable wire-level debug logging from `RUST_LOG`.

## Offline project provisioning

Until the planned Admin APIs exist, an operator with direct database access can run `sandbox-api provision-project --name NAME --credential-file PATH`. This command is an offline deployment tool, not a public client command or unauthenticated HTTP endpoint. The [authentication contract](auth-design.md#implemented-offline-project-provisioning) owns its scope and retry behavior.

The credential directory must be private on Unix, normally mode `0700`. The command creates a new mode `0600` JSON file containing a project ID and a 30-day bearer token, syncs the file and directory, and only then attempts the project insert. PostgreSQL receives hash-only token metadata. Stdout contains the project ID and file path, never the token. Existing files must be private regular files, at most 4096 bytes; symlinks are refused.

The initial project receives explicit allocation quotas matching the current placement defaults: 25 sandboxes, 100 vCPUs, 204800 MiB memory, and 1638400 MiB writable disk. This command does not add a pending-operation admission quota. Quota enforcement and admission behavior remain owned by the store/controller contracts.

Retry the same command with the same file after any uncertain result. It verifies the same ID, name, credential metadata, active status, and quotas. It never overwrites an existing project, replaces a token, renews expiry, unsuspends a project, or reverses revocation. If a project was legitimately changed, provisioning reports a conflict. An expired or malformed file is refused and retained for operator investigation. There is no atomic filesystem/database transaction: a crash can leave a credential file without a project. The retained file is the recovery input; deleting it and running with a fresh path can create a second project. Use an operator-owned private directory and retain the file until the result is confirmed.

## Local control-plane example

Prerequisites: the pinned Rust toolchain, `protoc`, Python 3, OpenSSL, and the [local PostgreSQL stack](implementation/dev-env.md). Run `make up` and configure `DATABASE_URL` from `.env.example`; the binary does not load `.env` automatically. This example uses a synthetic image digest to exercise admission. No runnable image is shipped and no customer command runs.

Create private local configuration outside the repository:

```sh
export SANDBOX_DEV_DIR="$HOME/.local/share/hudson-sandbox-dev"
umask 077
mkdir -p "$SANDBOX_DEV_DIR"
chmod 700 "$SANDBOX_DEV_DIR"
cat > "$SANDBOX_DEV_DIR/tls.cnf" <<'TLS'
[req]
prompt = no
distinguished_name = dn
x509_extensions = extensions
[dn]
CN = localhost
[extensions]
subjectAltName = DNS:localhost,IP:127.0.0.1
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
TLS
openssl req -x509 -newkey rsa:3072 -nodes -days 7 \
  -config "$SANDBOX_DEV_DIR/tls.cnf" \
  -keyout "$SANDBOX_DEV_DIR/server.key" \
  -out "$SANDBOX_DEV_DIR/server.pem"

cargo run -p sandbox-api -- provision-project \
  --name local-project \
  --credential-file "$SANDBOX_DEV_DIR/project.json"

cargo run -p sandbox-api -- serve \
  --tls-cert "$SANDBOX_DEV_DIR/server.pem" \
  --tls-key "$SANDBOX_DEV_DIR/server.key" \
  --image-digest sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

In another terminal with `SANDBOX_DEV_DIR` set, this client trusts that development certificate explicitly and reads the bearer token from its file without placing it in a shell argument or printing it:

```python
import json
import os
import pathlib
import ssl
import urllib.request

config = pathlib.Path(os.environ["SANDBOX_DEV_DIR"])
credential = json.loads((config / "project.json").read_text())
tls = ssl.create_default_context(cafile=str(config / "server.pem"))
request = urllib.request.Request(
    "https://localhost:8443/v1/sandboxes",
    headers={"Authorization": "Bearer " + credential["token"]},
)
with urllib.request.urlopen(request, context=tls, timeout=10) as response:
    print(response.read().decode())
```

An empty project returns `{"items":[],"next_cursor":null}`. Use the [implemented API routes](api-contract.md#example-and-resource-routes) for create/status/destroy. Create remains queued until a separately configured controller can place it; a fake supervisor always reports simulated observations. The TLS tests seed an uncertain create to exercise destroy admission without starting a controller or claiming a VM ran.

## Migrations, certificates, and verification

Both `serve` and `provision-project` run embedded migrations before proceeding. `sandbox-api migrate` runs them without opening a listener. Coordinate schema upgrades across operators and services; migration 0004 builds ordinary indexes and can block writes during construction. There is no automatic rollback or zero-downtime upgrade claim.

Certificates and keys are read once at startup. Replace them and restart to rotate; automatic renewal and reload are not implemented. The seven-day certificate above is for local development only. Do not disable certificate verification in clients.

[Transport and provisioning tests](../crates/sandbox-api/tests/server.rs) exercise real TCP/TLS, trusted/untrusted certificates, hostname verification, plaintext rejection, create retry, reads/lists, destroy conflict and cleanup admission, revocation, body/time/connection bounds, draining, provisioning recovery, private-file checks, database-error redaction, and the actual binary's SIGTERM path. Existing [authentication tests](../crates/sandbox-api/src/auth.rs) and controller tests cover their own layers. These are control-plane tests, not Firecracker isolation evidence.


## Private output reads

Add `--output-config /absolute/path/output.json` to `sandbox-api serve` to configure the [retained-output endpoint](api-contract.md#implemented-retained-output-reads). Use a service-owned private file and separate read-only object-storage credentials as described in [output storage](output-storage.md#api-reader-configuration-and-evidence). The API reads the same private bucket as the supervisor; it does not receive storage location or credentials from customers. The existing TLS, bearer authentication and request/connection bounds still apply. The separate [SSE endpoint](api-contract.md#implemented-output-streams) can use these archived objects and has its own session, queue and lifetime limits.

## Live-output host configuration

To enable live guest reads, add all of `--live-host-id`, `--live-endpoint`, `--live-ca-cert`, `--live-client-cert`, and `--live-client-key` to `sandbox-api serve`. The endpoint must be HTTPS and match the configured host's certificate identity. TLS file reads are bounded to 64 KiB. Provision these files in operator-controlled directories, keep the private key readable only by the API service, and restart to rotate them. The API uses this one configured host; HTTP callers cannot supply or override routing or certificates. Existing routes and archived streaming can run without live-host configuration.

Pin the API's **dedicated reader leaf certificate** with `--output-reader-cert-sha256` on `sandbox-host` or `sandbox-fake-host`. It must differ from every controller certificate, including rotation pins. This API client calls only the [read-only service](supervisor-protocol.md#read-only-live-output). It reconnects using normal CA/host-name verification; no controller credential, mutation method, plaintext fallback or guest endpoint is exposed to customers.

Simulated host observations are rejected by default. `--allow-simulated-live` is an explicit development opt-in and requires live-host configuration; it never turns simulated evidence into a real VM claim. All frames retain the provenance checked against the execution record. Configure `--output-config` as well to serve published history after guest destruction. File transfer, fleet provisioning, browser sessions and production release packaging remain separate work.

## File-reader configuration

Enable the [public capture/download/release routes](api-contract.md#implemented-file-downloads) with all five flags: `--file-host-id`, `--file-endpoint`, `--file-ca-cert`, `--file-client-cert`, `--file-client-key`. The endpoint must be a fixed HTTPS authority with no user information, query or non-root path. The host ID selects the expected TLS server identity. Each PEM input is bounded to 64 KiB. Configure the matching certificate fingerprint on the supervisor with `--file-reader-cert-sha256`; controller and output-reader certificates do not implicitly grant file access.

This single-host mapping is operator configuration. Customer IDs, paths and capture descriptors never choose an endpoint or credential file. A different allocation host returns unavailable until an operator configures a reader for it. The service cannot upload files or dispatch commands. No file-reader configuration means file requests for an otherwise readable sandbox return `503 unavailable`.

Simulated replies require explicit `--allow-simulated-files` and must also match the current sandbox's recorded simulation provenance. Leave it disabled for real hosts. Capture descriptors, bearer headers, workspace paths and file contents must not be added to access logs. The default server logging policy already excludes request headers, query strings and bodies.

## File source configuration

`--file-source-config /absolute/private/file-sources.json` enables public upload ingestion. The controller must also receive `--file-source-config` pointing to the same private bucket/endpoint. The file uses the existing [S3 configuration](output-storage.md) shape (`endpoint`, `region`, `bucket`, `access_key`, `secret_key`, optional explicit loopback HTTP for development) and must be an owner-only regular file. The API needs conditional source PUT/GET permissions; the controller needs source GET/version GET. Use separate least-privilege credentials. Source credentials do not grant supervisor access; the controller retains its separate mTLS mutation identity. No raw object keys or presigned URLs are accepted from customers.

Without this flag, uploads return `503`. Capture/download configuration remains independent. The upload route explicitly bounds its binary body at 8 MiB rather than the ordinary 64 KiB JSON extractor limit. Ingestion has four process-wide slots, a ten-second body deadline and a 15-second source-write deadline, within the normal 30-second request deadline. Configure matching controller storage before accepting uploads; admission alone cannot establish that a worker is running. Enable the separate [source cleanup worker](file-transfer.md#source-cleanup-worker) to retire expired bytes and reclaim their global/project source-byte charge. It retains operation counts and guest byte reservations. API configuration alone does not enable cleanup.
