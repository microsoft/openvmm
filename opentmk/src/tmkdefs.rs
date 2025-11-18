// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TMK error definitions and result type alias.
//!

use thiserror::Error;

/// Primary error type produced by TMK operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
pub enum TmkError {
    /// Returned when an error occurs in ACPI parsing or handling.
    #[error("Error occurred in ACPI handling")]
    AcpiError,
    /// Returned when a memory allocation attempt fails.
    #[error("allocation failed")]
    AllocationFailed,
    /// Returned when an input parameter is invalid.
    #[error("invalid parameter")]
    InvalidParameter,
    /// Returned when enabling a VTL operation fails.
    #[error("failed to enable VTL")]
    EnableVtlFailed,
    /// Returned when setting the default context fails.
    #[error("failed to set default context")]
    SetDefaultCtxFailed,
    /// Returned when starting a virtual processor fails.
    #[error("failed to start VP")]
    StartVpFailed,
    /// Returned when queuing a command fails.
    #[error("failed to queue command")]
    QueueCommandFailed,
    /// Returned when configuring VTL protection fails.
    #[error("failed to set up VTL protection")]
    SetupVtlProtectionFailed,
    /// Returned when configuring partition-level VTL fails.
    #[error("failed to set up partition VTL")]
    SetupPartitionVtlFailed,
    /// Returned when installing the interrupt handler fails.
    #[error("failed to set up interrupt handler")]
    SetupInterruptHandlerFailed,
    /// Returned when assigning an interrupt index fails.
    #[error("failed to set interrupt index")]
    SetInterruptIdxFailed,
    /// Returned when configuring secure intercept fails.
    #[error("failed to set up secure intercept")]
    SetupSecureInterceptFailed,
    /// Returned when applying VTL memory protection fails.
    #[error("failed to apply VTL protection for memory")]
    ApplyVtlProtectionForMemoryFailed,
    /// Returned when reading an MSR fails.
    #[error("failed to read MSR")]
    ReadMsrFailed,
    /// Returned when writing an MSR fails.
    #[error("failed to write MSR")]
    WriteMsrFailed,
    /// Returned when reading a register fails.
    #[error("failed to get register")]
    GetRegisterFailed,
    /// Returned when a hypercall code is unrecognized.
    #[error("invalid hypercall code")]
    InvalidHypercallCode,
    /// Returned when hypercall input is invalid.
    #[error("invalid hypercall input")]
    InvalidHypercallInput,
    /// Returned when a value is not properly aligned.
    #[error("invalid alignment")]
    InvalidAlignment,
    /// Returned when the operation lacks required privileges.
    #[error("access denied")]
    AccessDenied,
    /// Returned when the partition state is invalid.
    #[error("invalid partition state")]
    InvalidPartitionState,
    /// Returned when the operation is denied.
    #[error("operation denied")]
    OperationDenied,
    /// Returned when querying an unknown property.
    #[error("unknown property")]
    UnknownProperty,
    /// Returned when a property value is outside the supported range.
    #[error("property value out of range")]
    PropertyValueOutOfRange,
    /// Returned when memory resources are insufficient.
    #[error("insufficient memory")]
    InsufficientMemory,
    /// Returned when partition depth exceeds limits.
    #[error("partition too deep")]
    PartitionTooDeep,
    /// Returned when a partition identifier is invalid.
    #[error("invalid partition id")]
    InvalidPartitionId,
    /// Returned when a virtual processor index is invalid.
    #[error("invalid VP index")]
    InvalidVpIndex,
    /// Returned when a requested resource is not found.
    #[error("not found")]
    NotFound,
    /// Returned when a port identifier is invalid.
    #[error("invalid port id")]
    InvalidPortId,
    /// Returned when a connection identifier is invalid.
    #[error("invalid connection id")]
    InvalidConnectionId,
    /// Returned when available buffers are insufficient.
    #[error("insufficient buffers")]
    InsufficientBuffers,
    /// Returned when required acknowledgment is missing.
    #[error("not acknowledged")]
    NotAcknowledged,
    /// Returned when a virtual processor state is invalid.
    #[error("invalid VP state")]
    InvalidVpState,
    /// Returned when an operation was already acknowledged.
    #[error("already acknowledged")]
    Acknowledged,
    /// Returned when save or restore state is invalid.
    #[error("invalid save/restore state")]
    InvalidSaveRestoreState,
    /// Returned when SynIC state is invalid.
    #[error("invalid synic state")]
    InvalidSynicState,
    /// Returned when the object is already in use.
    #[error("object in use")]
    ObjectInUse,
    /// Returned when proximity domain information is invalid.
    #[error("invalid proximity domain info")]
    InvalidProximityDomainInfo,
    /// Returned when no data is available.
    #[error("no data")]
    NoData,
    /// Returned when the target component is inactive.
    #[error("inactive")]
    Inactive,
    /// Returned when required resources are unavailable.
    #[error("no resources")]
    NoResources,
    /// Returned when a requested feature is unavailable.
    #[error("feature unavailable")]
    FeatureUnavailable,
    /// Returned when only a partial packet is available.
    #[error("partial packet")]
    PartialPacket,
    /// Returned when the processor lacks a required feature.
    #[error("processor feature not supported")]
    ProcessorFeatureNotSupported,
    /// Returned when the processor cache line flush size is incompatible.
    #[error("processor cache line flush size incompatible")]
    ProcessorCacheLineFlushSizeIncompatible,
    /// Returned when a provided buffer is too small.
    #[error("insufficient buffer")]
    InsufficientBuffer,
    /// Returned when the processor is incompatible.
    #[error("incompatible processor")]
    IncompatibleProcessor,
    /// Returned when there are not enough device domains.
    #[error("insufficient device domains")]
    InsufficientDeviceDomains,
    /// Returned when CPUID feature validation fails.
    #[error("cpuid feature validation error")]
    CpuidFeatureValidationError,
    /// Returned when CPUID XSAVE feature validation fails.
    #[error("cpuid xsave feature validation error")]
    CpuidXsaveFeatureValidationError,
    /// Returned when processor startup times out.
    #[error("processor startup timeout")]
    ProcessorStartupTimeout,
    /// Returned when SMX is enabled and unsupported.
    #[error("smx enabled")]
    SmxEnabled,
    /// Returned when a logical processor index is invalid.
    #[error("invalid LP index")]
    InvalidLpIndex,
    /// Returned when a register value is invalid.
    #[error("invalid register value")]
    InvalidRegisterValue,
    /// Returned when a VTL state is invalid.
    #[error("invalid VTL state")]
    InvalidVtlState,
    /// Returned when NX support is not detected.
    #[error("nx not detected")]
    NxNotDetected,
    /// Returned when a device identifier is invalid.
    #[error("invalid device id")]
    InvalidDeviceId,
    /// Returned when a device state is invalid.
    #[error("invalid device state")]
    InvalidDeviceState,
    /// Returned when page requests remain pending.
    #[error("pending page requests")]
    PendingPageRequests,
    /// Returned when a page request is invalid.
    #[error("page request invalid")]
    PageRequestInvalid,
    /// Returned when a key already exists.
    #[error("key already exists")]
    KeyAlreadyExists,
    /// Returned when a device is already assigned to a domain.
    #[error("device already in domain")]
    DeviceAlreadyInDomain,
    /// Returned when a CPU group identifier is invalid.
    #[error("invalid cpu group id")]
    InvalidCpuGroupId,
    /// Returned when a CPU group state is invalid.
    #[error("invalid cpu group state")]
    InvalidCpuGroupState,
    /// Returned when an operation fails for an unspecified reason.
    #[error("operation failed")]
    OperationFailed,
    /// Returned when nested virtualization forbids the operation.
    #[error("not allowed with nested virtualization active")]
    NotAllowedWithNestedVirtActive,
    /// Returned when root partition memory is insufficient.
    #[error("insufficient root memory")]
    InsufficientRootMemory,
    /// Returned when an event buffer was already freed.
    #[error("event buffer already freed")]
    EventBufferAlreadyFreed,
    /// Returned when an operation times out.
    #[error("timeout")]
    Timeout,
    /// Returned when the VTL is already enabled.
    #[error("vtl already enabled")]
    VtlAlreadyEnabled,
    /// Returned when a register name is unrecognized.
    #[error("unknown register name")]
    UnknownRegisterName,
    /// Returned when the operation is not implemented.
    #[error("not implemented")]
    NotImplemented,
}

/// Result type alias for TMK operations using `TmkError`.
pub type TmkResult<T> = Result<T, TmkError>;
