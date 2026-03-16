// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signature verification.

use crate::x509::X509Certificate;
use thiserror::Error;

/// Error from PKCS#7 operations.
#[derive(Debug, Error)]
pub enum Pkcs7Error {
    #[error("failed to parse PKCS#7 from DER")]
    ParseDer(#[source] openssl::error::ErrorStack),
    #[error("failed to build X509 store")]
    StoreBuilder(#[source] openssl::error::ErrorStack),
    #[error("PKCS#7 verification failed")]
    Verify(#[source] openssl::error::ErrorStack),
}

/// A parsed PKCS#7 structure.
pub struct Pkcs7 {
    inner: openssl::pkcs7::Pkcs7,
}

/// Flags controlling PKCS#7 X.509 store behavior.
pub struct X509StoreFlags {
    /// Terminate chain verification at whatever certs are present (don't
    /// require a full chain to a root CA).
    pub partial_chain: bool,
    /// Don't check certificate expiration times.
    pub no_check_time: bool,
    /// Accept certs for any purpose.
    pub any_purpose: bool,
}

impl Pkcs7 {
    /// Parse a PKCS#7 structure from DER-encoded bytes.
    pub fn from_der(der: &[u8]) -> Result<Self, Pkcs7Error> {
        let inner = openssl::pkcs7::Pkcs7::from_der(der).map_err(Pkcs7Error::ParseDer)?;
        Ok(Self { inner })
    }

    /// Verify the PKCS#7 signed data against the given trusted certificates.
    ///
    /// Returns `Ok(true)` if verification succeeds, `Ok(false)` if the
    /// signature doesn't match.
    pub fn verify(
        &self,
        trusted_certs: &[X509Certificate],
        data: &[u8],
        flags: &X509StoreFlags,
    ) -> Result<bool, Pkcs7Error> {
        let mut store_builder =
            openssl::x509::store::X509StoreBuilder::new().map_err(Pkcs7Error::StoreBuilder)?;

        for cert in trusted_certs {
            store_builder
                .add_cert(cert.inner.clone())
                .map_err(Pkcs7Error::StoreBuilder)?;
        }

        let mut verify_flags = openssl::x509::verify::X509VerifyFlags::empty();
        if flags.partial_chain {
            verify_flags |= openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN;
        }
        if flags.no_check_time {
            verify_flags |= openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
        }
        store_builder
            .set_flags(verify_flags)
            .map_err(Pkcs7Error::StoreBuilder)?;

        if flags.any_purpose {
            store_builder
                .set_purpose(openssl::x509::X509PurposeId::ANY)
                .map_err(Pkcs7Error::StoreBuilder)?;
        }

        let store = store_builder.build();

        let empty_stack = openssl::stack::Stack::new().map_err(Pkcs7Error::Verify)?;

        match self.inner.verify(
            &empty_stack,
            &store,
            Some(data),
            None,
            openssl::pkcs7::Pkcs7Flags::empty(),
        ) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}
