use crate::{ArtifactStore, Error};
use object_store::{
    ClientOptions, RetryConfig,
    aws::{AmazonS3Builder, S3ConditionalPut},
    client::{HttpClient, HttpConnector},
};
use std::{fmt, sync::Arc, time::Duration};
use url::{Host, Url};

/// Operator configuration. Never deserialize this from a customer request.
/// Credentials must be scoped to a private output bucket by the operator.
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub allow_loopback_http: bool,
}
impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config").finish_non_exhaustive()
    }
}

// The client does not follow redirects, use ambient proxies/credentials, or
// retry writes automatically. Keep request bodies in this runtime: the spawned
// object_store connector uses an unbounded response channel.
#[derive(Debug)]
struct Connector(reqwest::Client);
impl HttpConnector for Connector {
    fn connect(&self, _: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(self.0.clone()))
    }
}

impl S3Config {
    fn validate(&self) -> Result<Url, Error> {
        let url = Url::parse(&self.endpoint).map_err(|_| Error::InvalidConfig)?;
        let loopback = match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        let bucket_valid = (3..=63).contains(&self.bucket.len())
            && self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && self.bucket.starts_with(|c: char| c.is_ascii_alphanumeric())
            && self.bucket.ends_with(|c: char| c.is_ascii_alphanumeric());
        if url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http" && self.allow_loopback_http && loopback))
            || !bucket_valid
            || self.region.is_empty()
            || self.region.len() > 64
            || !self
                .region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.access_key.is_empty()
            || self.secret_key.is_empty()
            || [&self.access_key, &self.secret_key]
                .into_iter()
                .chain(self.session_token.iter())
                .any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err(Error::InvalidConfig);
        }
        Ok(url)
    }

    pub fn build(self) -> Result<ArtifactStore, Error> {
        let endpoint = self.validate()?;
        // reqwest's no-provider feature requires an installed provider even
        // when ring is the only compiled one. Preserve any process default
        // already chosen by the embedding service.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::InvalidConfig)?;
        let mut builder = AmazonS3Builder::new()
            .with_endpoint(endpoint.as_str())
            .with_region(self.region)
            .with_bucket_name(self.bucket)
            .with_access_key_id(self.access_key)
            .with_secret_access_key(self.secret_key)
            .with_allow_http(endpoint.scheme() == "http")
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .with_http_connector(Connector(client))
            .with_retry(RetryConfig {
                max_retries: 0,
                ..Default::default()
            });
        if let Some(token) = self.session_token {
            builder = builder.with_token(token);
        }
        let store = builder.build().map_err(|_| Error::InvalidConfig)?;
        Ok(ArtifactStore {
            inner: Arc::new(store),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> S3Config {
        S3Config {
            endpoint: "https://objects.example.test".into(),
            region: "us-east-1".into(),
            bucket: "output-test".into(),
            access_key: "private-access".into(),
            secret_key: "private-secret".into(),
            session_token: None,
            allow_loopback_http: false,
        }
    }
    #[test]
    fn endpoints_require_tls_or_explicit_literal_loopback() {
        for endpoint in [
            "http://objects.example.test",
            "http://localhost:9000",
            "https://user:secret@example.test",
            "https://example.test/bucket",
            "https://example.test/?secret=x",
            "https://example.test/#x",
            "file:///tmp/output",
        ] {
            let mut c = config();
            c.endpoint = endpoint.into();
            c.allow_loopback_http = true;
            assert!(c.validate().is_err(), "accepted {endpoint}");
        }
        for endpoint in ["http://127.0.0.1:9000", "http://[::1]:9000"] {
            let mut c = config();
            c.endpoint = endpoint.into();
            assert!(c.validate().is_err());
            c.allow_loopback_http = true;
            assert!(c.validate().is_ok());
        }
        assert!(config().build().is_ok()); // Also catches ambiguous TLS providers.
    }
    #[test]
    fn config_debug_and_errors_do_not_include_credentials() {
        let mut c = config();
        assert_eq!(format!("{c:?}"), "S3Config { .. }");
        c.secret_key = String::new();
        assert!(matches!(c.build(), Err(Error::InvalidConfig)));
    }
}
