// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper traits for TPM Attestation Key Certificate (AK cert).

use tpm_resources::RequestAkCertKind;
use vm_resource::CanResolveTo;

/// Type of TPM AK cert.
pub enum TpmAkCertType {
    /// No Ak cert.
    None,
    /// Authorized AK cert that is not hardware-attested.
    /// Used by TVM
    Trusted(Box<dyn RequestAkCert>),
    /// Authorized and hardware-attested AK cert (backed by
    /// a TEE attestation report).
    /// Used by CVM
    HwAttested(Box<dyn RequestAkCert>),
}

impl CanResolveTo<ResolvedRequestAkCert> for RequestAkCertKind {
    // Workaround for async_trait not supporting GATs with missing lifetimes.
    type Input<'a> = &'a ();
}

/// A resolved get attestation report helper resource.
pub struct ResolvedRequestAkCert(pub Box<dyn RequestAkCert>);

impl<T: 'static + RequestAkCert> From<T> for ResolvedRequestAkCert {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

/// A trait for requesting an AK cert.
#[async_trait::async_trait]
pub trait RequestAkCert: Send + Sync {
    /// Helper function to create the request needed by `request_ak_cert`.
    fn create_ak_cert_request(
        &self,
        ak_pub_modulus: &[u8],
        ak_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        guest_input: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

    /// Helper function to request an AK cert.
    async fn request_ak_cert(
        &self,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync + 'static>>;

    /// Get a clone of the trait object.
    fn clone_box(&self) -> Box<dyn RequestAkCert>;
}
