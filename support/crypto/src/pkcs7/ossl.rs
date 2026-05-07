// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct Pkcs7SignedDataInner(openssl::pkcs7::Pkcs7);

pub struct Pkcs7CertStoreInner(openssl::x509::store::X509StoreBuilder);

fn err(err: openssl::error::ErrorStack, op: &'static str) -> Pkcs7Error {
    Pkcs7Error(crate::BackendError(err, op))
}

impl Pkcs7CertStoreInner {
    pub fn new() -> Result<Self, Pkcs7Error> {
        let builder = openssl::x509::store::X509StoreBuilder::new()
            .map_err(|e| err(e, "creating x509 store builder"))?;
        Ok(Self(builder))
    }

    pub fn add_cert_der(&mut self, data: &[u8]) -> Result<(), Pkcs7Error> {
        let cert = openssl::x509::X509::from_der(data)
            .map_err(|e| err(e, "decoding x509 certificate from DER"))?;
        self.0
            .add_cert(cert)
            .map_err(|e| err(e, "adding certificate to store"))
    }
}

impl Pkcs7SignedDataInner {
    pub fn from_der(data: &[u8]) -> Result<Self, Pkcs7Error> {
        openssl::pkcs7::Pkcs7::from_der(data)
            .map(Self)
            .map_err(|e| err(e, "decoding pkcs#7 from DER"))
    }

    pub fn to_der(&self) -> Result<Vec<u8>, Pkcs7Error> {
        self.0
            .to_der()
            .map_err(|e| err(e, "encoding pkcs#7 as DER"))
    }

    pub fn sign(
        cert: &crate::x509::X509Certificate,
        key_pair: &crate::rsa::RsaKeyPair,
        data: &[u8],
    ) -> Result<Self, Pkcs7Error> {
        let certs =
            openssl::stack::Stack::new().map_err(|e| err(e, "creating empty certificate stack"))?;
        let pkcs7 = openssl::pkcs7::Pkcs7::sign(
            &cert.0.cert,
            &key_pair.0.0,
            &certs,
            data,
            openssl::pkcs7::Pkcs7Flags::empty(),
        )
        .map_err(|e| err(e, "pkcs7 signing"))?;
        Ok(Self(pkcs7))
    }

    pub fn verify(
        self,
        mut store: Pkcs7CertStoreInner,
        signed_content: &[u8],
        uefi_mode: bool,
    ) -> Result<bool, Pkcs7Error> {
        if uefi_mode {
            // See `Pkcs7SignedData::verify` for the semantics of `uefi_mode`.
            //
            // - `PARTIAL_CHAIN`: accept any cert in the store as a trust
            //   anchor, not just self-signed roots. EFI signature lists
            //   typically provide leaf/intermediate certs.
            // - `NO_CHECK_TIME`: accept expired certs. UEFI signing certs
            //   observed in the wild are often long expired, and existing
            //   firmware verifiers accept them.
            let store_flags = openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN
                | openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
            store
                .0
                .set_flags(store_flags)
                .map_err(|e| err(e, "setting x509 verify flags"))?;

            // `X509Purpose::ANY`: accept any key-usage / extended-key-usage.
            // Without this, OpenSSL rejects UEFI signature-list certs with
            // "unsupported certificate purpose" because they are not marked
            // with the usages a verifier expects for the default purpose.
            store
                .0
                .set_purpose(openssl::x509::X509PurposeId::ANY)
                .map_err(|e| err(e, "setting x509 purpose"))?;
        }

        let store = store.0.build();

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
