// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct X509CertificateInner(openssl::x509::X509);

pub struct Pkcs7SignedDataInner(openssl::pkcs7::Pkcs7);

fn pkcs7_err(err: openssl::error::ErrorStack, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError(err, op))
}

fn x509_err(err: openssl::error::ErrorStack, op: &'static str) -> X509CertificateError {
    X509CertificateError(crate::BackendError(err, op))
}

fn verify_err(err: openssl::error::ErrorStack, op: &'static str) -> Pkcs7VerifyError {
    Pkcs7VerifyError(crate::BackendError(err, op))
}

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509CertificateError> {
        openssl::x509::X509::from_der(data)
            .map(Self)
            .map_err(|e| x509_err(e, "decoding x509 certificate from DER"))
    }
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        openssl::pkcs7::Pkcs7::from_der(data)
            .map(Self)
            .map_err(|e| pkcs7_err(e, "decoding pkcs#7 from DER"))
    }

    pub fn verify(
        &self,
        trusted_certs: &[&X509CertificateInner],
        signed_content: &[u8],
    ) -> Result<bool, Pkcs7VerifyError> {
        let mut store = openssl::x509::store::X509StoreBuilder::new()
            .map_err(|e| verify_err(e, "creating x509 store builder"))?;

        for cert in trusted_certs {
            store
                .add_cert(cert.0.clone())
                .map_err(|e| verify_err(e, "adding trusted x509 certificate to store"))?;
        }

        let store_flags = openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN
            | openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
        store
            .set_flags(store_flags)
            .map_err(|e| verify_err(e, "setting x509 verify flags"))?;

        store
            .set_purpose(openssl::x509::X509PurposeId::ANY)
            .map_err(|e| verify_err(e, "setting x509 purpose"))?;

        let store = store.build();

        // openssl-rs requires an explicit certificate stack here even though
        // PKCS#7 verification supports omitting it.
        let cert_stack = openssl::stack::Stack::new()
            .map_err(|e| verify_err(e, "allocating empty certificate stack"))?;

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
