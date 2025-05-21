#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TmkErrorType {
    AllocationFailed,
    InvalidParameter,
    EnableVtlFailed,
    SetDefaultCtxFailed,
    StartVpFailed,
    QueueCommandFailed,
    SetupVtlProtectionFailed,
    SetupPartitionVtlFailed,
    SetupInterruptHandlerFailed,
    SetInterruptIdxFailed,
    SetupSecureInterceptFailed,
    ApplyVtlProtectionForMemoryFailed,
    ReadMsrFailed,
    WriteMsrFailed,
    GetRegisterFailed,
    InvalidHypercallCode,
    InvalidHypercallInput,
    InvalidAlignment,
    AccessDenied,
    InvalidPartitionState,
    OperationDenied,
    UnknownProperty,
    PropertyValueOutOfRange,
    InsufficientMemory,
    PartitionTooDeep,
    InvalidPartitionId,
    InvalidVpIndex,
    NotFound,
    InvalidPortId,
    InvalidConnectionId,
    InsufficientBuffers,
    NotAcknowledged,
    InvalidVpState,
    Acknowledged,
    InvalidSaveRestoreState,
    InvalidSynicState,
    ObjectInUse,
    InvalidProximityDomainInfo,
    NoData,
    Inactive,
    NoResources,
    FeatureUnavailable,
    PartialPacket,
    ProcessorFeatureNotSupported,
    ProcessorCacheLineFlushSizeIncompatible,
    InsufficientBuffer,
    IncompatibleProcessor,
    InsufficientDeviceDomains,
    CpuidFeatureValidationError,
    CpuidXsaveFeatureValidationError,
    ProcessorStartupTimeout,
    SmxEnabled,
    InvalidLpIndex,
    InvalidRegisterValue,
    InvalidVtlState,
    NxNotDetected,
    InvalidDeviceId,
    InvalidDeviceState,
    PendingPageRequests,
    PageRequestInvalid,
    KeyAlreadyExists,
    DeviceAlreadyInDomain,
    InvalidCpuGroupId,
    InvalidCpuGroupState,
    OperationFailed,
    NotAllowedWithNestedVirtActive,
    InsufficientRootMemory,
    EventBufferAlreadyFreed,
    Timeout,
    VtlAlreadyEnabled,
    UnknownRegisterName,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TmkError(pub TmkErrorType);

pub type TmkResult<T> = Result<T, TmkError>;

impl core::error::Error for TmkError {}

impl core::fmt::Display for TmkError {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "TmkError({:?})", self.0)
	}
}

impl From<TmkErrorType> for TmkError {
    fn from(e: TmkErrorType) -> Self {
        TmkError(e)
    }
}
