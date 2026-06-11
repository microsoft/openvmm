// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signature verification using OpenSSL.

use super::*;

pub struct Pkcs7SignedDataInner(openssl::pkcs7::Pkcs7);

fn err(err: openssl::error::ErrorStack, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError(err, op))
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        openssl::pkcs7::Pkcs7::from_der(data)
            .map(Self)
            .map_err(|e| err(e, "decoding pkcs#7 from DER"))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0
            .to_der()
            .map_err(|e| err(e, "encoding pkcs#7 as DER"))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn sign(
        cert: &X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, crate::rsa::RsaError> {
        fn rsa_err(err: openssl::error::ErrorStack, op: &'static str) -> crate::rsa::RsaError {
            crate::rsa::RsaError(crate::BackendError(err, op))
        }
        let certs = openssl::stack::Stack::new()
            .map_err(|e| rsa_err(e, "creating empty certificate stack"))?;
        let pkcs7 = openssl::pkcs7::Pkcs7::sign(
            &cert.0.0,
            &key_pair.0.0,
            &certs,
            data,
            // - DETACHED: do not embed the content; the verifier supplies it.
            // - BINARY: do not perform text/CRLF canonicalization.
            // - NOATTR: omit signedAttrs; the signature covers the raw
            //   encapsulated content directly per RFC 5652 §5.4.
            openssl::pkcs7::Pkcs7Flags::DETACHED
                | openssl::pkcs7::Pkcs7Flags::BINARY
                | openssl::pkcs7::Pkcs7Flags::NOATTR,
        )
        .map_err(|e| rsa_err(e, "pkcs7 signing"))?;
        Ok(Self(pkcs7))
    }

    pub fn verify(
        &self,
        trusted_certs: &[X509Certificate],
        signed_content: &[u8],
    ) -> Result<bool, Pkcs7VerifyError> {
        let mut store = openssl::x509::store::X509StoreBuilder::new()
            .map_err(|e| err(e, "creating x509 store builder"))?;
        for trusted_cert in trusted_certs {
            store
                .add_cert(trusted_cert.0.0.clone())
                .map_err(|e| err(e, "adding certificate to store"))?;
        }

        // - `PARTIAL_CHAIN`: accept any cert in the store as a trust
        //   anchor, not just self-signed roots. EFI signature lists
        //   typically provide leaf/intermediate certs.
        // - `NO_CHECK_TIME`: accept expired certs. UEFI signing certs
        //   observed in the wild are often long expired, and existing
        //   firmware verifiers accept them.
        let store_flags = openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN
            | openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
        store
            .set_flags(store_flags)
            .map_err(|e| err(e, "setting x509 verify flags"))?;

        // `X509Purpose::ANY`: accept any key-usage / extended-key-usage.
        // Without this, OpenSSL rejects UEFI signature-list certs with
        // "unsupported certificate purpose" because they are not marked
        // with the usages a verifier expects for the default purpose.
        store
            .set_purpose(openssl::x509::X509PurposeId::ANY)
            .map_err(|e| err(e, "setting x509 purpose"))?;

        let store = store.build();

        // openssl-rs requires an explicit certificate stack here even though
        // PKCS#7 verification supports omitting it.
        let cert_stack = openssl::stack::Stack::new()
            .map_err(|e| err(e, "allocating empty certificate stack"))?;

        match self.0.verify(
            &cert_stack,
            &store,
            Some(signed_content),
            None,
            openssl::pkcs7::Pkcs7Flags::empty(),
        ) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}
