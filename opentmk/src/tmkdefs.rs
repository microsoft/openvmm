use thiserror::Error;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
pub enum TmkError {
    #[error("allocation failed")] 
    AllocationFailed,
    #[error("invalid parameter")] 
    InvalidParameter,
    #[error("failed to enable VTL")] 
    EnableVtlFailed,
    #[error("failed to set default context")] 
    SetDefaultCtxFailed,
    #[error("failed to start VP")] 
    StartVpFailed,
    #[error("failed to queue command")] 
    QueueCommandFailed,
    #[error("failed to set up VTL protection")] 
    SetupVtlProtectionFailed,
    #[error("failed to set up partition VTL")] 
    SetupPartitionVtlFailed,
    #[error("failed to set up interrupt handler")] 
    SetupInterruptHandlerFailed,
    #[error("failed to set interrupt index")] 
    SetInterruptIdxFailed,
    #[error("failed to set up secure intercept")] 
    SetupSecureInterceptFailed,
    #[error("failed to apply VTL protection for memory")] 
    ApplyVtlProtectionForMemoryFailed,
    #[error("failed to read MSR")] 
    ReadMsrFailed,
    #[error("failed to write MSR")] 
    WriteMsrFailed,
    #[error("failed to get register")] 
    GetRegisterFailed,
    #[error("invalid hypercall code")] 
    InvalidHypercallCode,
    #[error("invalid hypercall input")] 
    InvalidHypercallInput,
    #[error("invalid alignment")] 
    InvalidAlignment,
    #[error("access denied")] 
    AccessDenied,
    #[error("invalid partition state")] 
    InvalidPartitionState,
    #[error("operation denied")] 
    OperationDenied,
    #[error("unknown property")] 
    UnknownProperty,
    #[error("property value out of range")] 
    PropertyValueOutOfRange,
    #[error("insufficient memory")] 
    InsufficientMemory,
    #[error("partition too deep")] 
    PartitionTooDeep,
    #[error("invalid partition id")] 
    InvalidPartitionId,
    #[error("invalid VP index")] 
    InvalidVpIndex,
    #[error("not found")] 
    NotFound,
    #[error("invalid port id")] 
    InvalidPortId,
    #[error("invalid connection id")] 
    InvalidConnectionId,
    #[error("insufficient buffers")] 
    InsufficientBuffers,
    #[error("not acknowledged")] 
    NotAcknowledged,
    #[error("invalid VP state")] 
    InvalidVpState,
    #[error("already acknowledged")] 
    Acknowledged,
    #[error("invalid save/restore state")] 
    InvalidSaveRestoreState,
    #[error("invalid synic state")] 
    InvalidSynicState,
    #[error("object in use")] 
    ObjectInUse,
    #[error("invalid proximity domain info")] 
    InvalidProximityDomainInfo,
    #[error("no data")] 
    NoData,
    #[error("inactive")] 
    Inactive,
    #[error("no resources")] 
    NoResources,
    #[error("feature unavailable")] 
    FeatureUnavailable,
    #[error("partial packet")] 
    PartialPacket,
    #[error("processor feature not supported")] 
    ProcessorFeatureNotSupported,
    #[error("processor cache line flush size incompatible")] 
    ProcessorCacheLineFlushSizeIncompatible,
    #[error("insufficient buffer")] 
    InsufficientBuffer,
    #[error("incompatible processor")] 
    IncompatibleProcessor,
    #[error("insufficient device domains")] 
    InsufficientDeviceDomains,
    #[error("cpuid feature validation error")] 
    CpuidFeatureValidationError,
    #[error("cpuid xsave feature validation error")] 
    CpuidXsaveFeatureValidationError,
    #[error("processor startup timeout")] 
    ProcessorStartupTimeout,
    #[error("smx enabled")] 
    SmxEnabled,
    #[error("invalid LP index")] 
    InvalidLpIndex,
    #[error("invalid register value")] 
    InvalidRegisterValue,
    #[error("invalid VTL state")] 
    InvalidVtlState,
    #[error("nx not detected")] 
    NxNotDetected,
    #[error("invalid device id")] 
    InvalidDeviceId,
    #[error("invalid device state")] 
    InvalidDeviceState,
    #[error("pending page requests")] 
    PendingPageRequests,
    #[error("page request invalid")] 
    PageRequestInvalid,
    #[error("key already exists")] 
    KeyAlreadyExists,
    #[error("device already in domain")] 
    DeviceAlreadyInDomain,
    #[error("invalid cpu group id")] 
    InvalidCpuGroupId,
    #[error("invalid cpu group state")] 
    InvalidCpuGroupState,
    #[error("operation failed")] 
    OperationFailed,
    #[error("not allowed with nested virtualization active")] 
    NotAllowedWithNestedVirtActive,
    #[error("insufficient root memory")] 
    InsufficientRootMemory,
    #[error("event buffer already freed")] 
    EventBufferAlreadyFreed,
    #[error("timeout")] 
    Timeout,
    #[error("vtl already enabled")] 
    VtlAlreadyEnabled,
    #[error("unknown register name")] 
    UnknownRegisterName,
    #[error("not implemented")] 
    NotImplemented,
}

pub type TmkResult<T> = Result<T, TmkError>;
