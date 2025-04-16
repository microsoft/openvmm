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
    /// Type of AK cert
    pub ak_cert_type: TpmAkCertTypeResource,
    /// vTPM register layout (IO port or MMIO)
    pub register_layout: TpmRegisterLayout,
    /// Optional guest secret TPM key to be imported
    pub guest_secret_key: Option<Vec<u8>>,
    /// Optional logger to send event to the host
    pub logger: Option<Resource<TpmLoggerKind>>,
}

impl ResourceId<ChipsetDeviceHandleKind> for TpmDeviceHandle {
    const ID: &'static str = "tpm";
}

/// A resource kind for AK cert renewal helpers.
pub enum RequestAkCertKind {}

impl ResourceKind for RequestAkCertKind {
    const NAME: &'static str = "tpm_request_ak_cert";
}

/// `TpmAkCertType`-equivalent enum for resource
#[derive(MeshPayload)]
pub enum TpmAkCertTypeResource {
    /// No Ak cert.
    None,
    /// Authorized AK cert that is not hardware-attested.
    /// Used by TVM
    Trusted(Resource<RequestAkCertKind>),
    /// Authorized and hardware-attested AK cert (backed by
    /// a TEE attestation report).
    /// Used by CVM
    HwAttested(Resource<RequestAkCertKind>),
}

/// The vTPM control area register layout
#[derive(Inspect, MeshPayload, PartialEq)]
pub enum TpmRegisterLayout {
    /// Using IO port
    IoPort,
    /// MMIO
    Mmio,
}

/// A resource kind for TPM logger.
pub enum TpmLoggerKind {}

impl ResourceKind for TpmLoggerKind {
    const NAME: &'static str = "tpm_logger";
}
