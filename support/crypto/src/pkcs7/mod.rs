// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PKCS#7 signature verification.

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use crate::x509::X509Certificate;
use thiserror::Error;

/// Error from PKCS#7 operations.
#[derive(Debug, Error)]
#[error("PKCS#7 error")]
pub struct Pkcs7Error(#[source] super::BackendError);

/// A parsed PKCS#7 structure.
pub struct Pkcs7 {
    inner: sys::Pkcs7Inner,
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
        sys::pkcs7_from_der(der).map(|inner| Self { inner })
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
        self.inner.verify(trusted_certs, data, flags)
    }
}
