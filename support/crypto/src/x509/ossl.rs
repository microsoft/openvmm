// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using OpenSSL.

use super::X509Error;
use super::X509PublicKey;

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

    pub fn public_key(&self) -> Result<X509PublicKey, X509Error> {
        let pkey = self
            .0
            .public_key()
            .map_err(|e| err(e, "extracting public key"))?;
        if pkey.rsa().is_ok() {
            Ok(X509PublicKey::Rsa(crate::rsa::RsaPublicKey(
                crate::rsa::ossl::RsaPublicKeyInner(pkey),
            )))
        } else if pkey.ec_key().is_ok() {
            // Hand the already-parsed key to the ECDSA backend directly rather
            // than re-serializing and re-parsing it.
            let inner = crate::ecdsa::ossl::EcdsaPublicKeyInner::from_pkey(pkey)
                .map_err(|crate::ecdsa::EcdsaError(e)| X509Error(e))?;
            Ok(X509PublicKey::Ecdsa(crate::ecdsa::EcdsaPublicKey(inner)))
        } else {
            Err(err(
                openssl::error::ErrorStack::get(),
                "unsupported certificate public key type",
            ))
        }
    }

    pub fn verify(
        &self,
        issuer_public_key: &crate::rsa::RsaPublicKey,
    ) -> Result<bool, crate::rsa::RsaError> {
        self.0.verify(&issuer_public_key.0.0).map_err(|e| {
            crate::rsa::RsaError(crate::BackendError(e, "verifying certificate signature"))
        })
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> Result<bool, X509Error> {
        // `X509_check_issued` only performs deterministic comparisons on
        // already-parsed data (name, AKID/SKID, serial, KeyUsage) and cannot
        // fail with internal errors. Per the OpenSSL docs, every non-OK
        // result is an `X509_V_ERR*` constant "indicating why the issuer
        // does not match" — i.e., a legitimate `Ok(false)`.
        Ok(self.0.issued(&subject.0) == openssl::x509::X509VerifyResult::OK)
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.0
            .to_der()
            .map_err(|e| err(e, "encoding certificate as DER"))
    }

    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        let mut parts = Vec::new();
        for entry in self.0.issuer_name().entries() {
            let oid = entry.object().to_string();
            let value = entry
                .data()
                .as_utf8()
                .map_err(|e| err(e, "decoding issuer name entry"))?
                .to_string();
            parts.push(format!("{oid}={value}"));
        }
        Ok(parts.join(","))
    }

    pub fn serial_number(&self) -> Result<Vec<u8>, X509Error> {
        let bn = self
            .0
            .serial_number()
            .to_bn()
            .map_err(|e| err(e, "converting serial number"))?;
        Ok(bn.to_vec())
    }

    #[cfg(any(test, feature = "test_helpers"))]
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

    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        let sn = self
            .0
            .subject_name()
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next();
        match sn {
            None => Ok(None),
            Some(sn) => sn
                .data()
                .as_utf8()
                .map(|u| Some(u.to_string()))
                .map_err(|e| err(e, "decoding subject name")),
        }
    }
}
