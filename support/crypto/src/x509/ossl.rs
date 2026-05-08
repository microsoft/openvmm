// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::X509Error;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> X509Error {
    X509Error(crate::BackendError(err, op))
}

pub struct X509CertificateInner(pub(crate) openssl::x509::X509);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert =
            openssl::x509::X509::from_der(data).map_err(|e| err(e, "parsing DER certificate"))?;
        Ok(Self(cert))
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, X509Error> {
        let pkey = self
            .0
            .public_key()
            .map_err(|e| err(e, "extracting public key"))?;
        // Currently we only expect RSA public keys, so verify the type.
        // If someday we need to support other public key types, the return
        // type of this function will need to change.
        pkey.rsa()
            .map_err(|e| err(e, "extracting RSA public key"))?;
        Ok(crate::rsa::RsaPublicKey(
            crate::rsa::ossl::RsaPublicKeyInner(pkey),
        ))
    }

    pub fn verify(&self, issuer_public_key: &crate::rsa::RsaPublicKey) -> Result<bool, X509Error> {
        self.0
            .verify(&issuer_public_key.0.0)
            .map_err(|e| err(e, "verifying certificate signature"))
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> bool {
        let result = self.0.issued(&subject.0);
        result == openssl::x509::X509VerifyResult::OK
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.0
            .to_der()
            .map_err(|e| err(e, "encoding certificate as DER"))
    }

    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        let mut builder = openssl::x509::X509::builder()?;
        builder.set_version(2)?;
        let serial = openssl::bn::BigNum::from_u32(1)?.to_asn1_integer()?;
        builder.set_serial_number(&serial)?;
        builder.set_pubkey(&key.0.0)?;
        let mut name = openssl::x509::X509Name::builder()?;
        name.append_entry_by_text("C", country)?;
        name.append_entry_by_text("ST", state)?;
        name.append_entry_by_text("L", locality)?;
        name.append_entry_by_text("O", organization)?;
        name.append_entry_by_text("CN", common_name)?;
        let name = name.build();
        builder.set_subject_name(&name)?;
        builder.set_issuer_name(&name)?;
        let not_before = openssl::asn1::Asn1Time::from_unix(0)?;
        builder.set_not_before(&not_before)?;
        let not_after = openssl::asn1::Asn1Time::from_unix(i32::MAX.into())?;
        builder.set_not_after(&not_after)?;
        builder.sign(&key.0.0, openssl::hash::MessageDigest::sha256())?;
        Ok(X509CertificateInner(builder.build()))
    }
}
