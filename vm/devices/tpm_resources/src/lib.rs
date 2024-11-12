// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for the TPM device.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use inspect::Inspect;
use mesh::MeshPayload;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::NonVolatileStoreKind;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;

/// A handle to a TPM device.
#[derive(MeshPayload)]
pub struct TpmDeviceHandle {
    /// Non-volatile store for PPI (physical presence interface) data
    pub ppi_store: Resource<NonVolatileStoreKind>,
    /// Non-volatile store for TPM NVRAM data
    pub nvram_store: Resource<NonVolatileStoreKind>,
    /// Whether to refresh TPM seeds on init
    pub refresh_tpm_seeds: bool,
    /// Optional callback for getting an attestation report
    pub get_attestation_report: Option<Resource<GetAttestationReportKind>>,
    /// Optional callback for requesting AK cert
    pub request_ak_cert: Option<Resource<RequestAkCertKind>>,
    /// vTPM register layout (IO port or MMIO)
    pub register_layout: TpmRegisterLayout,
    /// Optional guest secret TPM key to be imported
    pub guest_secret_key: Option<Vec<u8>>,
}

impl ResourceId<ChipsetDeviceHandleKind> for TpmDeviceHandle {
    const ID: &'static str = "tpm";
}

/// A resource kind for AK cert renewal helpers.
pub enum GetAttestationReportKind {}

impl ResourceKind for GetAttestationReportKind {
    const NAME: &'static str = "tpm_get_attestation_report";
}

/// A resource kind for AK cert renewal helpers.
pub enum RequestAkCertKind {}

impl ResourceKind for RequestAkCertKind {
    const NAME: &'static str = "tpm_request_ak_cert";
}

/// The vTPM control area register layout
#[derive(Inspect, MeshPayload, PartialEq)]
pub enum TpmRegisterLayout {
    /// Using IO port
    IoPort,
    /// MMIO
    Mmio,
}
