// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper traits for TPM Attestation Key Certificate (AK cert).

use std::sync::Arc;
use tpm_resources::RequestAkCert;

/// Type of TPM AK cert.
pub enum TpmAkCertType {
    /// No Ak cert.
    None,
    /// Authorized AK cert that is not hardware-attested. Optional bool controls
    /// whether OpenHCL handles renewal.
    /// Used by TVM
    Trusted(Arc<dyn RequestAkCert>, Option<bool>),
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
            TpmAkCertType::None => None,
        }
    }

    /// Returns true if this AKCert type is attested, either with a TEE
    /// attestation report or a software-based VM attestation report.
    pub fn attested(&self) -> bool {
        match self {
            TpmAkCertType::HwAttested(_) | TpmAkCertType::SwAttested(_) => true,
            TpmAkCertType::Trusted(_, _) | TpmAkCertType::None => false,
        }
    }
}
