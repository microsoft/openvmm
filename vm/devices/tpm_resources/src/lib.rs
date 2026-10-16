// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for the TPM device.

#![forbid(unsafe_code)]

use guid::Guid;
use inspect::Inspect;
use mesh::MeshPayload;
use std::sync::Arc;
use vm_resource::CanResolveTo;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::NonVolatileStoreKind;

/// A handle to a TPM device.
#[derive(MeshPayload)]
pub struct TpmDeviceHandle {
    /// TPM reference implementation version
    pub version: TpmVersion,
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
    /// Whether or not the TPM is in a confidential VM
    pub is_confidential_vm: bool,
    /// BIOS GUID (for logging purposes)
    pub bios_guid: Guid,
    /// NVRAM size (default size if None)
    pub nvram_size: Option<usize>,
}

impl ResourceId<ChipsetDeviceHandleKind> for TpmDeviceHandle {
    const ID: &'static str = "tpm";
}

/// Version of the Microsoft TPM reference implementation to use.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub enum TpmVersion {
    /// TPM reference implementation version 1.38
    V138,
    /// TPM reference implementation version 1.85
    V185,
}

/// Returns the default vTPM NVRAM size for `version`.
///
/// Each reference implementation is compiled for a fixed NVRAM size and reads
/// and writes anywhere in that range, so this must match the library in use.
pub const fn default_vtpm_size(version: TpmVersion) -> usize {
    match version {
        TpmVersion::V138 => 32 * 1024,
        TpmVersion::V185 => 128 * 1024,
    }
}

/// A resource kind for AK cert renewal helpers.
pub enum RequestAkCertKind {}

impl ResourceKind for RequestAkCertKind {
    const NAME: &'static str = "tpm_request_ak_cert";
}

impl CanResolveTo<ResolvedRequestAkCert> for RequestAkCertKind {
    // Workaround for async_trait not supporting GATs with missing lifetimes.
    type Input<'a> = &'a ();
}

/// A resolved AK cert request helper resource.
pub struct ResolvedRequestAkCert(pub Arc<dyn RequestAkCert>);

impl<T: 'static + RequestAkCert> From<T> for ResolvedRequestAkCert {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// A helper for creating and issuing AK cert requests.
#[async_trait::async_trait]
pub trait RequestAkCert: Send + Sync {
    /// Creates the request payload needed by [`RequestAkCert::request_ak_cert`].
    fn create_ak_cert_request(
        &self,
        ak_pub_modulus: &[u8],
        ak_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        guest_input: &[u8],
        is_attestation_report: bool,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

    /// Requests an AK cert.
    async fn request_ak_cert(
        &self,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync + 'static>>;
}

/// `TpmAkCertType`-equivalent enum for resource
#[derive(MeshPayload)]
pub enum TpmAkCertTypeResource {
    /// No Ak cert.
    None,
    /// Authorized AK cert that is not hardware-attested. Optional bool controls
    /// whether OpenHCL handles renewal.
    /// Used by TVM
    Trusted(Resource<RequestAkCertKind>, Option<bool>),
    /// Authorized and hardware-attested AK cert (backed by
    /// a TEE attestation report).
    /// Used by CVM
    HwAttested(Resource<RequestAkCertKind>),
    /// Authorized and software-attested AK cert (backed by
    /// a software-based VM attestation report).
    /// Used by Vbs VM
    SwAttested(Resource<RequestAkCertKind>),
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

impl CanResolveTo<ResolvedTpmLogger> for TpmLoggerKind {
    // Workaround for async_trait not supporting GATs with missing lifetimes.
    type Input<'a> = &'a ();
}

/// A resolved TPM logger resource.
pub struct ResolvedTpmLogger(pub Arc<dyn TpmLogger>);

impl<T: 'static + TpmLogger> From<T> for ResolvedTpmLogger {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// An event reported by [`TpmLogger`].
pub enum TpmLogEvent {
    /// Failed to renew AK cert.
    AkCertRenewalFailed,
    /// Failed to change TPM seeds.
    IdentityChangeFailed,
    /// Invalid PPI or NVRAM state.
    InvalidState,
}

/// A host-provided logger for TPM events.
#[async_trait::async_trait]
pub trait TpmLogger: Send + Sync {
    /// Sends an event to the host and flushes it.
    async fn log_event_and_flush(&self, event: TpmLogEvent);

    /// Sends an event to the host without flushing it.
    ///
    /// This is needed for the non-async AK cert request callback.
    fn log_event(&self, event: TpmLogEvent);
}

#[async_trait::async_trait]
impl TpmLogger for Option<Arc<dyn TpmLogger>> {
    async fn log_event_and_flush(&self, event: TpmLogEvent) {
        if let Some(logger) = self {
            logger.log_event_and_flush(event).await;
        }
    }

    fn log_event(&self, event: TpmLogEvent) {
        if let Some(logger) = self {
            logger.log_event(event);
        }
    }
}
