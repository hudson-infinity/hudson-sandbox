#![allow(dead_code, clippy::unwrap_used)]
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose,
};
use sandbox_protocol::{
    AllocationId, Id,
    guest_wire::{ClientTls, ServerTls, guest_name},
};
use sha2::{Digest, Sha256};
pub(crate) struct Leaf {
    pub(crate) cert: Certificate,
    pub(crate) key: KeyPair,
}
impl Leaf {
    pub(crate) fn new(ca: &CertifiedIssuer<'_, KeyPair>, name: String, client: bool) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(vec![name]).unwrap();
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![if client {
            ExtendedKeyUsagePurpose::ClientAuth
        } else {
            ExtendedKeyUsagePurpose::ServerAuth
        }];
        Self {
            cert: p.signed_by(&key, ca).unwrap(),
            key,
        }
    }
    pub(crate) fn pin(&self) -> [u8; 32] {
        Sha256::digest(self.cert.der()).into()
    }
}
pub(crate) struct Fixture {
    pub(crate) allocation: AllocationId,
    pub(crate) ca: CertifiedIssuer<'static, KeyPair>,
    pub(crate) host: Leaf,
    pub(crate) guest: Leaf,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let mut p = CertificateParams::new(Vec::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap();
        let allocation = AllocationId::generate();
        let host = Leaf::new(&ca, "host.sandbox.internal".into(), true);
        let guest = Leaf::new(&ca, guest_name(allocation), false);
        Self {
            allocation,
            ca,
            host,
            guest,
        }
    }
    pub(crate) fn server(&self) -> ServerTls {
        ServerTls::new(
            self.ca.pem().as_bytes(),
            self.guest.cert.pem().as_bytes(),
            self.guest.key.serialize_pem().as_bytes(),
            self.host.pin(),
        )
        .unwrap()
    }
    pub(crate) fn client_for(
        &self,
        host: &Leaf,
        pin: [u8; 32],
        allocation: AllocationId,
    ) -> ClientTls {
        ClientTls::new(
            self.ca.pem().as_bytes(),
            host.cert.pem().as_bytes(),
            host.key.serialize_pem().as_bytes(),
            pin,
            allocation,
        )
        .unwrap()
    }
    pub(crate) fn client(&self) -> ClientTls {
        self.client_for(&self.host, self.guest.pin(), self.allocation)
    }
}
