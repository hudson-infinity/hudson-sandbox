//! Allocation-specific channel credentials. Ephemeral issuer keys are never persisted.
use anyhow::{Result, ensure};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use sandbox_protocol::{
    AllocationId,
    bootstrap::Bootstrap,
    guest_wire::{ClientTls, guest_name},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub guest: Bootstrap,
    host_cert_pem: String,
    host_key_pem: String,
    guest_pin: [u8; 32],
}
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("allocation", &self.guest.allocation)
            .finish_non_exhaustive()
    }
}
impl Identity {
    pub fn issue(allocation: AllocationId, generation: i64, now_ms: i64) -> Result<Self> {
        ensure!(generation > 0 && now_ms > 0, "invalid allocation identity");
        let now = OffsetDateTime::from_unix_timestamp(now_ms / 1000)?;
        let until = now
            .checked_add(Duration::hours(24))
            .ok_or_else(|| anyhow::anyhow!("certificate clock overflow"))?;
        let mut params = CertificateParams::new(Vec::new())?;
        params.not_before = now - Duration::minutes(1);
        params.not_after = until;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate()?)?;
        let leaf = |name: String, client: bool| -> Result<_> {
            let key = KeyPair::generate()?;
            let mut p = CertificateParams::new(vec![name])?;
            p.not_before = now - Duration::minutes(1);
            p.not_after = until;
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.extended_key_usages = vec![if client {
                ExtendedKeyUsagePurpose::ClientAuth
            } else {
                ExtendedKeyUsagePurpose::ServerAuth
            }];
            Ok((p.signed_by(&key, &ca)?, key))
        };
        let (host, host_key) = leaf(format!("host-{}", guest_name(allocation)), true)?;
        let (guest, guest_key) = leaf(guest_name(allocation), false)?;
        let identity = Self {
            guest: Bootstrap {
                version: 1,
                allocation,
                generation,
                valid_until_unix_ms: until.unix_timestamp() * 1000,
                ca_pem: ca.pem(),
                guest_cert_pem: guest.pem(),
                guest_key_pem: guest_key.serialize_pem(),
                host_pin: Sha256::digest(host.der()).into(),
            },
            host_cert_pem: host.pem(),
            host_key_pem: host_key.serialize_pem(),
            guest_pin: Sha256::digest(guest.der()).into(),
        };
        // Construct both endpoints now; issuance must never persist unusable key pairs.
        identity.guest.server_tls(now_ms)?;
        identity.client_tls(now_ms)?;
        Ok(identity)
    }
    pub fn client_tls(&self, now_ms: i64) -> Result<ClientTls> {
        self.guest.validate()?;
        ensure!(
            now_ms < self.guest.valid_until_unix_ms,
            "allocation channel identity expired"
        );
        ensure!(
            self.host_cert_pem.len() <= 16384 && self.host_key_pem.len() <= 16384,
            "invalid host credential size"
        );
        ClientTls::new(
            self.guest.ca_pem.as_bytes(),
            self.host_cert_pem.as_bytes(),
            self.host_key_pem.as_bytes(),
            self.guest_pin,
            self.guest.allocation,
        )
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use sandbox_protocol::Id;
    #[tokio::test]
    async fn issued_channel_authenticates_and_rejects_other_allocations() {
        let now = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let identity = Identity::issue(AllocationId::generate(), 1, now).unwrap();
        let other = Identity::issue(AllocationId::generate(), 1, now).unwrap();
        let server = identity.guest.server_tls(now).unwrap();
        let client = identity.client_tls(now).unwrap();
        let (a, b) = tokio::io::duplex(65536);
        let (a, b) = tokio::join!(server.accept(a), client.connect(b));
        assert!(a.is_ok() && b.is_ok());
        let wrong = other.client_tls(now).unwrap();
        let (a, b) = tokio::io::duplex(65536);
        let (a, b) = tokio::join!(server.accept(a), wrong.connect(b));
        assert!(a.is_err() && b.is_err());
        let until = identity.guest.valid_until_unix_ms;
        assert!(identity.client_tls(until).is_err());
        assert!(identity.guest.server_tls(until).is_err());
        let serialized = serde_json::to_value(&identity.guest).unwrap();
        assert!(serialized.get("host_key_pem").is_none());
        assert_ne!(identity.guest.guest_key_pem, identity.host_key_pem);
        assert!(!format!("{identity:?}").contains("PRIVATE KEY"));
    }
}
