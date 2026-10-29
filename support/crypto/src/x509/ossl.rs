// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using OpenSSL.

use super::X509Error;
use super::X509PublicKey;
use super::X509VerifyError;

fn err(err: openssl::error::ErrorStack, op: &'static str) -> X509Error {
    X509Error(crate::BackendError(err, op))
}

pub struct X509CertificateInner(pub(crate) openssl::x509::X509);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert = openssl::x509::X509::from_der(data)
            .map_err(|e| err(e, "parsing the DER certificate"))?;
        Ok(Self(cert))
    }

    pub fn public_key(&self) -> Result<X509PublicKey, X509Error> {
        let pkey = self
            .0
            .public_key()
            .map_err(|e| err(e, "extracting the certificate public key"))?;
        if pkey.rsa().is_ok() {
            Ok(X509PublicKey::Rsa(crate::rsa::RsaPublicKey(
                crate::rsa::ossl::RsaPublicKeyInner(pkey),
            )))
        } else if pkey.ec_key().is_ok() {
            let inner = crate::ecdsa::ossl::EcdsaPublicKeyInner::from_pkey(pkey)
                .map_err(|e| X509Error(e.0))?;
            Ok(X509PublicKey::Ecdsa(crate::ecdsa::EcdsaPublicKey(inner)))
        } else {
            Err(err(
                openssl::error::ErrorStack::get(),
                "checking that the certificate public key algorithm is supported",
            ))
        }
    }

    pub fn verify(&self, issuer_public_key: &X509PublicKey) -> Result<bool, X509VerifyError> {
        match issuer_public_key {
            X509PublicKey::Rsa(key) => self.0.verify(&key.0.0),
            X509PublicKey::Ecdsa(key) => self.0.verify(&key.0.pkey),
        }
        .map_err(|e| err(e, "verifying the certificate signature").into())
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
            .map_err(|e| err(e, "encoding the certificate as DER"))
    }

    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        let mut parts = Vec::new();
        for entry in self.0.issuer_name().entries() {
            let oid = entry.object().to_string();
            let value = entry
                .data()
                .as_utf8()
                .map_err(|e| err(e, "decoding an issuer name entry"))?
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
            .map_err(|e| err(e, "converting the serial number"))?;
        Ok(bn.to_vec())
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn build_self_signed(
        key: super::X509KeyPair<'_>,
        options: &super::X509SelfSignedOptions<'_>,
    ) -> anyhow::Result<Self> {
        let (key, hash) = match key {
            super::X509KeyPair::Rsa(key) => (&key.0.0, openssl::hash::MessageDigest::sha256()),
            super::X509KeyPair::Ecdsa(key) => (&key.0.pkey, openssl::hash::MessageDigest::sha384()),
        };
        let mut builder = openssl::x509::X509::builder()?;
        builder.set_version(2)?;
        let serial = openssl::bn::BigNum::from_u32(1)?.to_asn1_integer()?;
        builder.set_serial_number(&serial)?;
        builder.set_pubkey(key)?;
        let mut name = openssl::x509::X509Name::builder()?;
        for (nid, value) in [
            (openssl::nid::Nid::COUNTRYNAME, options.country),
            (openssl::nid::Nid::STATEORPROVINCENAME, options.state),
            (openssl::nid::Nid::LOCALITYNAME, options.locality),
            (openssl::nid::Nid::ORGANIZATIONNAME, options.organization),
            (openssl::nid::Nid::COMMONNAME, options.common_name),
        ] {
            if !value.is_empty() {
                name.append_entry_by_nid(nid, value)?;
            }
        }
        let name = name.build();
        builder.set_subject_name(&name)?;
        builder.set_issuer_name(&name)?;
        let not_before = openssl::asn1::Asn1Time::from_str_x509(options.not_before)?;
        builder.set_not_before(&not_before)?;
        let not_after = openssl::asn1::Asn1Time::from_str_x509(options.not_after)?;
        builder.set_not_after(&not_after)?;
        anyhow::ensure!(
            not_before.compare(&not_after)? != std::cmp::Ordering::Greater,
            "certificate validity ends before it starts"
        );
        if let Some(id) = options.authority_key_identifier {
            let id: String = id.iter().map(|byte| format!("{byte:02X}")).collect();
            // The typed AKI builder only supports deriving an ID from an issuer.
            #[expect(deprecated)]
            let extension = openssl::x509::X509Extension::new_nid(
                None,
                None,
                openssl::nid::Nid::AUTHORITY_KEY_IDENTIFIER,
                &format!("ASN1:SEQWRAP,IMPLICIT:0,FORMAT:HEX,OCTETSTRING:{id}"),
            )?;
            builder.append_extension(extension)?;
        }
        builder.sign(key, hash)?;
        Ok(Self(builder.build()))
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
                .map_err(|e| err(e, "decoding the subject common name")),
        }
    }
}
