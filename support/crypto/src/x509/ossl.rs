// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::X509Error;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> X509Error {
    X509Error(crate::BackendError(err, op))
}

pub struct X509CertificateInner {
    pub(crate) cert: openssl::x509::X509,
}

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert =
            openssl::x509::X509::from_der(data).map_err(|e| err(e, "parsing DER certificate"))?;
        Ok(Self { cert })
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, X509Error> {
        let pkey = self
            .cert
            .public_key()
            .map_err(|e| err(e, "extracting public key"))?;
        assert_eq!(pkey.id(), openssl::pkey::Id::RSA);
        Ok(crate::rsa::RsaPublicKey(
            crate::rsa::ossl::RsaPublicKeyInner::from_pkey(pkey),
        ))
    }

    pub fn verify(&self, issuer_public_key: &crate::rsa::RsaPublicKey) -> Result<bool, X509Error> {
        self.cert
            .verify(&issuer_public_key.0.pkey)
            .map_err(|e| err(e, "verifying certificate signature"))
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> bool {
        let result = self.cert.issued(&subject.cert);
        result == openssl::x509::X509VerifyResult::OK
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.cert
            .to_der()
            .map_err(|e| err(e, "encoding certificate as DER"))
    }
}

pub struct X509BuilderInner {
    builder: openssl::x509::X509Builder,
}

impl X509BuilderInner {
    pub fn new() -> Result<Self, X509Error> {
        let mut builder =
            openssl::x509::X509::builder().map_err(|e| err(e, "creating X509 builder"))?;
        builder
            .set_version(2)
            .map_err(|e| err(e, "setting version"))?;
        let serial = openssl::bn::BigNum::from_u32(1)
            .map_err(|e| err(e, "creating serial number"))?
            .to_asn1_integer()
            .map_err(|e| err(e, "converting serial number"))?;
        builder
            .set_serial_number(&serial)
            .map_err(|e| err(e, "setting serial number"))?;
        Ok(Self { builder })
    }

    pub fn set_pubkey_from_rsa_key_pair(
        &mut self,
        key_pair: &crate::rsa::RsaKeyPair,
    ) -> Result<(), X509Error> {
        let pkey = openssl::pkey::PKey::from_rsa(key_pair.0.rsa.clone())
            .map_err(|e| err(e, "converting RSA key to PKey"))?;
        self.builder
            .set_pubkey(&pkey)
            .map_err(|e| err(e, "setting public key"))
    }

    pub fn set_subject_and_issuer_name(
        &mut self,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> Result<(), X509Error> {
        let mut name =
            openssl::x509::X509Name::builder().map_err(|e| err(e, "creating X509Name builder"))?;
        name.append_entry_by_text("C", country)
            .map_err(|e| err(e, "setting country"))?;
        name.append_entry_by_text("ST", state)
            .map_err(|e| err(e, "setting state"))?;
        name.append_entry_by_text("L", locality)
            .map_err(|e| err(e, "setting locality"))?;
        name.append_entry_by_text("O", organization)
            .map_err(|e| err(e, "setting organization"))?;
        name.append_entry_by_text("CN", common_name)
            .map_err(|e| err(e, "setting common name"))?;
        let name = name.build();
        self.builder
            .set_subject_name(&name)
            .map_err(|e| err(e, "setting subject name"))?;
        self.builder
            .set_issuer_name(&name)
            .map_err(|e| err(e, "setting issuer name"))
    }

    pub fn set_validity_days(&mut self, days: u32) -> Result<(), X509Error> {
        let not_before =
            openssl::asn1::Asn1Time::days_from_now(0).map_err(|e| err(e, "creating not_before"))?;
        self.builder
            .set_not_before(&not_before)
            .map_err(|e| err(e, "setting not_before"))?;
        let not_after = openssl::asn1::Asn1Time::days_from_now(days)
            .map_err(|e| err(e, "creating not_after"))?;
        self.builder
            .set_not_after(&not_after)
            .map_err(|e| err(e, "setting not_after"))
    }

    pub fn sign_and_build(
        mut self,
        key_pair: &crate::rsa::RsaKeyPair,
    ) -> Result<X509CertificateInner, X509Error> {
        let pkey = openssl::pkey::PKey::from_rsa(key_pair.0.rsa.clone())
            .map_err(|e| err(e, "converting RSA key for signing"))?;
        self.builder
            .sign(&pkey, openssl::hash::MessageDigest::sha256())
            .map_err(|e| err(e, "signing certificate"))?;
        Ok(X509CertificateInner {
            cert: self.builder.build(),
        })
    }
}
