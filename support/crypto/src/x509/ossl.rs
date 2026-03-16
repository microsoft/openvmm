// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub(crate) struct X509CertificateInner {
    pub(crate) x509: openssl::x509::X509,
}

pub(crate) struct PublicKeyInner {
    pkey: openssl::pkey::PKey<openssl::pkey::Public>,
}

pub fn x509_from_der(der: &[u8]) -> Result<X509CertificateInner, X509Error> {
    let x509 = openssl::x509::X509::from_der(der)
        .map_err(|e| X509Error(crate::BackendError(e, "parsing X.509 certificate")))?;
    Ok(X509CertificateInner { x509 })
}

impl X509CertificateInner {
    pub fn public_key(&self) -> Result<PublicKeyInner, X509Error> {
        let pkey = self
            .x509
            .public_key()
            .map_err(|e| X509Error(crate::BackendError(e, "getting public key")))?;
        Ok(PublicKeyInner { pkey })
    }

    pub fn verify(&self, key: &PublicKeyInner) -> Result<bool, X509Error> {
        self.x509
            .verify(&key.pkey)
            .map_err(|e| X509Error(crate::BackendError(e, "verifying certificate")))
    }

    pub fn issued(&self, child: &X509CertificateInner) -> bool {
        self.x509.issued(&child.x509) == openssl::x509::X509VerifyResult::OK
    }
}

impl PublicKeyInner {
    pub fn key_type(&self) -> KeyType {
        if self.pkey.id() == openssl::pkey::Id::RSA {
            KeyType::Rsa
        } else {
            KeyType::Other
        }
    }

    pub fn verify_rsa_sha256(&self, data: &[u8], signature: &[u8]) -> Result<bool, X509Error> {
        let mut verifier =
            openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &self.pkey)
                .map_err(|e| X509Error(crate::BackendError(e, "creating RSA-SHA256 verifier")))?;
        verifier
            .set_rsa_padding(openssl::rsa::Padding::PKCS1)
            .map_err(|e| X509Error(crate::BackendError(e, "setting RSA padding")))?;
        verifier
            .update(data)
            .map_err(|e| X509Error(crate::BackendError(e, "updating verifier")))?;
        verifier
            .verify(signature)
            .map_err(|e| X509Error(crate::BackendError(e, "verifying signature")))
    }
}
