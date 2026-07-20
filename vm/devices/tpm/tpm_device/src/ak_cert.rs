// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper traits for TPM Attestation Key Certificate (AK cert).

use std::sync::Arc;
use tpm_resources::RequestAkCertKind;
use vm_resource::CanResolveTo;

/// Type of TPM AK cert.
pub enum TpmAkCertType {
    /// No Ak cert.
    None,
    /// Authorized AK cert that is not hardware-attested. Optional bool controls
    /// whether OpenHCL handles renewal.
    /// Used by TVM
    Trusted(Arc<dyn RequestAkCert>, Option<bool>),
    /// Authorized TVM AK cert with IDK_S-backed host-certification evidence.
    HostCertified(Arc<dyn RequestAkCert>, Option<bool>),
    /// Authorized and hardware-attested AK cert (backed by
    /// a TEE attestation report).
    /// Used by CVM
    HwAttested(Arc<dyn RequestAkCert>),
    /// Authorized and software-attested AK cert (backed by
    /// a software-based VM attestation report).
    /// Used by Vbs VM
    SwAttested(Arc<dyn RequestAkCert>),
}

impl TpmAkCertType {
    /// Get the `RequestAkCert` from the enum
    pub fn get_ak_cert_helper(&self) -> Option<&Arc<dyn RequestAkCert>> {
        match self {
            TpmAkCertType::HwAttested(helper) => Some(helper),
            TpmAkCertType::SwAttested(helper) => Some(helper),
            TpmAkCertType::Trusted(helper, _) => Some(helper),
            TpmAkCertType::HostCertified(helper, _) => Some(helper),
            TpmAkCertType::None => None,
        }
    }

    /// Returns true if this AKCert type exposes an attestation-report NV index.
    pub fn has_attestation_report(&self) -> bool {
        match self {
            TpmAkCertType::HwAttested(_)
            | TpmAkCertType::SwAttested(_)
            | TpmAkCertType::HostCertified(_, _) => true,
            TpmAkCertType::Trusted(_, _) | TpmAkCertType::None => false,
        }
    }

    /// Returns true if the attestation report is generated locally on NV read.
    pub fn refreshes_attestation_report(&self) -> bool {
        matches!(
            self,
            TpmAkCertType::HwAttested(_) | TpmAkCertType::SwAttested(_)
        )
    }

    /// Returns true if the attestation report must be preserved across boot.
    pub fn preserves_attestation_report(&self) -> bool {
        matches!(self, TpmAkCertType::HostCertified(_, _))
    }
}

impl CanResolveTo<ResolvedRequestAkCert> for RequestAkCertKind {
    // Workaround for async_trait not supporting GATs with missing lifetimes.
    type Input<'a> = &'a ();
}

/// A resolved request AK cert helper resource.
pub struct ResolvedRequestAkCert(pub Arc<dyn RequestAkCert>);

/// AK certificate request result.
pub struct AkCertRequestResult {
    pub ak_cert: Vec<u8>,
    pub host_certification_evidence: Option<Vec<u8>>,
}

impl<T: 'static + RequestAkCert> From<T> for ResolvedRequestAkCert {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
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
        is_attestation_report: bool,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

    /// Helper function to request an AK cert.
    async fn request_ak_cert(
        &self,
        request: Vec<u8>,
    ) -> Result<AkCertRequestResult, Box<dyn std::error::Error + Send + Sync + 'static>>;
}
