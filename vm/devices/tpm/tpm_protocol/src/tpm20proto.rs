// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TPM 2.0 Protocol types, as defined in the spec

//! NOTE: once the `tpm-rs` project matures, this hand-rolled code should be *deleted* and
//! replaced with types from that `tpm-rs` project.

use self::packed_nums::*;
use bitfield_struct::bitfield;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[expect(non_camel_case_types)]
mod packed_nums {
    pub type u16_be = zerocopy::U16<zerocopy::BigEndian>;
    pub type u32_be = zerocopy::U32<zerocopy::BigEndian>;
    pub type u64_be = zerocopy::U64<zerocopy::BigEndian>;
}

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum InvalidInput {
    #[error("input data size too large for buffer - input size > upper bound: {0} > {1}")]
    BufferSizeTooLarge(usize, usize),
    #[error("input list length too long - input length > upper bound: {0} > {1}")]
    PcrSelectionsLengthTooLong(usize, usize),
    #[error("input payload size too large - input size > upper bound: {0} > {1}")]
    NvPublicPayloadTooLarge(usize, usize),
}

/// Errors returned when TPM protocol helper builders receive invalid inputs.
#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum TpmProtoError {
    #[error("input user_auth to TpmsSensitiveCreate is invalid")]
    TpmsSensitiveCreateUserAuth(#[source] InvalidInput),
    #[error("input data to TpmsSensitiveCreate is invalid")]
    TpmsSensitiveCreateData(#[source] InvalidInput),
    #[error("input auth_policy to TpmtPublic is invalid")]
    TpmtPublicAuthPolicy(#[source] InvalidInput),
    #[error("input unique to TpmtPublic is invalid")]
    TpmtPublicUnique(#[source] InvalidInput),
    #[error("input auth_policy to TpmsNvPublic is invalid")]
    TpmsNvPublicAuthPolicy(#[source] InvalidInput),
    #[error("input PCR allocation to PcrAllocateCmd is invalid")]
    PcrAllocatePcrAllocation(#[source] InvalidInput),
    #[error("input outside_info to CreatePrimaryCmd is invalid")]
    CreatePrimaryOutsideInfo(#[source] InvalidInput),
    #[error("input creation PCR to CreatePrimaryCmd is invalid")]
    CreatePrimaryCreationPcr(#[source] InvalidInput),
    #[error("input auth to NvDefineSpaceCmd is invalid")]
    NvDefineSpaceAuth(#[source] InvalidInput),
    #[error("input public_info to NvDefineSpaceCmd is invalid")]
    NvDefineSpacePublicInfo(#[source] InvalidInput),
    #[error("input data to NvWriteCmd is invalid")]
    NvWriteData(#[source] InvalidInput),
}

/// Errors surfaced when validating TPM replies against expected invariants.
#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum ResponseValidationError {
    #[error("response size is too small to fit into the buffer")]
    ResponseSizeTooSmall,
    #[error(
        "size {size} specified in the response header does not meet the minimal size of command type {expected_size}, command succeeded: {command_succeeded}"
    )]
    HeaderResponseSizeMismatch {
        size: u32,
        expected_size: usize,
        command_succeeded: bool,
    },
    #[error(
        "unexpected session tag {response_session_tag} specified in the response header, expected: {expected_session_tag}, command succeeded: {command_succeeded}"
    )]
    HeaderSessionTagMismatch {
        response_session_tag: u16,
        expected_session_tag: u16,
        command_succeeded: bool,
    },
}

/// Wrapper around TPM reserved handle values (`TPMI_RH_PROVISION`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct ReservedHandle(pub u32_be);

impl PartialEq<ReservedHandle> for u32 {
    fn eq(&self, other: &ReservedHandle) -> bool {
        other.0.get() == *self
    }
}

impl ReservedHandle {
    /// Builds a reserved handle from a handle type and offset.
    pub const fn new(kind: u8, offset: u32) -> ReservedHandle {
        ReservedHandle(new_u32_be((kind as u32) << 24 | offset))
    }
}

/// Handle type identifying NV indexes.
pub const TPM20_HT_NV_INDEX: u8 = 0x01;
/// Handle type identifying permanent handles.
pub const TPM20_HT_PERMANENT: u8 = 0x40;
/// Handle type identifying persistent handles.
pub const TPM20_HT_PERSISTENT: u8 = 0x81;

/// Reserved handle for the owner hierarchy (`TPM_RH_OWNER`).
pub const TPM20_RH_OWNER: ReservedHandle = ReservedHandle::new(TPM20_HT_PERMANENT, 0x01);
/// Reserved handle for the platform hierarchy (`TPM_RH_PLATFORM`).
pub const TPM20_RH_PLATFORM: ReservedHandle = ReservedHandle::new(TPM20_HT_PERMANENT, 0x0c);
/// Reserved handle for the endorsement hierarchy (`TPM_RH_ENDORSEMENT`).
pub const TPM20_RH_ENDORSEMENT: ReservedHandle = ReservedHandle::new(TPM20_HT_PERMANENT, 0x0b);
/// Reserved handle used for password sessions (`TPM_RS_PW`).
pub const TPM20_RS_PW: ReservedHandle = ReservedHandle::new(TPM20_HT_PERMANENT, 0x09);

/// Base NV index reserved for platform manufacturers.
pub const NV_INDEX_RANGE_BASE_PLATFORM_MANUFACTURER: u32 =
    (TPM20_HT_NV_INDEX as u32) << 24 | 0x400000;
/// Base NV index reserved for TCG-assigned use.
pub const NV_INDEX_RANGE_BASE_TCG_ASSIGNED: u32 = (TPM20_HT_NV_INDEX as u32) << 24 | 0xc00000;

/// Suggested maximum size for `TPM2B_MAX_BUFFER` payloads, in bytes.
pub const MAX_DIGEST_BUFFER_SIZE: usize = 1024;

/// Wrapper around TPM session tag values.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SessionTag(pub u16_be);

impl PartialEq<SessionTag> for u16 {
    fn eq(&self, other: &SessionTag) -> bool {
        other.0.get() == *self
    }
}

impl SessionTag {
    const fn new(val: u16) -> SessionTag {
        SessionTag(new_u16_be(val))
    }
}

/// Enumerates well-known TPM session tags.
#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Copy, Clone)]
#[repr(u16)]
pub enum SessionTagEnum {
    // No structure type specified
    Null = 0x8000,

    // A command/response for a command defined in this specification. The
    // command/response has no attached sessions. If a command has an
    // error and the command tag value is either TPM_ST_NO_SESSIONS or
    // TPM_ST_SESSIONS, then this tag value is used for the response code.
    NoSessions = 0x8001,

    // A command/response for a command defined in this specification. The
    // command/response has one or more attached sessions and the sessionOffset
    // field is present.
    Sessions = 0x8002,
    AttestClock = 0x8014,
    AttestCommandAudit = 0x8015,
    AttestSessionAudit = 0x8016,
    AttestCertify = 0x8017,
    AttestQuote = 0x8018,
    AttestTick = 0x8019,
    AttestTickstamp = 0x801A,
    AttestTransport = 0x801B,
    AttestCreation = 0x801C,
    AttestNv = 0x801D,
    // Tickets
    Creation = 0x8021,
    Verified = 0x8022,
    Auth = 0x8023,
    Hashcheck = 0x8024,
    FuManifest = 0x8029,
}

impl From<SessionTagEnum> for SessionTag {
    fn from(x: SessionTagEnum) -> Self {
        SessionTag::new(x as u16)
    }
}

impl SessionTagEnum {
    /// Converts a raw session tag value into a `SessionTagEnum`.
    pub fn from_u16(val: u16) -> Option<SessionTagEnum> {
        let ret = match val {
            0x8000 => Self::Null,
            0x8001 => Self::NoSessions,
            0x8002 => Self::Sessions,
            0x8014 => Self::AttestClock,
            0x8015 => Self::AttestCommandAudit,
            0x8016 => Self::AttestSessionAudit,
            0x8017 => Self::AttestCertify,
            0x8018 => Self::AttestQuote,
            0x8019 => Self::AttestTick,
            0x801A => Self::AttestTickstamp,
            0x801B => Self::AttestTransport,
            0x801C => Self::AttestCreation,
            0x801D => Self::AttestNv,
            0x8021 => Self::Creation,
            0x8022 => Self::Verified,
            0x8023 => Self::Auth,
            0x8024 => Self::Hashcheck,
            0x8029 => Self::FuManifest,
            _ => return None,
        };

        Some(ret)
    }
}

/// Wrapper around TPM command codes (`TPM_CC`).
#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct CommandCode(pub u32_be);

impl PartialEq<CommandCode> for u32 {
    fn eq(&self, other: &CommandCode) -> bool {
        other.0.get() == *self
    }
}

impl CommandCode {
    const fn new(val: u32) -> CommandCode {
        CommandCode(new_u32_be(val))
    }

    /// Attempts to map this command code into a known enum variant.
    pub fn into_enum(self) -> Option<CommandCodeEnum> {
        CommandCodeEnum::from_u32(self.0.get())
    }
}

#[expect(non_camel_case_types, clippy::upper_case_acronyms, missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum CommandCodeEnum {
    NV_UndefineSpaceSpecial = 0x0000011f,
    EvictControl = 0x00000120,
    HierarchyControl = 0x00000121,
    NV_UndefineSpace = 0x00000122,
    ChangeEPS = 0x00000124,
    ChangePPS = 0x00000125,
    Clear = 0x00000126,
    ClearControl = 0x00000127,
    ClockSet = 0x00000128,
    HierarchyChangeAuth = 0x00000129,
    NV_DefineSpace = 0x0000012a,
    PCR_Allocate = 0x0000012b,
    PCR_SetAuthPolicy = 0x0000012c,
    PP_Commands = 0x0000012d,
    SetPrimaryPolicy = 0x0000012e,
    FieldUpgradeStart = 0x0000012f,
    ClockRateAdjust = 0x00000130,
    CreatePrimary = 0x00000131,
    NV_GlobalWriteLock = 0x00000132,
    GetCommandAuditDigest = 0x00000133,
    NV_Increment = 0x00000134,
    NV_SetBits = 0x00000135,
    NV_Extend = 0x00000136,
    NV_Write = 0x00000137,
    NV_WriteLock = 0x00000138,
    DictionaryAttackLockReset = 0x00000139,
    DictionaryAttackParameters = 0x0000013a,
    NV_ChangeAuth = 0x0000013b,
    PCR_Event = 0x0000013c,
    PCR_Reset = 0x0000013d,
    SequenceComplete = 0x0000013e,
    SetAlgorithmSet = 0x0000013f,
    SetCommandCodeAuditStatus = 0x00000140,
    FieldUpgradeData = 0x00000141,
    IncrementalSelfTest = 0x00000142,
    SelfTest = 0x00000143,
    Startup = 0x00000144,
    Shutdown = 0x00000145,
    StirRandom = 0x00000146,
    ActivateCredential = 0x00000147,
    Certify = 0x00000148,
    PolicyNV = 0x00000149,
    CertifyCreation = 0x0000014a,
    Duplicate = 0x0000014b,
    GetTime = 0x0000014c,
    GetSessionAuditDigest = 0x0000014d,
    NV_Read = 0x0000014e,
    NV_ReadLock = 0x0000014f,
    ObjectChangeAuth = 0x00000150,
    PolicySecret = 0x00000151,
    Rewrap = 0x00000152,
    Create = 0x00000153,
    ECDH_ZGen = 0x00000154,
    HMAC = 0x00000155,
    Import = 0x00000156,
    Load = 0x00000157,
    Quote = 0x00000158,
    RSA_Decrypt = 0x00000159,
    HMAC_Start = 0x0000015b,
    SequenceUpdate = 0x0000015c,
    Sign = 0x0000015d,
    Unseal = 0x0000015e,
    PolicySigned = 0x00000160,
    ContextLoad = 0x00000161,
    ContextSave = 0x00000162,
    ECDH_KeyGen = 0x00000163,
    EncryptDecrypt = 0x00000164,
    FlushContext = 0x00000165,
    LoadExternal = 0x00000167,
    MakeCredential = 0x00000168,
    NV_ReadPublic = 0x00000169,
    PolicyAuthorize = 0x0000016a,
    PolicyAuthValue = 0x0000016b,
    PolicyCommandCode = 0x0000016c,
    PolicyCounterTimer = 0x0000016d,
    PolicyCpHash = 0x0000016e,
    PolicyLocality = 0x0000016f,
    PolicyNameHash = 0x00000170,
    PolicyOR = 0x00000171,
    PolicyTicket = 0x00000172,
    ReadPublic = 0x00000173,
    RSA_Encrypt = 0x00000174,
    StartAuthSession = 0x00000176,
    VerifySignature = 0x00000177,
    ECC_Parameters = 0x00000178,
    FirmwareRead = 0x00000179,
    GetCapability = 0x0000017a,
    GetRandom = 0x0000017b,
    GetTestResult = 0x0000017c,
    Hash = 0x0000017d,
    PCR_Read = 0x0000017e,
    PolicyPCR = 0x0000017f,
    PolicyRestart = 0x00000180,
    ReadClock = 0x00000181,
    PCR_Extend = 0x00000182,
    PCR_SetAuthValue = 0x00000183,
    NV_Certify = 0x00000184,
    EventSequenceComplete = 0x00000185,
    HashSequenceStart = 0x00000186,
    PolicyPhysicalPresence = 0x00000187,
    PolicyDuplicationSelect = 0x00000188,
    PolicyGetDigest = 0x00000189,
    TestParms = 0x0000018a,
    Commit = 0x0000018b,
    PolicyPassword = 0x0000018c,
    ZGen_2Phase = 0x0000018d,
    EC_Ephemeral = 0x0000018e,
    PolicyNvWritten = 0x0000018f,
    PolicyTemplate = 0x00000190,
    CreateLoaded = 0x00000191,
    PolicyAuthorizeNV = 0x00000192,
    EncryptDecrypt2 = 0x00000193,
    AC_GetCapability = 0x00000194,
    AC_Send = 0x00000195,
    Policy_AC_SendSelect = 0x00000196,
    CertifyX509 = 0x00000197,
    ACT_SetTimeout = 0x00000198,
}

impl From<CommandCodeEnum> for CommandCode {
    fn from(x: CommandCodeEnum) -> Self {
        CommandCode::new(x as u32)
    }
}

impl CommandCodeEnum {
    /// Create a CommandCodeEnum from a u32 value.
    pub fn from_u32(val: u32) -> Option<CommandCodeEnum> {
        let ret = match val {
            0x0000011f => Self::NV_UndefineSpaceSpecial,
            0x00000120 => Self::EvictControl,
            0x00000121 => Self::HierarchyControl,
            0x00000122 => Self::NV_UndefineSpace,
            0x00000124 => Self::ChangeEPS,
            0x00000125 => Self::ChangePPS,
            0x00000126 => Self::Clear,
            0x00000127 => Self::ClearControl,
            0x00000128 => Self::ClockSet,
            0x00000129 => Self::HierarchyChangeAuth,
            0x0000012a => Self::NV_DefineSpace,
            0x0000012b => Self::PCR_Allocate,
            0x0000012c => Self::PCR_SetAuthPolicy,
            0x0000012d => Self::PP_Commands,
            0x0000012e => Self::SetPrimaryPolicy,
            0x0000012f => Self::FieldUpgradeStart,
            0x00000130 => Self::ClockRateAdjust,
            0x00000131 => Self::CreatePrimary,
            0x00000132 => Self::NV_GlobalWriteLock,
            0x00000133 => Self::GetCommandAuditDigest,
            0x00000134 => Self::NV_Increment,
            0x00000135 => Self::NV_SetBits,
            0x00000136 => Self::NV_Extend,
            0x00000137 => Self::NV_Write,
            0x00000138 => Self::NV_WriteLock,
            0x00000139 => Self::DictionaryAttackLockReset,
            0x0000013a => Self::DictionaryAttackParameters,
            0x0000013b => Self::NV_ChangeAuth,
            0x0000013c => Self::PCR_Event,
            0x0000013d => Self::PCR_Reset,
            0x0000013e => Self::SequenceComplete,
            0x0000013f => Self::SetAlgorithmSet,
            0x00000140 => Self::SetCommandCodeAuditStatus,
            0x00000141 => Self::FieldUpgradeData,
            0x00000142 => Self::IncrementalSelfTest,
            0x00000143 => Self::SelfTest,
            0x00000144 => Self::Startup,
            0x00000145 => Self::Shutdown,
            0x00000146 => Self::StirRandom,
            0x00000147 => Self::ActivateCredential,
            0x00000148 => Self::Certify,
            0x00000149 => Self::PolicyNV,
            0x0000014a => Self::CertifyCreation,
            0x0000014b => Self::Duplicate,
            0x0000014c => Self::GetTime,
            0x0000014d => Self::GetSessionAuditDigest,
            0x0000014e => Self::NV_Read,
            0x0000014f => Self::NV_ReadLock,
            0x00000150 => Self::ObjectChangeAuth,
            0x00000151 => Self::PolicySecret,
            0x00000152 => Self::Rewrap,
            0x00000153 => Self::Create,
            0x00000154 => Self::ECDH_ZGen,
            0x00000155 => Self::HMAC,
            0x00000156 => Self::Import,
            0x00000157 => Self::Load,
            0x00000158 => Self::Quote,
            0x00000159 => Self::RSA_Decrypt,
            0x0000015b => Self::HMAC_Start,
            0x0000015c => Self::SequenceUpdate,
            0x0000015d => Self::Sign,
            0x0000015e => Self::Unseal,
            0x00000160 => Self::PolicySigned,
            0x00000161 => Self::ContextLoad,
            0x00000162 => Self::ContextSave,
            0x00000163 => Self::ECDH_KeyGen,
            0x00000164 => Self::EncryptDecrypt,
            0x00000165 => Self::FlushContext,
            0x00000167 => Self::LoadExternal,
            0x00000168 => Self::MakeCredential,
            0x00000169 => Self::NV_ReadPublic,
            0x0000016a => Self::PolicyAuthorize,
            0x0000016b => Self::PolicyAuthValue,
            0x0000016c => Self::PolicyCommandCode,
            0x0000016d => Self::PolicyCounterTimer,
            0x0000016e => Self::PolicyCpHash,
            0x0000016f => Self::PolicyLocality,
            0x00000170 => Self::PolicyNameHash,
            0x00000171 => Self::PolicyOR,
            0x00000172 => Self::PolicyTicket,
            0x00000173 => Self::ReadPublic,
            0x00000174 => Self::RSA_Encrypt,
            0x00000176 => Self::StartAuthSession,
            0x00000177 => Self::VerifySignature,
            0x00000178 => Self::ECC_Parameters,
            0x00000179 => Self::FirmwareRead,
            0x0000017a => Self::GetCapability,
            0x0000017b => Self::GetRandom,
            0x0000017c => Self::GetTestResult,
            0x0000017d => Self::Hash,
            0x0000017e => Self::PCR_Read,
            0x0000017f => Self::PolicyPCR,
            0x00000180 => Self::PolicyRestart,
            0x00000181 => Self::ReadClock,
            0x00000182 => Self::PCR_Extend,
            0x00000183 => Self::PCR_SetAuthValue,
            0x00000184 => Self::NV_Certify,
            0x00000185 => Self::EventSequenceComplete,
            0x00000186 => Self::HashSequenceStart,
            0x00000187 => Self::PolicyPhysicalPresence,
            0x00000188 => Self::PolicyDuplicationSelect,
            0x00000189 => Self::PolicyGetDigest,
            0x0000018a => Self::TestParms,
            0x0000018b => Self::Commit,
            0x0000018c => Self::PolicyPassword,
            0x0000018d => Self::ZGen_2Phase,
            0x0000018e => Self::EC_Ephemeral,
            0x0000018f => Self::PolicyNvWritten,
            0x00000190 => Self::PolicyTemplate,
            0x00000191 => Self::CreateLoaded,
            0x00000192 => Self::PolicyAuthorizeNV,
            0x00000193 => Self::EncryptDecrypt2,
            0x00000194 => Self::AC_GetCapability,
            0x00000195 => Self::AC_Send,
            0x00000196 => Self::Policy_AC_SendSelect,
            0x00000197 => Self::CertifyX509,
            0x00000198 => Self::ACT_SetTimeout,
            _ => return None,
        };

        Some(ret)
    }
}

const FLAG_FMT1: u32 = 0x0080;
const FLAG_VER1: u32 = 0x0100;
const FLAG_WARN: u32 = 0x0800 + FLAG_VER1;

/// TPM response codes returned in a `ReplyHeader`.
#[repr(u32)]
pub enum ResponseCode {
    /// Success
    Success = 0x000,
    /// The given handle value is not valid or cannot be used for this
    /// command.
    Value = FLAG_FMT1 + 0x004,
    /// Hierarchy is not enabled or is not correct for the use.
    Hierarchy = FLAG_FMT1 + 0x0005,
    /// The handle is not correct for the use.
    Handle = FLAG_FMT1 + 0x000B,
    /// The authorization HMAC check failed.
    AuthFail = FLAG_FMT1 + 0x000E,
    /// Structure is the wrong size.
    Size = FLAG_FMT1 + 0x0015,
    /// The TPM was unable to unmarshal a value because there were not
    /// enough bytes in the input buffer.
    Insufficient = FLAG_FMT1 + 0x001A,
    /// Integrity check fail.
    Integrity = FLAG_FMT1 + 0x001F,
    /// TPM is in failure mode.
    Failure = FLAG_VER1 + 0x0001,
    /// Use of an authorization session with a context command.
    AuthContext = FLAG_VER1 + 0x0045,
    /// The NV index is used before being initialized or the state saved by
    /// TPM20_CC_Shutdown could not be restored.
    NvUninitialized = FLAG_VER1 + 0x04A,
    /// ...
    Sensitive = FLAG_VER1 + 0x055,
    /// Gap for session context ID is too large.
    ContextGap = FLAG_WARN + 0x001,
    /// Out of memory for object contexts.
    ObjectMemory = FLAG_WARN + 0x002,
    /// Out of memory for session contexts.
    SessionMemory = FLAG_WARN + 0x003,
    /// Out of shared object/session memory or need space for internal
    /// operations.
    Memory = FLAG_WARN + 0x004,
    /// Out of session handles - a session must be flushed before a new
    /// session may be created.
    SessionHandles = FLAG_WARN + 0x005,
    /// Out of object handles - the handle space for objects is depleted and
    /// a reboot is required .
    /// NOTE:This cannot occur on the reference implementation.
    ObjectHandles = FLAG_WARN + 0x006,
    /// The TPM has suspended operation on the command. Forward progress was
    /// made and the command may be retried.
    Yielded = FLAG_WARN + 0x008,
    /// The command was cancelled. The command may be retried.
    Cancelled = FLAG_WARN + 0x009,
    /// TPM is performing self tests.
    Testing = FLAG_WARN + 0x00A,
    /// The TPM is rate-limiting accesses to prevent wearout of NV.
    NvRate = FLAG_WARN + 0x020,
    /// Commands are not being accepted because the TPM is in DA lockout
    /// mode.
    Lockout = FLAG_WARN + 0x021,
    /// The TPM was not able to start the command. Retry might work.
    Retry = FLAG_WARN + 0x022,
    /// The command may require writing of NV and NV is not current
    /// accessible.
    NvUnavailable = FLAG_WARN + 0x023,
    /// This value is reserved and shall not be returned by the TPM.
    NotUsed = FLAG_WARN + 0x07F,
    /// Add to a parameter-, handle-, or session-related error.
    Rc1 = 0x100,
}

impl ResponseCode {
    /// Converts a raw TPM response code into a `ResponseCode` value.
    pub fn from_u32(val: u32) -> Option<ResponseCode> {
        let ret = match val {
            x if x == ResponseCode::Success as u32 => ResponseCode::Success,
            x if x == ResponseCode::Value as u32 => ResponseCode::Value,
            x if x == ResponseCode::Hierarchy as u32 => ResponseCode::Hierarchy,
            x if x == ResponseCode::Handle as u32 => ResponseCode::Handle,
            x if x == ResponseCode::AuthFail as u32 => ResponseCode::AuthFail,
            x if x == ResponseCode::Size as u32 => ResponseCode::Size,
            x if x == ResponseCode::Insufficient as u32 => ResponseCode::Insufficient,
            x if x == ResponseCode::Integrity as u32 => ResponseCode::Integrity,
            x if x == ResponseCode::Failure as u32 => ResponseCode::Failure,
            x if x == ResponseCode::AuthContext as u32 => ResponseCode::AuthContext,
            x if x == ResponseCode::NvUninitialized as u32 => ResponseCode::NvUninitialized,
            x if x == ResponseCode::Sensitive as u32 => ResponseCode::Sensitive,
            x if x == ResponseCode::ContextGap as u32 => ResponseCode::ContextGap,
            x if x == ResponseCode::ObjectMemory as u32 => ResponseCode::ObjectMemory,
            x if x == ResponseCode::SessionMemory as u32 => ResponseCode::SessionMemory,
            x if x == ResponseCode::Memory as u32 => ResponseCode::Memory,
            x if x == ResponseCode::SessionHandles as u32 => ResponseCode::SessionHandles,
            x if x == ResponseCode::ObjectHandles as u32 => ResponseCode::ObjectHandles,
            x if x == ResponseCode::Yielded as u32 => ResponseCode::Yielded,
            x if x == ResponseCode::Cancelled as u32 => ResponseCode::Cancelled,
            x if x == ResponseCode::Testing as u32 => ResponseCode::Testing,
            x if x == ResponseCode::NvRate as u32 => ResponseCode::NvRate,
            x if x == ResponseCode::Lockout as u32 => ResponseCode::Lockout,
            x if x == ResponseCode::Retry as u32 => ResponseCode::Retry,
            x if x == ResponseCode::NvUnavailable as u32 => ResponseCode::NvUnavailable,
            x if x == ResponseCode::NotUsed as u32 => ResponseCode::NotUsed,
            _ => return None,
        };
        Some(ret)
    }
}

/// Wrapper around a TPM algorithm identifier (`TPM_ALG_ID`).
#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct AlgId(pub u16_be);

impl PartialEq<AlgId> for u16 {
    fn eq(&self, other: &AlgId) -> bool {
        other.0.get() == *self
    }
}

impl AlgId {
    const fn new(val: u16) -> AlgId {
        AlgId(new_u16_be(val))
    }
}

/// Individual algorithm identifiers recognized by the TPM.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug)]
#[repr(u16)]
pub enum AlgIdEnum {
    /// `$TPM_ALG_RSA` - RSA asymmetric algorithm family.
    RSA = 0x0001,
    /// `$TPM_ALG_SHA1` - SHA-1 hashing algorithm.
    SHA = 0x0004,
    /// `$TPM_ALG_AES` - AES symmetric cipher family.
    AES = 0x0006,
    /// `$TPM_ALG_SHA256` - SHA-256 hashing algorithm.
    SHA256 = 0x000b,
    /// `$TPM_ALG_SHA384` - SHA-384 hashing algorithm.
    SHA384 = 0x000c,
    /// `$TPM_ALG_SHA512` - SHA-512 hashing algorithm.
    SHA512 = 0x000d,
    /// `$TPM_ALG_NULL` - indicates the absence of an algorithm.
    NULL = 0x0010,
    /// `$TPM_ALG_SM3_256` - SM3-256 hashing algorithm.
    SM3_256 = 0x0012,
    /// `$TPM_ALG_RSASSA` - RSA signature scheme.
    RSASSA = 0x0014,
    /// `$TPM_ALG_CFB` - Cipher feedback mode for symmetric ciphers.
    CFB = 0x0043,
}

impl From<AlgIdEnum> for AlgId {
    fn from(x: AlgIdEnum) -> Self {
        AlgId::new(x as u16)
    }
}

impl AlgIdEnum {
    /// Converts a raw algorithm identifier into an `AlgIdEnum`.
    pub fn from_u16(val: u16) -> Option<AlgIdEnum> {
        let ret = match val {
            0x0004 => Self::SHA,
            0x000b => Self::SHA256,
            0x000c => Self::SHA384,
            0x000d => Self::SHA512,
            0x0012 => Self::SM3_256,
            _ => return None,
        };

        Some(ret)
    }
}

/// `TPMA_OBJECT`
#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct TpmaObject(pub u32_be);

impl TpmaObject {
    const fn new(val: u32) -> Self {
        Self(new_u32_be(val))
    }
}

impl From<TpmaObjectBits> for TpmaObject {
    fn from(x: TpmaObjectBits) -> Self {
        let val: u32 = x.into();
        Self::new(val)
    }
}

impl From<u32> for TpmaObject {
    fn from(x: u32) -> Self {
        Self::new(x)
    }
}

/// Bitfield representation of the `TPMA_OBJECT` attribute mask.
#[bitfield(u32)]
pub struct TpmaObjectBits {
    _reserved0: bool,
    pub fixed_tpm: bool,
    pub st_clear: bool,
    _reserved1: bool,
    pub fixed_parent: bool,
    pub sensitive_data_origin: bool,
    pub user_with_auth: bool,
    pub admin_with_policy: bool,
    #[bits(2)]
    _reserved2: u8,
    pub no_da: bool,
    pub encrypted_duplication: bool,
    #[bits(4)]
    _reserved3: u8,
    pub restricted: bool,
    pub decrypt: bool,
    pub sign_encrypt: bool,
    #[bits(13)]
    _reserved4: u16,
}

/// `TPMA_NV`
#[repr(transparent)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct TpmaNv(pub u32_be);

impl TpmaNv {
    const fn new(val: u32) -> Self {
        Self(new_u32_be(val))
    }
}

impl From<TpmaNvBits> for TpmaNv {
    fn from(x: TpmaNvBits) -> Self {
        let val: u32 = x.into();
        Self::new(val)
    }
}

impl From<u32> for TpmaNv {
    fn from(x: u32) -> Self {
        Self::new(x)
    }
}

/// Bitfield representation of the `TPMA_NV` attribute mask.
#[bitfield(u32)]
pub struct TpmaNvBits {
    pub nv_ppwrite: bool,
    pub nv_ownerwrite: bool,
    pub nv_authwrite: bool,
    pub nv_policywrite: bool,
    // bits 7:4: `TPM_NT`
    // 0001 - `tpm_nt_counter`
    pub nt_counter: bool,
    // 0010 - `tpm_nt_bits`
    pub nt_bits: bool,
    // 0100 - `tpm_nt_extend`
    pub nt_extend: bool,
    _unused0: bool,
    // bits 9:8 are reserved
    #[bits(2)]
    _reserved1: u8,
    pub nv_policy_delete: bool,
    pub nv_writelocked: bool,
    pub nv_writeall: bool,
    pub nv_writedefine: bool,
    pub nv_write_stclear: bool,
    pub nv_globallock: bool,
    pub nv_ppread: bool,
    pub nv_ownerread: bool,
    pub nv_authread: bool,
    pub nv_policyread: bool,
    // bits 24:20 are reserved
    #[bits(5)]
    _reserved2: u8,
    pub nv_no_da: bool,
    pub nv_orderly: bool,
    pub nv_clear_stclear: bool,
    pub nv_readlocked: bool,
    pub nv_written: bool,
    pub nv_platformcreate: bool,
    pub nv_read_stclear: bool,
}

/// Workaround to allow constructing a zerocopy U64 in a const context.
const fn new_u64_be(val: u64) -> u64_be {
    u64_be::from_bytes(val.to_be_bytes())
}

/// Workaround to allow constructing a zerocopy U32 in a const context.
const fn new_u32_be(val: u32) -> u32_be {
    u32_be::from_bytes(val.to_be_bytes())
}

/// Workaround to allow constructing a zerocopy U16 in a const context.
const fn new_u16_be(val: u16) -> u16_be {
    u16_be::from_bytes(val.to_be_bytes())
}

/// TPM command / response definitions
pub mod protocol {
    use super::*;

    /// Common structs shared between multiple command / response structs
    pub mod common {
        use super::*;

        /// Wire-level header that precedes every TPM command buffer.
        #[repr(C)]
        #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub struct CmdHeader {
            /// Identifies whether a command includes one or more sessions.
            pub session_tag: SessionTag,
            /// Total size in bytes of the serialized command structure.
            pub size: u32_be,
            /// Numeric identifier of the TPM command being invoked.
            pub command_code: CommandCode,
        }

        impl CmdHeader {
            /// Constructs a header for a fixed-size command payload.
            pub fn new<Cmd: Sized>(
                session_tag: SessionTag,
                command_code: CommandCode,
            ) -> CmdHeader {
                CmdHeader {
                    session_tag,
                    size: (size_of::<Cmd>() as u32).into(),
                    command_code,
                }
            }
        }

        /// Wire-level header returned with every TPM command response.
        #[repr(C)]
        #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub struct ReplyHeader {
            /// Session tag echoed by the TPM for the response.
            pub session_tag: u16_be,
            /// Total size in bytes of the serialized response buffer.
            pub size: u32_be,
            /// Result code describing success or failure of the command.
            pub response_code: u32_be,
        }

        impl ReplyHeader {
            /// Performs a few command-agnostic validation checks:
            /// - Ensures the size matches the size_of the provided `FullReply` type
            /// - Compares provided session_tag
            ///
            /// Returns Ok(bool) if the validation passes. The bool value indicates whether
            /// the response_code is [`ResponseCode::Success`] or not.
            /// Returns Err(ResponseValidationError) otherwise.
            pub fn base_validation(
                &self,
                session_tag: SessionTag,
                expected_size: u32,
            ) -> Result<bool, ResponseValidationError> {
                // Response code other than Success indicates that the command fails
                // See Section 6.2, "Trusted Platform Module Library Part 3: Commands", revision 1.38.
                let command_succeeded = ResponseCode::from_u32(self.response_code.get())
                    .map(|c| matches!(c, ResponseCode::Success))
                    .unwrap_or(false);

                let (expected_tag, expected_size) = if command_succeeded {
                    (session_tag, expected_size as usize)
                } else {
                    // If the command fails, the expected tag should be NoSessions and the minimal size
                    // of the response should be the size of the header.
                    // See Section 6.1, "Trusted Platform Module Library Part 3: Commands", revision 1.38.
                    //
                    // DEVNOTE: we do not handle the special case caused by sending unsupported commands where
                    // the session tag will be `TPM_RC_BAD_TAG` instead.
                    (SessionTagEnum::NoSessions.into(), size_of::<Self>())
                };

                if self.session_tag.get() != expected_tag {
                    Err(ResponseValidationError::HeaderSessionTagMismatch {
                        response_session_tag: self.session_tag.get(),
                        expected_session_tag: session_tag.0.get(),
                        command_succeeded,
                    })?
                }

                // Allow the size specified in the header to be equal to or larger than the expected size in case
                // that the expected size does not take the authorization area into account.
                if (self.size.get() as usize) < expected_size {
                    Err(ResponseValidationError::HeaderResponseSizeMismatch {
                        size: self.size.get(),
                        expected_size,
                        command_succeeded,
                    })?
                }

                Ok(command_succeeded)
            }
        }

        /// Authorization block accompanying a TPM command.
        #[repr(C)]
        #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub struct CmdAuth {
            /// Handle being used to authorize the command.
            handle: ReservedHandle,
            /// Size of the caller-provided nonce.
            nonce_2b: u16_be,
            /// Session attributes associated with the authorization.
            session: u8,
            /// Size of the authorization buffer.
            auth_2b: u16_be,
        }

        impl CmdAuth {
            /// Builds a command authorization descriptor from raw pieces.
            pub fn new(handle: ReservedHandle, nonce_2b: u16, session: u8, auth_2b: u16) -> Self {
                CmdAuth {
                    handle,
                    nonce_2b: nonce_2b.into(),
                    session,
                    auth_2b: auth_2b.into(),
                }
            }
        }

        /// Authorization block returned with a TPM command response.
        #[repr(C)]
        #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub struct ReplyAuth {
            /// Size of the TPM-provided nonce.
            pub nonce_2b: u16_be,
            /// Session attributes echoed by the TPM.
            pub session: u8,
            /// Size of the returned authorization buffer.
            pub auth_2b: u16_be,
        }
    }

    use common::CmdHeader;
    use common::ReplyHeader;

    /// Marker trait for a struct that corresponds to a TPM Command
    pub trait TpmCommand: IntoBytes + FromBytes + Sized + Immutable + KnownLayout {
        /// Reply type returned when the command is executed.
        type Reply: TpmReply;

        /// Validates and deserializes a raw reply buffer into the strongly typed reply.
        fn base_validate_reply(
            reply_buf: &[u8],
            session_tag: impl Into<SessionTag>,
        ) -> Result<(Self::Reply, bool), ResponseValidationError> {
            let res = Self::Reply::deserialize(reply_buf)
                .ok_or(ResponseValidationError::ResponseSizeTooSmall)?;
            let succeeded = res.base_validation(session_tag.into())?;

            Ok((res, succeeded))
        }
    }

    /// Marker trait for a struct that corresponds to a TPM Reply
    pub trait TpmReply: IntoBytes + FromBytes + Sized + Immutable + KnownLayout {
        /// Command type that produced this reply.
        type Command: TpmCommand;

        /// Performs generic validation of the reply header.
        fn base_validation(
            &self,
            session_tag: SessionTag,
        ) -> Result<bool, ResponseValidationError> {
            // `Reply::deserialize` guarantees this should not fail
            let header = ReplyHeader::ref_from_prefix(self.as_bytes())
                .expect("unexpected response size")
                .0; // TODO: zerocopy: error (https://github.com/microsoft/openvmm/issues/759)
            header.base_validation(session_tag, self.payload_size() as u32)
        }
        /// Attempts to parse a reply structure from raw bytes.
        fn deserialize(bytes: &[u8]) -> Option<Self>;
        /// Returns the size of the payload area (bytes after the reply header).
        fn payload_size(&self) -> usize;
    }

    /// General type for TPM 2.0 sized buffers.
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct Tpm2bBuffer {
        /// Length of data stored in `buffer`.
        pub size: u16_be,
        // Use value that is large enough as the buffer size so that we
        // only need to define one struct.
        /// Raw bytes backing the buffer.
        pub buffer: [u8; MAX_DIGEST_BUFFER_SIZE],
    }

    impl Tpm2bBuffer {
        /// Create a `Tpm2bBuffer` from a slice.
        pub fn new(data: &[u8]) -> Result<Self, InvalidInput> {
            let size = data.len();
            if size > MAX_DIGEST_BUFFER_SIZE {
                Err(InvalidInput::BufferSizeTooLarge(
                    size,
                    MAX_DIGEST_BUFFER_SIZE,
                ))?
            }

            let mut buffer = [0u8; MAX_DIGEST_BUFFER_SIZE];
            buffer[..size].copy_from_slice(data);

            Ok(Self {
                size: new_u16_be(size as u16),
                buffer,
            })
        }

        /// Serializes the buffer into the TPM 2B wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.size.as_bytes());
            buffer.extend_from_slice(&self.buffer[..self.size.get() as usize]);

            buffer
        }

        /// Parses a TPM 2B buffer from bytes.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<u16_be>();
            if bytes.len() < end {
                return None;
            }

            let size: u16 = u16_be::read_from_bytes(&bytes[start..end]).ok()?.into(); // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)
            if size as usize > MAX_DIGEST_BUFFER_SIZE {
                return None;
            }

            start = end;
            end += size as usize;
            if bytes.len() < end {
                return None;
            }
            let mut buffer = [0u8; MAX_DIGEST_BUFFER_SIZE];
            buffer[..size as usize].copy_from_slice(&bytes[start..end]);

            Some(Self {
                size: size.into(),
                buffer,
            })
        }

        /// Returns the number of payload bytes occupied by this structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.size);
            payload_size += self.size.get() as usize;

            payload_size
        }
    }

    /// `TPML_PCR_SELECTION`
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmlPcrSelection {
        /// Number of valid `PcrSelection` entries in `pcr_selections`.
        pub count: u32_be,
        /// Fixed array containing individual PCR selection descriptors.
        pub pcr_selections: [PcrSelection; 5],
    }

    impl TpmlPcrSelection {
        /// Builds a PCR selection list from a slice of PCR selections.
        pub fn new(pcr_selections: &[PcrSelection]) -> Result<Self, InvalidInput> {
            let count = pcr_selections.len();
            if count > 5 {
                Err(InvalidInput::PcrSelectionsLengthTooLong(count, 5))?
            }

            let mut base = [PcrSelection::new_zeroed(); 5];
            base[..count].copy_from_slice(pcr_selections);

            Ok(Self {
                count: new_u32_be(count as u32),
                pcr_selections: base,
            })
        }

        /// Serializes the PCR selection list into bytes.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.count.as_bytes());
            for i in 0..self.count.get() {
                buffer.extend_from_slice(&self.pcr_selections[i as usize].serialize());
            }

            buffer
        }

        /// Attempts to parse a PCR selection list from bytes.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<u32_be>();

            if bytes.len() < end {
                return None;
            }

            let count: u32 = u32_be::read_from_bytes(&bytes[start..end]).ok()?.into(); // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)
            if count > 5 {
                return None;
            }

            let mut pcr_selections = [PcrSelection::new_zeroed(); 5];
            for i in 0..count {
                start = end;
                pcr_selections[i as usize] = PcrSelection::deserialize(&bytes[start..])?;
                end += pcr_selections[i as usize].payload_size();
            }

            Some(Self {
                count: count.into(),
                pcr_selections,
            })
        }

        /// Returns the number of bytes occupied by the serialized structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;
            let count = self.count;

            payload_size += size_of_val(&count);
            for i in 0..count.get() {
                payload_size += self.pcr_selections[i as usize].payload_size();
            }

            payload_size
        }
    }

    /// `TPMS_SENSITIVE_CREATE`
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmsSensitiveCreate {
        user_auth: Tpm2bBuffer,
        data: Tpm2bBuffer,
    }

    impl TpmsSensitiveCreate {
        /// Builds the sensitive payload using caller-provided auth and seed material.
        pub fn new(user_auth: &[u8], data: &[u8]) -> Result<Self, TpmProtoError> {
            let user_auth =
                Tpm2bBuffer::new(user_auth).map_err(TpmProtoError::TpmsSensitiveCreateUserAuth)?;
            let data = Tpm2bBuffer::new(data).map_err(TpmProtoError::TpmsSensitiveCreateData)?;
            Ok(Self { user_auth, data })
        }

        /// Serializes the structure into a TPM buffer.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(&self.user_auth.serialize());
            buffer.extend_from_slice(&self.data.serialize());

            buffer
        }

        /// Returns the payload length of the serialized structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += self.user_auth.payload_size();
            payload_size += self.data.payload_size();

            payload_size
        }
    }

    /// `TPM2B_SENSITIVE_CREATE`
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct Tpm2bSensitiveCreate {
        size: u16_be,
        sensitive: TpmsSensitiveCreate,
    }

    impl Tpm2bSensitiveCreate {
        /// Wraps a sensitive payload with its serialized size.
        pub fn new(sensitive: TpmsSensitiveCreate) -> Self {
            let size = sensitive.payload_size() as u16;
            Self {
                size: size.into(),
                sensitive,
            }
        }

        /// Serializes the wrapped sensitive payload.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.size.as_bytes());
            buffer.extend_from_slice(&self.sensitive.serialize());

            buffer
        }

        /// Returns the size in bytes of the encoded structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;
            let size = self.size;

            payload_size += size_of_val(&size);
            payload_size += self.sensitive.payload_size();

            payload_size
        }
    }

    /// `TPMT_RSA_SCHEME`
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout, PartialEq)]
    pub struct TpmtRsaScheme {
        scheme: AlgId,
        hash_alg: AlgId,
    }

    impl TpmtRsaScheme {
        /// Create a TpmtRsaScheme from its components.
        pub fn new(scheme: AlgId, hash_alg: Option<AlgId>) -> Self {
            let hash_alg = hash_alg.unwrap_or_else(|| AlgId::new(0));

            Self { scheme, hash_alg }
        }

        /// Serializes the structure into a byte buffer.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.scheme.as_bytes());

            // No parameters when algorithm is NULL
            if self.scheme != AlgIdEnum::NULL.into() {
                // Only support scheme with hash (e.g., RSASSA) for now
                buffer.extend_from_slice(self.hash_alg.as_bytes());
            }

            buffer
        }

        /// Deserializes the structure from a byte buffer.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<AlgId>();

            if bytes.len() < end {
                return None;
            }

            let scheme = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            let hash_alg = if scheme != AlgIdEnum::NULL.into() {
                start = end;
                end += size_of::<AlgId>();
                AlgId::read_from_prefix(&bytes[start..end]).ok()?.0 // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
            } else {
                AlgId::new(0)
            };

            Some(Self { scheme, hash_alg })
        }

        /// Returns the size in bytes of the encoded structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.scheme);

            if self.scheme != AlgIdEnum::NULL.into() {
                payload_size += size_of_val(&self.hash_alg);
            }

            payload_size
        }
    }

    /// `TPMT_SYM_DEF_OBJECT`
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout, PartialEq)]
    pub struct TpmtSymDefObject {
        algorithm: AlgId,
        key_bits: u16_be,
        mode: AlgId,
    }

    impl TpmtSymDefObject {
        /// Creates a new TpmtSymDefObject.
        pub fn new(algorithm: AlgId, key_bits: Option<u16>, mode: Option<AlgId>) -> Self {
            let key_bits = key_bits.map_or_else(|| new_u16_be(0), |v| v.into());
            let mode = mode.unwrap_or_else(|| AlgId::new(0));

            Self {
                algorithm,
                key_bits,
                mode,
            }
        }

        /// Serializes the structure into a byte buffer.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.algorithm.as_bytes());

            // No parameters when algorithm is NULL
            if self.algorithm != AlgIdEnum::NULL.into() {
                buffer.extend_from_slice(self.key_bits.as_bytes());
                buffer.extend_from_slice(self.mode.as_bytes());
            }

            buffer
        }

        /// Deserializes the structure from a byte buffer.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<AlgId>();

            if bytes.len() < end {
                return None;
            }

            let algorithm = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            let (key_bits, mode) = if algorithm != AlgIdEnum::NULL.into() {
                start = end;
                end += size_of::<u16_be>();
                let key_bits = u16_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

                start = end;
                end += size_of::<AlgId>();
                let mode = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

                (key_bits, mode)
            } else {
                (new_u16_be(0), AlgId::new(0))
            };

            Some(Self {
                algorithm,
                key_bits,
                mode,
            })
        }

        /// Returns the size in bytes of the encoded structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.algorithm);

            if self.algorithm != AlgIdEnum::NULL.into() {
                payload_size += size_of_val(&self.key_bits);
                payload_size += size_of_val(&self.mode);
            }

            payload_size
        }
    }

    /// Parameters describing an RSA key (`TPMS_RSA_PARAMS`).
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout, PartialEq)]
    pub struct TpmsRsaParams {
        symmetric: TpmtSymDefObject,
        scheme: TpmtRsaScheme,
        key_bits: u16_be,
        /// Public exponent value (`0` encodes $65537$).
        pub exponent: u32_be,
    }

    impl TpmsRsaParams {
        /// Creates RSA parameters with the provided symmetric definition and scheme.
        pub fn new(
            symmetric: TpmtSymDefObject,
            scheme: TpmtRsaScheme,
            key_bits: u16,
            exponent: u32,
        ) -> Self {
            Self {
                symmetric,
                scheme,
                key_bits: key_bits.into(),
                exponent: exponent.into(),
            }
        }

        /// Serializes the parameters into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(&self.symmetric.serialize());
            buffer.extend_from_slice(&self.scheme.serialize());
            buffer.extend_from_slice(self.key_bits.as_bytes());
            buffer.extend_from_slice(self.exponent.as_bytes());

            buffer
        }

        /// Deserializes parameters from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = 0;

            let symmetric = TpmtSymDefObject::deserialize(&bytes[start..])?;
            end += symmetric.payload_size();

            start = end;
            let scheme = TpmtRsaScheme::deserialize(&bytes[start..])?;
            end += scheme.payload_size();

            // TODO: zerocopy: as of zerocopy 0.8 this can be simplified with `read_from_bytes`....ok()?, to avoid (https://github.com/microsoft/openvmm/issues/759)
            // manual size checks. Leaving this code as-is to reduce risk of the 0.7 -> 0.8 move.
            start = end;
            end += size_of::<u16_be>();
            if bytes.len() < end {
                return None;
            }
            let key_bits = u16_be::read_from_bytes(&bytes[start..end]).ok()?;

            // TODO: zerocopy: as of zerocopy 0.8 this can be simplified with `read_from_bytes`....ok()?, to avoid (https://github.com/microsoft/openvmm/issues/759)
            // manual size checks. Leaving this code as-is to reduce risk of the 0.7 -> 0.8 move.
            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let exponent = u32_be::read_from_bytes(&bytes[start..end]).ok()?;

            Some(Self {
                symmetric,
                scheme,
                key_bits,
                exponent,
            })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += self.symmetric.payload_size();
            payload_size += self.scheme.payload_size();
            payload_size += size_of_val(&self.key_bits);
            payload_size += size_of_val(&self.exponent);

            payload_size
        }
    }

    /// Public area describing a TPM object (`TPMT_PUBLIC`).
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmtPublic {
        my_type: AlgId,
        name_alg: AlgId,
        /// Attributes that define object capabilities.
        pub object_attributes: TpmaObject,
        auth_policy: Tpm2bBuffer,
        // `TPMS_RSA_PARAMS`
        /// Algorithm-specific parameters associated with the object.
        pub parameters: TpmsRsaParams,
        // `TPM2B_PUBLIC_KEY_RSA`
        /// Unique data that identifies the object.
        pub unique: Tpm2bBuffer,
    }

    impl TpmtPublic {
        /// Builds a public area from individual components.
        pub fn new(
            my_type: AlgId,
            name_alg: AlgId,
            object_attributes: TpmaObjectBits,
            auth_policy: &[u8],
            parameters: TpmsRsaParams,
            unique: &[u8],
        ) -> Result<Self, TpmProtoError> {
            let auth_policy =
                Tpm2bBuffer::new(auth_policy).map_err(TpmProtoError::TpmtPublicAuthPolicy)?;
            let unique = Tpm2bBuffer::new(unique).map_err(TpmProtoError::TpmtPublicUnique)?;
            Ok(Self {
                my_type,
                name_alg,
                object_attributes: object_attributes.into(),
                auth_policy,
                parameters,
                unique,
            })
        }

        /// Serializes the public area into TPM wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.my_type.as_bytes());
            buffer.extend_from_slice(self.name_alg.as_bytes());
            buffer.extend_from_slice(self.object_attributes.as_bytes());
            buffer.extend_from_slice(&self.auth_policy.serialize());
            buffer.extend_from_slice(&self.parameters.serialize());
            buffer.extend_from_slice(&self.unique.serialize());

            buffer
        }

        /// Parses the public area from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<AlgId>();
            if bytes.len() < end {
                return None;
            }
            let r#type = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<AlgId>();
            if bytes.len() < end {
                return None;
            }
            let name_alg = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<TpmaObject>();
            if bytes.len() < end {
                return None;
            }
            let object_attributes: u32 = u32_be::read_from_bytes(&bytes[start..end]).ok()?.into(); // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let auth_policy = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += auth_policy.payload_size();
            if bytes.len() < end {
                return None;
            }

            start = end;
            let parameters = TpmsRsaParams::deserialize(&bytes[start..])?;
            end += parameters.payload_size();

            start = end;
            let unique = Tpm2bBuffer::deserialize(&bytes[start..])?;

            Some(Self {
                my_type: r#type,
                name_alg,
                object_attributes: object_attributes.into(),
                auth_policy,
                parameters,
                unique,
            })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.my_type);
            payload_size += size_of_val(&self.name_alg);
            payload_size += size_of_val(&self.object_attributes);
            payload_size += self.auth_policy.payload_size();
            payload_size += self.parameters.payload_size();
            payload_size += self.unique.payload_size();

            payload_size
        }
    }

    /// Size-prefixed wrapper for [`TpmtPublic`] (`TPM2B_PUBLIC`).
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct Tpm2bPublic {
        /// Size of the serialized public area.
        pub size: u16_be,
        /// Embedded public area contents.
        pub public_area: TpmtPublic,
    }

    impl Tpm2bPublic {
        /// Wraps the given public area and records its serialized size.
        pub fn new(public_area: TpmtPublic) -> Self {
            let size = public_area.payload_size() as u16;
            Self {
                size: size.into(),
                public_area,
            }
        }

        /// Serializes the wrapper into TPM wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.size.as_bytes());
            buffer.extend_from_slice(&self.public_area.serialize());

            buffer
        }

        /// Parses the wrapper from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let end = size_of::<u16_be>();

            if bytes.len() < end {
                return None;
            }

            let size = u16_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let public_area = TpmtPublic::deserialize(&bytes[start..])?;

            Some(Self { size, public_area })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.size);
            payload_size += self.public_area.payload_size();

            payload_size
        }
    }

    /// `TPMS_CREATION_DATA`
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmsCreationData {
        pcr_select: TpmlPcrSelection,
        pcr_digest: Tpm2bBuffer,
        locality: u8,
        parent_name_alg: AlgId,
        parent_name: Tpm2bBuffer,
        parent_qualified_name: Tpm2bBuffer,
        outside_info: Tpm2bBuffer,
    }

    impl TpmsCreationData {
        /// Attempts to parse a creation data structure from the TPM wire representation.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = 0;

            let pcr_select = TpmlPcrSelection::deserialize(&bytes[start..])?;
            end += pcr_select.payload_size();

            start = end;
            let pcr_digest = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += pcr_digest.payload_size();

            start = end;
            end += size_of::<u8>();
            if bytes.len() < end {
                return None;
            }
            let locality = bytes[start];

            start = end;
            end += size_of::<AlgId>();
            if bytes.len() < end {
                return None;
            }
            let parent_name_alg = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let parent_name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += parent_name.payload_size();

            start = end;
            let parent_qualified_name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += parent_qualified_name.payload_size();

            start = end;
            let outside_info = Tpm2bBuffer::deserialize(&bytes[start..])?;

            Some(Self {
                pcr_select,
                pcr_digest,
                locality,
                parent_name_alg,
                parent_name,
                parent_qualified_name,
                outside_info,
            })
        }

        /// Returns the number of bytes required to encode this structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += self.pcr_select.payload_size();
            payload_size += self.pcr_digest.payload_size();
            payload_size += size_of_val(&self.locality);
            payload_size += size_of_val(&self.parent_name_alg);
            payload_size += self.parent_name.payload_size();
            payload_size += self.parent_qualified_name.payload_size();
            payload_size += self.outside_info.payload_size();

            payload_size
        }
    }

    /// `TPM2B_CREATION_DATA`
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    #[repr(C)]
    pub struct Tpm2bCreationData {
        size: u16_be,
        creation_data: TpmsCreationData,
    }

    impl Tpm2bCreationData {
        /// Parses a size-prefixed creation data blob from the TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let end = size_of::<u16_be>();

            if bytes.len() < end {
                return None;
            }

            let size = u16_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let creation_data = TpmsCreationData::deserialize(&bytes[start..])?;

            Some(Self {
                size,
                creation_data,
            })
        }

        /// Returns the number of bytes required to encode this structure.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.size);
            payload_size += self.creation_data.payload_size();

            payload_size
        }
    }

    /// `TPMT_TK_CREATION`
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmtTkCreation {
        tag: SessionTag,
        hierarchy: ReservedHandle,
        digest: Tpm2bBuffer,
    }

    impl TpmtTkCreation {
        /// Parses a creation ticket (`TPMT_TK_CREATION`) from the TPM wire encoding.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<SessionTag>();
            if bytes.len() < end {
                return None;
            }
            let tag = SessionTag::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<ReservedHandle>();
            if bytes.len() < end {
                return None;
            }
            let hierarchy = ReservedHandle::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let digest = Tpm2bBuffer::deserialize(&bytes[start..])?;

            Some(Self {
                tag,
                hierarchy,
                digest,
            })
        }

        /// Returns the number of bytes required to encode this ticket.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.tag);
            payload_size += size_of_val(&self.hierarchy);
            payload_size += self.digest.payload_size();

            payload_size
        }
    }

    /// Description of a TPM NV index (`TPMS_NV_PUBLIC`).
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct TpmsNvPublic {
        nv_index: u32_be,
        name_alg: AlgId,
        /// Attribute flags controlling NV behavior.
        pub attributes: TpmaNv,
        auth_policy: Tpm2bBuffer,
        /// Size of the stored NV data in bytes.
        pub data_size: u16_be,
    }

    impl TpmsNvPublic {
        /// Constructs an NV public description from discrete fields.
        pub fn new(
            nv_index: u32,
            name_alg: AlgId,
            attributes: TpmaNvBits,
            auth_policy: &[u8],
            data_size: u16,
        ) -> Result<Self, TpmProtoError> {
            let auth_policy =
                Tpm2bBuffer::new(auth_policy).map_err(TpmProtoError::TpmsNvPublicAuthPolicy)?;

            Ok(Self {
                nv_index: nv_index.into(),
                name_alg,
                attributes: attributes.into(),
                auth_policy,
                data_size: data_size.into(),
            })
        }

        /// Serializes the NV public structure into TPM wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.nv_index.as_bytes());
            buffer.extend_from_slice(self.name_alg.as_bytes());
            buffer.extend_from_slice(self.attributes.as_bytes());
            buffer.extend_from_slice(&self.auth_policy.serialize());
            buffer.extend_from_slice(self.data_size.as_bytes());

            buffer
        }

        /// Deserializes an NV public structure from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let nv_index: u32 = u32_be::read_from_bytes(&bytes[start..end]).ok()?.into(); // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<AlgId>();
            if bytes.len() < end {
                return None;
            }
            let name_alg = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<TpmaNv>();
            if bytes.len() < end {
                return None;
            }
            let attributes: u32 = u32_be::read_from_bytes(&bytes[start..end]).ok()?.into(); // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let auth_policy = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += auth_policy.payload_size();

            start = end;
            end += size_of::<u16_be>();
            if bytes.len() < end {
                return None;
            }
            let data_size = u16_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            Some(Self {
                nv_index: nv_index.into(),
                name_alg,
                attributes: attributes.into(),
                auth_policy,
                data_size,
            })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.nv_index);
            payload_size += size_of_val(&self.name_alg);
            payload_size += size_of_val(&self.attributes);
            payload_size += self.auth_policy.payload_size();
            payload_size += size_of_val(&self.data_size);

            payload_size
        }
    }

    /// Size-prefixed wrapper for [`TpmsNvPublic`] (`TPM2B_NV_PUBLIC`).
    #[repr(C)]
    #[derive(Debug, Copy, Clone, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct Tpm2bNvPublic {
        size: u16_be,
        /// Wrapped NV index description.
        pub nv_public: TpmsNvPublic,
    }

    impl Tpm2bNvPublic {
        /// Wraps the given NV public information and records its serialized size.
        pub fn new(nv_public: TpmsNvPublic) -> Result<Self, InvalidInput> {
            let size = nv_public.payload_size();
            if size > u16::MAX.into() {
                Err(InvalidInput::NvPublicPayloadTooLarge(size, u16::MAX.into()))?
            }

            Ok(Self {
                size: (size as u16).into(),
                nv_public,
            })
        }

        /// Serializes the wrapper into TPM wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.size.as_bytes());
            buffer.extend_from_slice(&self.nv_public.serialize());

            buffer
        }

        /// Parses the wrapper from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let end = size_of::<u16_be>();

            if bytes.len() < end {
                return None;
            }

            let size = u16_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let nv_public = TpmsNvPublic::deserialize(&bytes[start..])?;

            Some(Self { size, nv_public })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.size);
            payload_size += self.nv_public.payload_size();

            payload_size
        }
    }

    // === ClearControl === //

    /// Command body for `TPM2_ClearControl`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ClearControlCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        auth_size: u32_be,
        auth: common::CmdAuth,
        disable: u8,
    }

    impl ClearControlCmd {
        /// Creates a `ClearControl` command that toggles the disable flag.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            disable: bool,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::ClearControl.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                disable: disable as u8,
            }
        }
    }

    /// Reply payload for `TPM2_ClearControl`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ClearControlReply {
        /// Common reply header preceding parameter data.
        pub header: ReplyHeader,
        /// Size of the authorization area that follows.
        pub param_size: u32_be,
        /// Authorization block returned by the TPM.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for ClearControlCmd {
        type Reply = ClearControlReply;
    }

    impl TpmReply for ClearControlReply {
        type Command = ClearControlCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Clear === //

    /// Command body for `TPM2_Clear`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ClearCmd {
        header: CmdHeader,

        auth_handle: ReservedHandle,
        auth_size: u32_be,
        auth: common::CmdAuth,
    }

    impl ClearCmd {
        /// Creates a `Clear` command targeting the provided hierarchy handle.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::Clear.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
            }
        }
    }

    /// Reply payload for `TPM2_Clear`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ClearReply {
        /// Common reply header preceding parameter data.
        pub header: ReplyHeader,
        /// Size of the authorization area that follows.
        pub param_size: u32_be,
        /// Authorization block returned by the TPM.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for ClearCmd {
        type Reply = ClearReply;
    }

    impl TpmReply for ClearReply {
        type Command = ClearCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Startup === //

    /// Identifies the type of TPM startup being requested.
    pub enum StartupType {
        /// Initialize state as though coming from a powered-off state.
        Clear,
        /// Resume from previously saved state.
        State,
    }

    /// Command body for `TPM2_Startup`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct StartupCmd {
        header: CmdHeader,
        startup_type: u16_be,
    }

    impl StartupCmd {
        /// Creates a TPM startup command with the given startup type.
        pub fn new(session_tag: SessionTag, startup_type: StartupType) -> StartupCmd {
            StartupCmd {
                header: CmdHeader::new::<Self>(session_tag, CommandCodeEnum::Startup.into()),
                startup_type: match startup_type {
                    StartupType::Clear => 0,
                    StartupType::State => 1,
                }
                .into(),
            }
        }
    }

    /// Reply payload for `TPM2_Startup`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct StartupReply {
        /// Common reply header preceding parameter data.
        pub header: ReplyHeader,
    }

    impl TpmCommand for StartupCmd {
        type Reply = StartupReply;
    }

    impl TpmReply for StartupReply {
        type Command = StartupCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Self Test === //

    /// Command body for `TPM2_SelfTest`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SelfTestCmd {
        header: CmdHeader,
        full_test: u8,
    }

    impl SelfTestCmd {
        /// Creates a self-test command optionally requesting a full diagnostic.
        pub fn new(session_tag: SessionTag, full_test: bool) -> SelfTestCmd {
            SelfTestCmd {
                header: CmdHeader::new::<Self>(session_tag, CommandCodeEnum::SelfTest.into()),
                full_test: full_test as u8,
            }
        }
    }

    /// Reply payload for `TPM2_SelfTest`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct SelfTestReply {
        /// Common reply header preceding parameter data.
        pub header: ReplyHeader,
    }

    impl TpmCommand for SelfTestCmd {
        type Reply = SelfTestReply;
    }

    impl TpmReply for SelfTestReply {
        type Command = SelfTestCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Pcr Allocate === //

    /// Represents a selection of PCRs for allocation.
    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PcrSelection {
        /// Hash algorithm that identifies the PCR bank.
        pub hash: AlgId,
        /// Number of significant bytes in the selection bitmap.
        pub size_of_select: u8,
        /// Bitmap describing the selected PCR indices.
        pub bitmap: [u8; 3],
    }

    impl PcrSelection {
        /// Serializes the selection into TPM wire format.
        pub fn serialize(self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.hash.as_bytes());
            buffer.extend_from_slice(self.size_of_select.as_bytes());
            buffer.extend_from_slice(&self.bitmap[..self.size_of_select as usize]);

            buffer
        }

        /// Parses a selection from TPM wire format.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<AlgId>();
            if bytes.len() < end {
                return None;
            }
            let hash = AlgId::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u8>();
            if bytes.len() < end {
                return None;
            }
            let size_of_select = bytes[start];
            if size_of_select > 3 {
                return None;
            }

            start = end;
            end += size_of_select as usize;
            if bytes.len() < end {
                return None;
            }
            let mut bitmap = [0u8; 3];
            bitmap[..size_of_select as usize].copy_from_slice(&bytes[start..end]);

            Some(Self {
                hash,
                size_of_select,
                bitmap,
            })
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.hash);
            payload_size += size_of_val(&self.size_of_select);
            payload_size += self.size_of_select as usize;

            payload_size
        }
    }

    /// Command payload for `TPM2_PCR_Allocate`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PcrAllocateCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        // Parameters
        pcr_allocation: TpmlPcrSelection,
    }

    impl PcrAllocateCmd {
        /// Maps bit positions in the PCR allocation request to the corresponding hashing algorithm identifiers.
        pub const HASH_ALG_TO_ID: [(u32, AlgId); 5] = [
            (1 << 0, AlgId::new(AlgIdEnum::SHA as u16)),
            (1 << 1, AlgId::new(AlgIdEnum::SHA256 as u16)),
            (1 << 2, AlgId::new(AlgIdEnum::SHA384 as u16)),
            (1 << 3, AlgId::new(AlgIdEnum::SHA512 as u16)),
            (1 << 4, AlgId::new(AlgIdEnum::SM3_256 as u16)),
        ];

        /// Builds a new PCR allocation command targeting the supplied hierarchy and PCR selections.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            pcr_selections: &[PcrSelection],
        ) -> Result<Self, TpmProtoError> {
            let pcr_allocation = TpmlPcrSelection::new(pcr_selections)
                .map_err(TpmProtoError::PcrAllocatePcrAllocation)?;

            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::PCR_Allocate.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                pcr_allocation,
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            Ok(cmd)
        }

        /// Serializes the command into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.auth_handle.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth.as_bytes());
            buffer.extend_from_slice(&self.pcr_allocation.serialize());

            buffer
        }

        /// Returns the total number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.auth_handle);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth);
            payload_size += self.pcr_allocation.payload_size();

            payload_size
        }
    }

    /// Reply payload returned from `TPM2_PCR_Allocate`.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PcrAllocateReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Size in bytes of the authorization area.
        pub auth_size: u32_be,
        /// Indicates whether the TPM accepted the allocation request.
        pub allocation_success: u8,
        /// Maximum number of PCR slots supported after allocation.
        pub max_pcr: u32_be,
        /// Additional space required to satisfy the allocation.
        pub size_needed: u32_be,
        /// Remaining space available after allocation.
        pub size_available: u32_be,

        /// Authorization data returned alongside the reply.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for PcrAllocateCmd {
        type Reply = PcrAllocateReply;
    }

    impl TpmReply for PcrAllocateReply {
        type Command = PcrAllocateCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === ChangeSeed === //

    /// Command payload shared by `TPM2_ChangeEPS` and `TPM2_ChangePPS`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ChangeSeedCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        auth_size: u32_be,
        auth: common::CmdAuth,
    }

    impl ChangeSeedCmd {
        /// Creates a change-seed command for the target hierarchy.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            command_code: CommandCodeEnum,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, command_code.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
            }
        }
    }

    /// Reply payload for change-seed commands.
    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ChangeSeedReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Size in bytes of the parameter area that follows the header.
        pub param_size: u32_be,

        /// Authorization data returned alongside the reply.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for ChangeSeedCmd {
        type Reply = ChangeSeedReply;
    }

    impl TpmReply for ChangeSeedReply {
        type Command = ChangeSeedCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: option-to-error (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === CreatePrimary === //

    /// Command payload for `TPM2_CreatePrimary`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct CreatePrimaryCmd {
        /// Command metadata and transport header.
        pub header: CmdHeader,
        primary_handle: ReservedHandle,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        // Parameters
        in_sensitive: Tpm2bSensitiveCreate,
        in_public: Tpm2bPublic,
        outside_info: Tpm2bBuffer,
        creation_pcr: TpmlPcrSelection,
    }

    impl CreatePrimaryCmd {
        /// Builds a primary key creation command with the provided template and authorization.
        pub fn new(
            session: SessionTag,
            primary_handle: ReservedHandle,
            auth: common::CmdAuth,
            in_sensitive_user_auth: &[u8],
            in_sensitive_data: &[u8],
            in_public: TpmtPublic,
            outside_info: &[u8],
            creation_pcr: &[PcrSelection],
        ) -> Result<Self, TpmProtoError> {
            let sensitive_create =
                TpmsSensitiveCreate::new(in_sensitive_user_auth, in_sensitive_data)?;
            let in_sensitive = Tpm2bSensitiveCreate::new(sensitive_create);
            let in_public = Tpm2bPublic::new(in_public);
            let outside_info =
                Tpm2bBuffer::new(outside_info).map_err(TpmProtoError::CreatePrimaryOutsideInfo)?;
            let creation_pcr = TpmlPcrSelection::new(creation_pcr)
                .map_err(TpmProtoError::CreatePrimaryCreationPcr)?;

            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::CreatePrimary.into()),
                primary_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                in_sensitive,
                in_public,
                outside_info,
                creation_pcr,
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            Ok(cmd)
        }

        /// Serializes the command payload into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.primary_handle.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth.as_bytes());
            buffer.extend_from_slice(&self.in_sensitive.serialize());
            buffer.extend_from_slice(&self.in_public.serialize());
            buffer.extend_from_slice(&self.outside_info.serialize());
            buffer.extend_from_slice(&self.creation_pcr.serialize());

            buffer
        }

        /// Returns the number of bytes that [`Self::serialize`] will emit.
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.primary_handle);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth);
            payload_size += self.in_sensitive.payload_size();
            payload_size += self.in_public.payload_size();
            payload_size += self.outside_info.payload_size();
            payload_size += self.creation_pcr.payload_size();

            payload_size
        }
    }

    /// Reply payload returned from `TPM2_CreatePrimary`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct CreatePrimaryReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Handle assigned to the created primary object.
        pub object_handle: ReservedHandle,
        // Parameter size
        param_size: u32_be,
        // Parameters
        /// Public portion of the created primary object.
        pub out_public: Tpm2bPublic,
        creation_data: Tpm2bCreationData,
        creation_hash: Tpm2bBuffer,
        creation_ticket: TpmtTkCreation,
        name: Tpm2bBuffer,
        // Authorization area
        auth: common::ReplyAuth,
    }

    impl TpmCommand for CreatePrimaryCmd {
        type Reply = CreatePrimaryReply;
    }

    impl TpmReply for CreatePrimaryReply {
        type Command = CreatePrimaryCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();
            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                let mut cmd = CreatePrimaryReply::new_zeroed();
                cmd.header = header;
                return Some(cmd);
            }

            start = end;
            end += size_of::<ReservedHandle>();
            let object_handle = ReservedHandle::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u32_be>();
            let param_size = u32_be::read_from_bytes(&bytes[start..end]).ok()?; // TODO: zerocopy: simplify (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let out_public = Tpm2bPublic::deserialize(&bytes[start..])?;
            end += out_public.payload_size();

            start = end;
            let creation_data = Tpm2bCreationData::deserialize(&bytes[start..])?;
            end += creation_data.payload_size();

            start = end;
            let creation_hash = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += creation_hash.payload_size();

            start = end;
            let creation_ticket = TpmtTkCreation::deserialize(&bytes[start..])?;
            end += creation_ticket.payload_size();

            start = end;
            let name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += name.payload_size();

            start = end;
            end += size_of::<common::ReplyAuth>();
            let auth = common::ReplyAuth::read_from_prefix(&bytes[start..end])
                .ok()?
                .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                object_handle,
                param_size,
                out_public,
                creation_data,
                creation_hash,
                creation_ticket,
                name,
                auth,
            })
        }

        fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.object_handle);
            payload_size += size_of_val(&self.param_size);
            payload_size += self.out_public.payload_size();
            payload_size += self.creation_data.payload_size();
            payload_size += self.creation_hash.payload_size();
            payload_size += self.creation_ticket.payload_size();
            payload_size += self.name.payload_size();
            payload_size += size_of_val(&self.auth);

            payload_size
        }
    }

    // === FlushContext === //

    /// Command payload for `TPM2_FlushContext`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct FlushContextCmd {
        /// Command metadata and transport header.
        pub header: CmdHeader,
        // Parameter
        flush_handle: ReservedHandle,
    }

    impl FlushContextCmd {
        /// Creates a flush context command targeting the specified handle.
        pub fn new(flush_handle: ReservedHandle) -> Self {
            Self {
                header: CmdHeader::new::<Self>(
                    SessionTagEnum::NoSessions.into(),
                    CommandCodeEnum::FlushContext.into(),
                ),
                flush_handle,
            }
        }
    }

    /// Reply payload returned from `TPM2_FlushContext`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct FlushContextReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
    }

    impl TpmCommand for FlushContextCmd {
        type Reply = FlushContextReply;
    }

    impl TpmReply for FlushContextReply {
        type Command = FlushContextCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === EvictControl === //

    /// Command payload for `TPM2_EvictControl`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct EvictControlCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        object_handle: ReservedHandle,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        // Parameter
        persistent_handle: ReservedHandle,
    }

    impl EvictControlCmd {
        /// Creates an evict control command that persists or evicts the specified object handle.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            object_handle: ReservedHandle,
            auth: common::CmdAuth,
            persistent_handle: ReservedHandle,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::EvictControl.into()),
                auth_handle,
                object_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                persistent_handle,
            }
        }
    }

    /// Reply payload for `TPM2_EvictControl`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct EvictControlReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
    }

    impl TpmCommand for EvictControlCmd {
        type Reply = EvictControlReply;
    }

    impl TpmReply for EvictControlReply {
        type Command = EvictControlCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: error-to-option (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === ReadPublic === //

    /// Command payload for `TPM2_ReadPublic`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ReadPublicCmd {
        header: CmdHeader,
        object_handle: ReservedHandle,
    }

    impl ReadPublicCmd {
        /// Builds a read-public command for the given object handle.
        pub fn new(session: SessionTag, object_handle: ReservedHandle) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::ReadPublic.into()),
                object_handle,
            }
        }
    }

    /// Reply payload returned from `TPM2_ReadPublic`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct ReadPublicReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Public portion of the requested object.
        pub out_public: Tpm2bPublic,
        name: Tpm2bBuffer,
        qualified_name: Tpm2bBuffer,
    }

    impl TpmCommand for ReadPublicCmd {
        type Reply = ReadPublicReply;
    }

    impl TpmReply for ReadPublicReply {
        type Command = ReadPublicCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();

            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                return Some(Self {
                    header,
                    out_public: Tpm2bPublic::new_zeroed(),
                    name: Tpm2bBuffer::new_zeroed(),
                    qualified_name: Tpm2bBuffer::new_zeroed(),
                });
            }

            start = end;
            let out_public = Tpm2bPublic::deserialize(&bytes[start..])?;
            end += out_public.payload_size();

            start = end;
            let name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += name.payload_size();

            start = end;
            let qualified_name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += qualified_name.payload_size();

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                out_public,
                name,
                qualified_name,
            })
        }

        fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of::<ReplyHeader>();
            payload_size += self.out_public.payload_size();
            payload_size += self.name.payload_size();
            payload_size += self.qualified_name.payload_size();

            payload_size
        }
    }

    // === Nv DefineSpace === //

    /// Command payload for `TPM2_NV_DefineSpace`.
    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvDefineSpaceCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        // Authorization area
        auth_size: u32_be,
        auth_cmd: common::CmdAuth,
        // Parameters
        auth: Tpm2bBuffer,
        public_info: Tpm2bNvPublic,
    }

    impl NvDefineSpaceCmd {
        /// Creates an NV index definition command with the provided attributes and authorization data.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth_cmd: common::CmdAuth,
            auth: u64,
            public_info: TpmsNvPublic,
        ) -> Result<Self, TpmProtoError> {
            let auth = new_u64_be(auth);
            let auth =
                Tpm2bBuffer::new(auth.as_bytes()).map_err(TpmProtoError::NvDefineSpaceAuth)?;
            let public_info =
                Tpm2bNvPublic::new(public_info).map_err(TpmProtoError::NvDefineSpacePublicInfo)?;

            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::NV_DefineSpace.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth_cmd,
                auth,
                public_info,
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            Ok(cmd)
        }

        /// Serializes the command into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.auth_handle.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth_cmd.as_bytes());
            buffer.extend_from_slice(&self.auth.serialize());
            buffer.extend_from_slice(&self.public_info.serialize());

            buffer
        }

        /// Returns the number of bytes produced by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.auth_handle);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth_cmd);
            payload_size += self.auth.payload_size();
            payload_size += self.public_info.payload_size();

            payload_size
        }
    }

    /// Reply payload for `TPM2_NV_DefineSpace`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvDefineSpaceReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
    }

    impl TpmCommand for NvDefineSpaceCmd {
        type Reply = NvDefineSpaceReply;
    }

    impl TpmReply for NvDefineSpaceReply {
        type Command = NvDefineSpaceCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Nv UndefineSpace === //

    /// Command payload for `TPM2_NV_UndefineSpace`.
    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvUndefineSpaceCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        nv_index: u32_be,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
    }

    impl NvUndefineSpaceCmd {
        /// Creates an NV undefine command targeting the specified index.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            nv_index: u32,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::NV_UndefineSpace.into()),
                auth_handle,
                nv_index: nv_index.into(),
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
            }
        }
    }

    /// Reply payload for `TPM2_NV_UndefineSpace`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvUndefineSpaceReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
    }

    impl TpmCommand for NvUndefineSpaceCmd {
        type Reply = NvUndefineSpaceReply;
    }

    impl TpmReply for NvUndefineSpaceReply {
        type Command = NvUndefineSpaceCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Nv ReadPublic === //

    /// Command payload for `TPM2_NV_ReadPublic`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct NvReadPublicCmd {
        header: CmdHeader,
        nv_index: u32_be,
    }

    impl NvReadPublicCmd {
        /// Builds an NV read-public command targeting the requested index.
        pub fn new(session: SessionTag, nv_index: u32) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::NV_ReadPublic.into()),
                nv_index: nv_index.into(),
            }
        }
    }

    /// Reply payload for `TPM2_NV_ReadPublic`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvReadPublicReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        // Parameters
        /// Public information describing the NV index.
        pub nv_public: Tpm2bNvPublic,
        nv_name: Tpm2bBuffer,
    }

    impl TpmCommand for NvReadPublicCmd {
        type Reply = NvReadPublicReply;
    }

    impl TpmReply for NvReadPublicReply {
        type Command = NvReadPublicCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();

            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                return Some(Self {
                    header,
                    nv_public: Tpm2bNvPublic::new_zeroed(),
                    nv_name: Tpm2bBuffer::new_zeroed(),
                });
            }

            start = end;
            let nv_public = Tpm2bNvPublic::deserialize(&bytes[start..])?;
            end += nv_public.payload_size();

            start = end;
            let nv_name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += nv_name.payload_size();

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                nv_public,
                nv_name,
            })
        }

        fn payload_size(&self) -> usize {
            let mut size = 0;

            size += size_of::<ReplyHeader>();
            size += self.nv_public.payload_size();
            size += self.nv_name.payload_size();

            size
        }
    }

    // === Nv Write === //

    /// Command payload for `TPM2_NV_Write`.
    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvWriteCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        /// NV index being written.
        pub nv_index: u32_be,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        auth_value: u64_be,
        // Parameters
        /// Buffer containing the data to write.
        pub data: Tpm2bBuffer,
        /// Offset within the NV index at which the write begins.
        pub offset: u16_be,
    }

    impl NvWriteCmd {
        /// Constructs an NV write command with the provided payload and authorization information.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            auth_value: u64,
            nv_index: u32,
            data: &[u8],
            offset: u16,
        ) -> Result<Self, TpmProtoError> {
            let data = Tpm2bBuffer::new(data).map_err(TpmProtoError::NvWriteData)?;
            // If `auth_handle` is not the owner, assuming password-based authorization is used.
            let auth_value_size = if auth_handle != TPM20_RH_OWNER {
                size_of::<u64_be>() as u32
            } else {
                0
            };

            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::NV_Write.into()),
                auth_handle,
                nv_index: nv_index.into(),
                auth_size: (size_of::<common::CmdAuth>() as u32 + auth_value_size).into(),
                auth,
                auth_value: auth_value.into(),
                data,
                offset: offset.into(),
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            Ok(cmd)
        }

        /// Updates the data payload and offset while recomputing the payload size.
        pub fn update_write_data(&mut self, data: &[u8], offset: u16) -> Result<(), TpmProtoError> {
            let data = Tpm2bBuffer::new(data).map_err(TpmProtoError::NvWriteData)?;

            self.data = data;
            self.offset = offset.into();
            self.header.size = new_u32_be(self.payload_size() as u32);

            Ok(())
        }

        /// Serializes the command into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.auth_handle.as_bytes());
            buffer.extend_from_slice(self.nv_index.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth.as_bytes());
            if self.auth_handle != TPM20_RH_OWNER {
                buffer.extend_from_slice(self.auth_value.as_bytes());
            }
            buffer.extend_from_slice(&self.data.serialize());
            buffer.extend_from_slice(self.offset.as_bytes());

            buffer
        }

        /// Returns the number of bytes emitted by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.auth_handle);
            payload_size += size_of_val(&self.nv_index);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth);
            if self.auth_handle != TPM20_RH_OWNER {
                payload_size += size_of_val(&self.auth_value);
            }
            payload_size += self.data.payload_size();
            payload_size += size_of_val(&self.offset);

            payload_size
        }
    }

    /// Reply payload for `TPM2_NV_Write`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvWriteReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
    }

    impl TpmCommand for NvWriteCmd {
        type Reply = NvWriteReply;
    }

    impl TpmReply for NvWriteReply {
        type Command = NvWriteCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            Some(Self::read_from_prefix(bytes).ok()?.0) // TODO: zerocopy: tpm better error? (https://github.com/microsoft/openvmm/issues/759)
        }

        fn payload_size(&self) -> usize {
            size_of::<Self>()
        }
    }

    // === Nv Read === //

    /// Command payload for `TPM2_NV_Read`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct NvReadCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        /// NV index being read.
        pub nv_index: u32_be,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        // Parameters
        size: u16_be,
        /// Offset within the NV index that the read should start from.
        pub offset: u16_be,
    }

    impl NvReadCmd {
        /// Constructs an NV read command for the specified index and range.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            nv_index: u32,
            auth: common::CmdAuth,
            size: u16,
            offset: u16,
        ) -> Self {
            Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::NV_Read.into()),
                auth_handle,
                nv_index: nv_index.into(),
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                size: size.into(),
                offset: offset.into(),
            }
        }

        /// Updates the read size and offset used by the command payload.
        pub fn update_read_parameters(&mut self, size: u16, offset: u16) {
            self.size = size.into();
            self.offset = offset.into();
        }

        /// Attempts to parse an NV read command from raw TPM command bytes.
        pub fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<CmdHeader>();
            if bytes.len() < end {
                return None;
            }
            let header = CmdHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            if header.command_code != CommandCodeEnum::NV_Read.into() {
                return None;
            }

            start = end;
            end += size_of::<ReservedHandle>();
            if bytes.len() < end {
                return None;
            }
            let auth_handle = ReservedHandle::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let nv_index = u32_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let auth_size = u32_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Skip authorization area
            end += auth_size.get() as usize;

            start = end;
            end += size_of::<u16_be>();
            if bytes.len() < end {
                return None;
            }
            let size = u16_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u16_be>();
            if bytes.len() < end {
                return None;
            }
            let offset = u16_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            Some(Self {
                header,
                auth_handle,
                nv_index,
                auth_size,
                auth: common::CmdAuth::new(ReservedHandle(0.into()), 0, 0, 0),
                size,
                offset,
            })
        }
    }

    /// Reply payload for `TPM2_NV_Read`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct NvReadReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Size in bytes of the parameter area contained in the reply.
        pub parameter_size: u32_be,
        // Parameter
        /// Buffer containing the data read from NV storage.
        pub data: Tpm2bBuffer,
        // Authorization area
        /// Authorization data returned alongside the reply.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for NvReadCmd {
        type Reply = NvReadReply;
    }

    impl TpmReply for NvReadReply {
        type Command = NvReadCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();

            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                return Some(Self {
                    header,
                    parameter_size: 0.into(),
                    data: Tpm2bBuffer::new_zeroed(),
                    auth: common::ReplyAuth::new_zeroed(),
                });
            }

            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let parameter_size = u32_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            let data = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += data.payload_size();

            start = end;
            end += size_of::<common::ReplyAuth>();
            if bytes.len() < end {
                return None;
            }
            let auth = common::ReplyAuth::read_from_prefix(&bytes[start..end])
                .ok()?
                .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                parameter_size,
                data,
                auth,
            })
        }

        fn payload_size(&self) -> usize {
            let mut size = 0;

            size += size_of::<ReplyHeader>();
            size += self.data.payload_size();

            size
        }
    }

    // === Import === //

    /// Command payload for `TPM2_Import`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct ImportCmd {
        /// Command metadata and transport header.
        pub header: CmdHeader,
        /// Handle that authorizes the import operation.
        pub auth_handle: ReservedHandle,
        // Authorization area
        /// Size of the authorization area.
        pub auth_size: u32_be,
        /// Authorization area contents.
        pub auth: common::CmdAuth,
        // Parameters
        // `TPM2B_DATA`
        /// Optional symmetric key used to wrap the duplicate blob.
        pub encryption_key: Tpm2bBuffer,
        // `TPM2B_PUBLIC`
        /// Public area of the object being imported.
        pub object_public: Tpm2bPublic,
        // `TPM2B_PRIVATE`
        /// Encrypted private area of the object.
        pub duplicate: Tpm2bBuffer,
        // `TPM2B_ENCRYPTED_SECRET`
        /// Encrypted seed for symmetric protection.
        pub in_sym_seed: Tpm2bBuffer,
        // `TPMT_SYM_DEF_OBJECT`
        /// Symmetric algorithm definition used during import.
        pub symmetric_alg: TpmtSymDefObject,
    }

    impl ImportCmd {
        /// Builds an import command using the provided buffers and authorization context.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            encryption_key: &Tpm2bBuffer,
            object_public: &Tpm2bPublic,
            duplicate: &Tpm2bBuffer,
            in_sym_seed: &Tpm2bBuffer,
            symmetric_alg: &TpmtSymDefObject,
        ) -> Self {
            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::Import.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                encryption_key: *encryption_key,
                object_public: *object_public,
                duplicate: *duplicate,
                in_sym_seed: *in_sym_seed,
                symmetric_alg: *symmetric_alg,
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            cmd
        }

        /// Deserializes the command parameters assuming no inner wrapping key (`TPM_ALG_NULL`).
        pub fn deserialize_no_wrapping_key(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = 0;

            // When there is no inner wrapper for `duplicate`, `encryption_key`
            // should be an empty buffer and `symmetric_alg` should be `TPM_ALG_NULL`.
            // See Table 42, Section 13.3.2, "Trusted Platform Module Library Part 3: Commands", revision 1.38.
            let encryption_key = Tpm2bBuffer::new_zeroed();
            let symmetric_alg = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);

            let object_public = Tpm2bPublic::deserialize(&bytes[start..])?;
            end += object_public.payload_size();

            start = end;
            let duplicate = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += duplicate.payload_size();

            start = end;
            let in_sym_seed = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += in_sym_seed.payload_size();

            // Handle zero paddings applied to valid payload
            if bytes.len() < end {
                return None;
            }

            Some(Self {
                header: CmdHeader::new_zeroed(),
                auth_handle: ReservedHandle(0.into()),
                auth_size: 0.into(),
                auth: common::CmdAuth::new_zeroed(),
                encryption_key,
                object_public,
                duplicate,
                in_sym_seed,
                symmetric_alg,
            })
        }

        /// Serializes the command into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.auth_handle.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth.as_bytes());
            buffer.extend_from_slice(&self.encryption_key.serialize());
            buffer.extend_from_slice(&self.object_public.serialize());
            buffer.extend_from_slice(&self.duplicate.serialize());
            buffer.extend_from_slice(&self.in_sym_seed.serialize());
            buffer.extend_from_slice(&self.symmetric_alg.serialize());

            buffer
        }

        /// Returns the number of bytes produced by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.auth_handle);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth);
            payload_size += self.encryption_key.payload_size();
            payload_size += self.object_public.payload_size();
            payload_size += self.duplicate.payload_size();
            payload_size += self.in_sym_seed.payload_size();
            payload_size += self.symmetric_alg.payload_size();

            payload_size
        }
    }

    /// Reply payload for `TPM2_Import`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct ImportReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Size in bytes of the parameter area contained in the reply.
        pub parameter_size: u32_be,
        // Parameter
        // `TPM2B_PRIVATE`
        /// Private portion of the imported object encrypted for the TPM.
        pub out_private: Tpm2bBuffer,
        // Authorization area
        /// Authorization data returned alongside the reply.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for ImportCmd {
        type Reply = ImportReply;
    }

    impl TpmReply for ImportReply {
        type Command = ImportCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();

            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                return Some(Self {
                    header,
                    parameter_size: 0.into(),
                    out_private: Tpm2bBuffer::new_zeroed(),
                    auth: common::ReplyAuth::new_zeroed(),
                });
            }

            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let parameter_size = u32_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
            let expected_auth_start = end + parameter_size.get() as usize;

            start = end;
            let out_private = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += out_private.payload_size();

            start = end;
            if start != expected_auth_start {
                return None;
            }
            end += size_of::<common::ReplyAuth>();
            if bytes.len() < end {
                return None;
            }
            let auth = common::ReplyAuth::read_from_prefix(&bytes[start..end])
                .ok()?
                .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                parameter_size,
                out_private,
                auth,
            })
        }

        fn payload_size(&self) -> usize {
            let mut size = 0;

            size += size_of::<ReplyHeader>();
            size += self.out_private.payload_size();

            size
        }
    }

    // === Load === //

    /// Command payload for `TPM2_Load`.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct LoadCmd {
        header: CmdHeader,
        auth_handle: ReservedHandle,
        // Authorization area
        auth_size: u32_be,
        auth: common::CmdAuth,
        // Parameters
        // `TPM2B_PRIVATE`
        in_private: Tpm2bBuffer,
        // `TPM2B_PUBLIC`
        in_public: Tpm2bPublic,
    }

    impl LoadCmd {
        /// Creates a load command for importing an object into the provided hierarchy.
        pub fn new(
            session: SessionTag,
            auth_handle: ReservedHandle,
            auth: common::CmdAuth,
            in_private: &Tpm2bBuffer,
            in_public: &Tpm2bPublic,
        ) -> Self {
            let mut cmd = Self {
                header: CmdHeader::new::<Self>(session, CommandCodeEnum::Load.into()),
                auth_handle,
                auth_size: (size_of::<common::CmdAuth>() as u32).into(),
                auth,
                in_private: *in_private,
                in_public: *in_public,
            };

            cmd.header.size = new_u32_be(cmd.payload_size() as u32);

            cmd
        }

        /// Serializes the command into TPM wire format.
        pub fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.header.as_bytes());
            buffer.extend_from_slice(self.auth_handle.as_bytes());
            buffer.extend_from_slice(self.auth_size.as_bytes());
            buffer.extend_from_slice(self.auth.as_bytes());
            buffer.extend_from_slice(&self.in_private.serialize());
            buffer.extend_from_slice(&self.in_public.serialize());

            buffer
        }

        /// Returns the number of bytes produced by [`Self::serialize`].
        pub fn payload_size(&self) -> usize {
            let mut payload_size = 0;

            payload_size += size_of_val(&self.header);
            payload_size += size_of_val(&self.auth_handle);
            payload_size += size_of_val(&self.auth_size);
            payload_size += size_of_val(&self.auth);
            payload_size += self.in_private.payload_size();
            payload_size += self.in_public.payload_size();

            payload_size
        }
    }

    /// Reply payload for `TPM2_Load`.
    #[repr(C)]
    #[derive(Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
    pub struct LoadReply {
        /// Standard TPM reply header and status.
        pub header: ReplyHeader,
        /// Object handle assigned by the TPM once the object is loaded.
        pub object_handle: ReservedHandle,
        /// Size in bytes of the parameter area contained in the reply.
        pub parameter_size: u32_be,
        // Parameter
        // `TPM2B_NAME`
        /// Qualified name of the loaded object.
        pub name: Tpm2bBuffer,
        // Authorization area
        /// Authorization data returned alongside the reply.
        pub auth: common::ReplyAuth,
    }

    impl TpmCommand for LoadCmd {
        type Reply = LoadReply;
    }

    impl TpmReply for LoadReply {
        type Command = LoadCmd;

        fn deserialize(bytes: &[u8]) -> Option<Self> {
            let mut start = 0;
            let mut end = size_of::<ReplyHeader>();

            let header = ReplyHeader::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            // Handle the command failure.
            if header.size.get() as usize == end {
                return Some(Self {
                    header,
                    object_handle: ReservedHandle::new_zeroed(),
                    parameter_size: 0.into(),
                    name: Tpm2bBuffer::new_zeroed(),
                    auth: common::ReplyAuth::new_zeroed(),
                });
            }

            start = end;
            end += size_of::<ReservedHandle>();
            if bytes.len() < end {
                return None;
            }
            let object_handle = ReservedHandle::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            start = end;
            end += size_of::<u32_be>();
            if bytes.len() < end {
                return None;
            }
            let parameter_size = u32_be::read_from_prefix(&bytes[start..end]).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
            let expected_auth_start = end + parameter_size.get() as usize;

            start = end;
            let name = Tpm2bBuffer::deserialize(&bytes[start..])?;
            end += name.payload_size();

            start = end;
            if start != expected_auth_start {
                return None;
            }
            end += size_of::<common::ReplyAuth>();
            if bytes.len() < end {
                return None;
            }
            let auth = common::ReplyAuth::read_from_prefix(&bytes[start..end])
                .ok()?
                .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

            if header.size.get() as usize != end {
                return None;
            }

            Some(Self {
                header,
                object_handle,
                parameter_size,
                name,
                auth,
            })
        }

        fn payload_size(&self) -> usize {
            let mut size = 0;

            size += size_of::<ReplyHeader>();
            size += size_of::<ReservedHandle>();
            size += self.name.payload_size();

            size
        }
    }
}

#[cfg(test)]
mod tests {
    use super::protocol::common::*;
    use super::protocol::*;
    use super::*;
    use crate::platform_akcert_attributes;

    #[test]
    fn test_create_primary() {
        const AK_PUB_EXPECTED_CMD: [u8; 321] = [
            0x80, 0x02, 0x00, 0x00, 0x01, 0x41, 0x00, 0x00, 0x01, 0x31, 0x40, 0x00, 0x00, 0x0b,
            0x00, 0x00, 0x00, 0x09, 0x40, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x18, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x05, 0x04,
            0x72, 0x00, 0x00, 0x00, 0x10, 0x00, 0x14, 0x00, 0x0b, 0x08, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        const AK_PUB_REPLY_SUCCEED: [u8; 488] = [
            0x80, 0x02, 0x00, 0x00, 0x01, 0xe8, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0xd1, 0x01, 0x18, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x05, 0x04, 0x72,
            0x00, 0x00, 0x00, 0x10, 0x00, 0x14, 0x00, 0x0b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0xc8, 0x38, 0xd1, 0x52, 0x00, 0x00, 0xe9, 0x3c, 0x89, 0x4c, 0x52, 0xfb,
            0x79, 0x7b, 0xc4, 0x14, 0x28, 0x5f, 0xaa, 0x50, 0x78, 0x9a, 0x31, 0x2b, 0x4d, 0xfe,
            0xad, 0xad, 0x97, 0x28, 0x49, 0xb2, 0x39, 0x77, 0x5e, 0x06, 0x49, 0xb7, 0x93, 0xf5,
            0x2f, 0x84, 0x85, 0x2e, 0x17, 0x87, 0x52, 0x96, 0x36, 0x74, 0x76, 0x21, 0x5f, 0xc2,
            0x90, 0x81, 0xf7, 0xe9, 0xd8, 0xac, 0x07, 0x60, 0xaf, 0x83, 0xa2, 0x08, 0xda, 0x94,
            0x77, 0x2c, 0x73, 0x9c, 0xd4, 0x80, 0x47, 0x43, 0xa6, 0x4e, 0x36, 0xc3, 0x7e, 0xe2,
            0x9c, 0xfb, 0xf1, 0x7e, 0x36, 0x8e, 0x7a, 0x86, 0xde, 0x3d, 0x4e, 0x8a, 0x3a, 0xce,
            0x7a, 0xa1, 0x58, 0xf6, 0xdb, 0x49, 0x3e, 0xc2, 0x2e, 0xcb, 0x4a, 0xbc, 0x19, 0x81,
            0xd5, 0x5d, 0x4f, 0x57, 0x39, 0xf5, 0x9e, 0x02, 0x56, 0x91, 0x37, 0xc2, 0x87, 0x96,
            0x26, 0xd8, 0x4a, 0x45, 0x16, 0x01, 0xe0, 0x2e, 0x20, 0x95, 0x75, 0xb8, 0x20, 0x6d,
            0x83, 0x54, 0x65, 0x3d, 0x66, 0xf4, 0x8a, 0x43, 0x84, 0x9f, 0xa6, 0xc5, 0x2c, 0x08,
            0xe7, 0x59, 0x8e, 0x1f, 0x6d, 0xea, 0x32, 0x5b, 0x36, 0x8e, 0xd1, 0xf3, 0x09, 0x60,
            0x86, 0xdb, 0x55, 0xc9, 0xf0, 0xf9, 0x79, 0x87, 0x71, 0x1c, 0x7c, 0x98, 0xa4, 0xc8,
            0x91, 0x77, 0xa7, 0x95, 0x82, 0x19, 0xcc, 0x9d, 0xde, 0x4d, 0x7b, 0xf7, 0xc1, 0x31,
            0x5b, 0xae, 0x45, 0x6e, 0x6b, 0xf1, 0xaf, 0x89, 0x07, 0x91, 0x80, 0x9d, 0xe5, 0x49,
            0xfc, 0x5e, 0xb2, 0x15, 0x67, 0xcf, 0x05, 0xbb, 0xb3, 0x98, 0x54, 0x34, 0x45, 0x2c,
            0xc3, 0x3d, 0x09, 0x8e, 0x8d, 0x60, 0xba, 0x67, 0xd9, 0xbe, 0x1c, 0x2a, 0x2c, 0x2a,
            0xfa, 0xed, 0x26, 0x81, 0x96, 0x48, 0x17, 0xb3, 0xa6, 0x90, 0x9a, 0x78, 0xa5, 0xac,
            0x80, 0xb2, 0xbe, 0xff, 0x3d, 0x35, 0x00, 0x37, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20,
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55, 0x01, 0x00, 0x10, 0x00, 0x04, 0x40, 0x00, 0x00, 0x0b, 0x00,
            0x04, 0x40, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x20, 0x28, 0xd0, 0x26, 0xfa, 0xfd,
            0x74, 0x91, 0x06, 0x74, 0x3e, 0x27, 0xc4, 0x28, 0x05, 0x51, 0x58, 0x5e, 0x5d, 0x17,
            0x66, 0x8e, 0xb5, 0x21, 0x83, 0x5e, 0xd6, 0x01, 0x27, 0xef, 0xfc, 0x05, 0xd4, 0x80,
            0x21, 0x40, 0x00, 0x00, 0x0b, 0x00, 0x30, 0xfb, 0xfe, 0xd4, 0xe7, 0x9f, 0xc5, 0x2f,
            0xfd, 0x7c, 0xe0, 0x4a, 0x97, 0xb5, 0xec, 0x61, 0x59, 0x4d, 0x43, 0x19, 0x29, 0xc0,
            0x4f, 0xef, 0xda, 0xdc, 0xe1, 0x48, 0x4d, 0xbd, 0x3d, 0x47, 0x0e, 0xe3, 0x2f, 0xd4,
            0xf9, 0x57, 0x4f, 0x77, 0x0f, 0x58, 0x5c, 0x73, 0x58, 0xc2, 0x2d, 0xd7, 0x4a, 0x00,
            0x22, 0x00, 0x0b, 0x92, 0x57, 0x64, 0x38, 0x21, 0xf9, 0x68, 0xe9, 0xfc, 0x47, 0xfa,
            0xbf, 0x9c, 0x56, 0x49, 0x7a, 0x63, 0xc2, 0xc0, 0x8a, 0x12, 0x80, 0x49, 0x73, 0xc3,
            0x8b, 0x00, 0x06, 0x99, 0xe9, 0xfc, 0x22, 0x00, 0x00, 0x01, 0x00, 0x00,
        ];

        const EK_PUB_EXPECTED_CMD: [u8; 355] = [
            0x80, 0x02, 0x00, 0x00, 0x01, 0x63, 0x00, 0x00, 0x01, 0x31, 0x40, 0x00, 0x00, 0x0b,
            0x00, 0x00, 0x00, 0x09, 0x40, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x3a, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x03, 0x00,
            0xb2, 0x00, 0x20, 0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xb3, 0xf8, 0x1a, 0x90, 0xcc,
            0x8d, 0x46, 0xa5, 0xd7, 0x24, 0xfd, 0x52, 0xd7, 0x6e, 0x06, 0x52, 0x0b, 0x64, 0xf2,
            0xa1, 0xda, 0x1b, 0x33, 0x14, 0x69, 0xaa, 0x00, 0x06, 0x00, 0x80, 0x00, 0x43, 0x00,
            0x10, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        const EK_PUB_REPLY_SUCCEED: [u8; 522] = [
            0x80, 0x02, 0x00, 0x00, 0x02, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0xf3, 0x01, 0x3a, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x03, 0x00, 0xb2,
            0x00, 0x20, 0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xb3, 0xf8, 0x1a, 0x90, 0xcc, 0x8d,
            0x46, 0xa5, 0xd7, 0x24, 0xfd, 0x52, 0xd7, 0x6e, 0x06, 0x52, 0x0b, 0x64, 0xf2, 0xa1,
            0xda, 0x1b, 0x33, 0x14, 0x69, 0xaa, 0x00, 0x06, 0x00, 0x80, 0x00, 0x43, 0x00, 0x10,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x9e, 0x9c, 0x1b, 0x40, 0x00, 0x00,
            0xea, 0x2f, 0xd5, 0xd7, 0xde, 0x9b, 0x18, 0x83, 0x55, 0x00, 0x09, 0x53, 0x13, 0xa8,
            0x88, 0x10, 0x24, 0x46, 0x44, 0xa8, 0x2d, 0x62, 0xd3, 0x24, 0xe5, 0xf9, 0xcd, 0xca,
            0x61, 0xb7, 0xd8, 0x15, 0x98, 0xf8, 0x56, 0x64, 0x14, 0x7b, 0x40, 0x5a, 0x47, 0xbd,
            0xd1, 0xc8, 0x7d, 0x1f, 0x93, 0x72, 0x3f, 0x03, 0xe0, 0x29, 0x38, 0x08, 0x03, 0xae,
            0x62, 0x13, 0x10, 0xf5, 0x88, 0x5f, 0x86, 0x84, 0x82, 0xfb, 0xda, 0xd8, 0x78, 0xfd,
            0x02, 0x9e, 0x88, 0x5c, 0xaf, 0x30, 0xd4, 0x3d, 0x41, 0xb2, 0xb7, 0x7a, 0x36, 0xa5,
            0x95, 0x37, 0x08, 0x44, 0x20, 0x10, 0xb3, 0x6c, 0xd0, 0x6d, 0xe9, 0xab, 0xce, 0x35,
            0xc0, 0x82, 0x52, 0x06, 0x41, 0x4c, 0xc5, 0x48, 0x5b, 0xe6, 0x22, 0x00, 0x7e, 0x1d,
            0x4b, 0x68, 0x80, 0x34, 0xe9, 0xea, 0x6e, 0xf9, 0xf7, 0xf7, 0x84, 0xbe, 0x56, 0xdf,
            0xea, 0x85, 0x97, 0x1b, 0x03, 0x5c, 0x5c, 0x9f, 0xf4, 0x72, 0xef, 0xe7, 0xfe, 0x5e,
            0x73, 0x2f, 0xf1, 0xdd, 0x40, 0x80, 0x16, 0x8d, 0x1b, 0x95, 0xee, 0xec, 0x21, 0x1c,
            0x30, 0x84, 0x25, 0x08, 0x8d, 0x0e, 0xda, 0x5b, 0x00, 0x9c, 0x49, 0x8b, 0xc8, 0xb3,
            0x48, 0x9a, 0xc9, 0x19, 0x0f, 0x68, 0xc7, 0x0a, 0x7a, 0x65, 0x35, 0xa0, 0x09, 0x23,
            0x88, 0x3f, 0x97, 0x53, 0x4e, 0xbc, 0x08, 0xc0, 0x5b, 0x69, 0x94, 0xcc, 0xd9, 0xb9,
            0xea, 0x8c, 0x20, 0x9e, 0x1a, 0xf9, 0x57, 0x08, 0x1a, 0xe0, 0x2d, 0x88, 0x56, 0x1f,
            0x9f, 0x50, 0x2e, 0x12, 0xf2, 0x69, 0x9a, 0xdf, 0x30, 0x56, 0xc1, 0xf0, 0x31, 0xef,
            0x64, 0xd5, 0x34, 0x02, 0x15, 0xf4, 0xd7, 0x7b, 0x76, 0xd9, 0x99, 0x24, 0x83, 0x99,
            0xa5, 0x05, 0xc1, 0xcd, 0xa6, 0xbd, 0xc3, 0x3d, 0x7c, 0x1e, 0x94, 0xdd, 0x00, 0x37,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14,
            0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b,
            0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55, 0x01, 0x00, 0x10, 0x00,
            0x04, 0x40, 0x00, 0x00, 0x0b, 0x00, 0x04, 0x40, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00,
            0x20, 0x28, 0xd0, 0x26, 0xfa, 0xfd, 0x74, 0x91, 0x06, 0x74, 0x3e, 0x27, 0xc4, 0x28,
            0x05, 0x51, 0x58, 0x5e, 0x5d, 0x17, 0x66, 0x8e, 0xb5, 0x21, 0x83, 0x5e, 0xd6, 0x01,
            0x27, 0xef, 0xfc, 0x05, 0xd4, 0x80, 0x21, 0x40, 0x00, 0x00, 0x0b, 0x00, 0x30, 0xe2,
            0xf2, 0x64, 0xc3, 0xd7, 0x9e, 0xc1, 0x07, 0xbb, 0x49, 0x74, 0x67, 0xd3, 0xc7, 0xf6,
            0xb7, 0x8c, 0xe3, 0x2e, 0x28, 0x36, 0xa6, 0x1f, 0x6f, 0x0b, 0xbd, 0xe3, 0x8e, 0x77,
            0xa1, 0x8c, 0x50, 0xe4, 0xaa, 0xa4, 0x01, 0x61, 0xb4, 0x7a, 0x4a, 0x3b, 0x5d, 0xac,
            0xe1, 0xd1, 0x65, 0x69, 0x1e, 0x00, 0x22, 0x00, 0x0b, 0xe5, 0x6f, 0x0f, 0xae, 0x8d,
            0x0f, 0x91, 0xb9, 0x84, 0x17, 0xc3, 0x86, 0x13, 0xa6, 0x12, 0xbe, 0xec, 0x85, 0xf9,
            0x0b, 0xd3, 0xfe, 0x4f, 0x3d, 0x79, 0x7d, 0x6d, 0x3c, 0xc5, 0xcc, 0xb1, 0x5b, 0x00,
            0x00, 0x01, 0x00, 0x00,
        ];

        const REPLY_FAIL: [u8; 10] = [0x80, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x02, 0xda];

        // Create AK pub
        let symmetric = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
        let scheme = TpmtRsaScheme::new(AlgIdEnum::RSASSA.into(), Some(AlgIdEnum::SHA256.into()));
        let rsa_params = TpmsRsaParams::new(symmetric, scheme, 2048, 0);

        let object_attributes = TpmaObjectBits::new()
            .with_fixed_tpm(true)
            .with_fixed_parent(true)
            .with_sensitive_data_origin(true)
            .with_user_with_auth(true)
            .with_no_da(true)
            .with_restricted(true)
            .with_sign_encrypt(true);

        let result = TpmtPublic::new(
            AlgIdEnum::RSA.into(),
            AlgIdEnum::SHA256.into(),
            object_attributes,
            &[],
            rsa_params,
            &[0u8; 256],
        );
        assert!(result.is_ok());
        let in_public = result.unwrap();

        let result = CreatePrimaryCmd::new(
            SessionTagEnum::Sessions.into(),
            TPM20_RH_ENDORSEMENT,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            &[],
            &[],
            in_public,
            &[],
            &[],
        );
        assert!(result.is_ok());
        let cmd = result.unwrap();

        let bytes = cmd.serialize();

        assert_eq!(bytes, AK_PUB_EXPECTED_CMD);

        let mut reply = [0u8; 4096];
        reply[..AK_PUB_REPLY_SUCCEED.len()].copy_from_slice(&AK_PUB_REPLY_SUCCEED);

        let response = CreatePrimaryReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x0);
        assert_eq!(response.object_handle.0.get(), 0x80000000);

        reply[..REPLY_FAIL.len()].copy_from_slice(&REPLY_FAIL);

        let response = CreatePrimaryReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x2da);

        // Create EK pub
        const AUTH_POLICY_A_SHA_256: [u8; 32] = [
            0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xB3, 0xF8, 0x1A, 0x90, 0xCC, 0x8D, 0x46, 0xA5,
            0xD7, 0x24, 0xFD, 0x52, 0xD7, 0x6E, 0x06, 0x52, 0x0B, 0x64, 0xF2, 0xA1, 0xDA, 0x1B,
            0x33, 0x14, 0x69, 0xAA,
        ];
        let symmetric = TpmtSymDefObject::new(
            AlgIdEnum::AES.into(),
            Some(128),
            Some(AlgIdEnum::CFB.into()),
        );
        let scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);
        let rsa_params = TpmsRsaParams::new(symmetric, scheme, 2048, 0);

        let object_attributes = TpmaObjectBits::new()
            .with_fixed_tpm(true)
            .with_fixed_parent(true)
            .with_sensitive_data_origin(true)
            .with_admin_with_policy(true)
            .with_restricted(true)
            .with_decrypt(true);

        let result = TpmtPublic::new(
            AlgIdEnum::RSA.into(),
            AlgIdEnum::SHA256.into(),
            object_attributes,
            &AUTH_POLICY_A_SHA_256,
            rsa_params,
            &[0u8; 256],
        );
        assert!(result.is_ok());
        let in_public = result.unwrap();

        let result = CreatePrimaryCmd::new(
            SessionTagEnum::Sessions.into(),
            TPM20_RH_ENDORSEMENT,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            &[],
            &[],
            in_public,
            &[],
            &[],
        );
        assert!(result.is_ok());
        let cmd = result.unwrap();

        let bytes = cmd.serialize();

        assert_eq!(bytes, EK_PUB_EXPECTED_CMD);

        reply[..EK_PUB_REPLY_SUCCEED.len()].copy_from_slice(&EK_PUB_REPLY_SUCCEED);

        let response = CreatePrimaryReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x0);
        assert_eq!(response.object_handle.0.get(), 0x80000000);
    }

    #[test]
    fn test_read_public() {
        const REPLY_SUCCEED: [u8; 364] = [
            0x80, 0x01, 0x00, 0x00, 0x01, 0x6c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x18, 0x00, 0x01,
            0x00, 0x0b, 0x00, 0x05, 0x04, 0x72, 0x00, 0x00, 0x00, 0x10, 0x00, 0x14, 0x00, 0x0b,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0xa6, 0xaf, 0x71, 0xec, 0x00, 0x00,
            0xe0, 0x69, 0xa5, 0xc5, 0xcd, 0x94, 0x59, 0x3b, 0x79, 0xe6, 0xee, 0x14, 0xd3, 0x50,
            0xfb, 0x0b, 0xa9, 0x03, 0x51, 0xbf, 0x23, 0xc5, 0x15, 0xdc, 0xbc, 0x4a, 0x3b, 0xaa,
            0xef, 0x12, 0x3c, 0x24, 0x47, 0xf2, 0x81, 0xf6, 0x85, 0xf4, 0x8c, 0x16, 0x14, 0x10,
            0x3c, 0x3b, 0x2e, 0x7b, 0x04, 0x5e, 0x25, 0x66, 0xcd, 0x8d, 0x86, 0x0b, 0x8c, 0x2b,
            0x5f, 0xca, 0x36, 0x1d, 0x5f, 0xff, 0xbf, 0x70, 0x63, 0x79, 0x5b, 0x7f, 0x93, 0x94,
            0x6d, 0xbd, 0x6e, 0x4f, 0x22, 0x94, 0x93, 0x87, 0xe1, 0x63, 0x4d, 0xa4, 0x9a, 0x2f,
            0xad, 0x90, 0x4c, 0xc9, 0x37, 0x14, 0x59, 0xd3, 0x03, 0x6d, 0x37, 0x98, 0xd4, 0x85,
            0x19, 0x9b, 0x93, 0x7e, 0x61, 0x93, 0x6d, 0x1c, 0xe0, 0xe6, 0x72, 0x71, 0x81, 0x45,
            0xe0, 0xea, 0x5f, 0xb4, 0x6a, 0x9a, 0x3e, 0x86, 0x60, 0x86, 0xaf, 0xfc, 0x86, 0x0f,
            0x0d, 0xe8, 0x81, 0x46, 0x59, 0xad, 0xeb, 0x6f, 0xef, 0x38, 0x5e, 0x53, 0xea, 0x91,
            0xcb, 0xa9, 0xf8, 0x31, 0xcd, 0x52, 0x85, 0x55, 0xa8, 0x91, 0x68, 0xd8, 0xdd, 0x20,
            0x67, 0x21, 0x30, 0x03, 0xcd, 0x48, 0x3b, 0xb0, 0x33, 0x16, 0xb4, 0xf0, 0x06, 0x55,
            0xdf, 0x15, 0xd2, 0x65, 0x55, 0x2f, 0xec, 0xec, 0xc5, 0x74, 0xea, 0xd8, 0x0f, 0x29,
            0xac, 0x24, 0x38, 0x32, 0x34, 0x1f, 0xb3, 0x20, 0x28, 0xf6, 0x55, 0xfb, 0x51, 0xf1,
            0x22, 0xa3, 0x5e, 0x38, 0xc6, 0xa5, 0xa4, 0xe0, 0xc2, 0xa3, 0x50, 0x27, 0xf6, 0x1d,
            0x55, 0x8e, 0x95, 0xe9, 0x95, 0x26, 0x8e, 0x70, 0x35, 0x7b, 0x73, 0xbb, 0x8e, 0xf2,
            0xdc, 0x37, 0x30, 0x99, 0x20, 0x2e, 0x1f, 0x09, 0xbd, 0x85, 0x24, 0x44, 0x05, 0x8f,
            0x11, 0xc4, 0xb5, 0x71, 0xc1, 0x2e, 0x52, 0xf6, 0x2e, 0x6f, 0x9a, 0x11, 0x00, 0x22,
            0x00, 0x0b, 0x61, 0xca, 0x8b, 0xec, 0x0f, 0x9e, 0xc1, 0x38, 0x35, 0xd3, 0x43, 0x58,
            0x77, 0xdf, 0x53, 0x82, 0xe7, 0xb2, 0xff, 0x7b, 0xe4, 0x6c, 0xfb, 0x34, 0xa4, 0x28,
            0xdd, 0xda, 0xcb, 0xe9, 0x50, 0x50, 0x00, 0x22, 0x00, 0x0b, 0x51, 0xfa, 0x43, 0xbd,
            0x35, 0x01, 0xd6, 0x66, 0xa0, 0x4d, 0xc8, 0x03, 0x4f, 0xa1, 0x64, 0xa0, 0x91, 0x63,
            0x3c, 0x27, 0xd5, 0x90, 0xa3, 0x7a, 0xae, 0xbc, 0x52, 0xcc, 0x4e, 0x9a, 0xa3, 0x66,
        ];

        const REPLY_FAIL: [u8; 10] = [0x80, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x01, 0x8b];

        let mut reply = [0u8; 4096];
        reply[..REPLY_SUCCEED.len()].copy_from_slice(&REPLY_SUCCEED);

        let response: Option<ReadPublicReply> = ReadPublicReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x0);

        reply[..REPLY_FAIL.len()].copy_from_slice(&REPLY_FAIL);

        let response = ReadPublicReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x18b);
    }

    #[test]
    fn test_nv_read_public() {
        const REPLY_SUCCEED: [u8; 62] = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x3e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x01, 0x40,
            0x00, 0x01, 0x00, 0x0b, 0x42, 0x06, 0x00, 0x04, 0x00, 0x00, 0x10, 0x00, 0x00, 0x22,
            0x00, 0x0b, 0xc1, 0x0f, 0x8d, 0x61, 0x77, 0xea, 0xd0, 0x29, 0x52, 0xa6, 0x2d, 0x3a,
            0x39, 0xc7, 0x22, 0x0b, 0xb9, 0xa1, 0xe1, 0xfe, 0x08, 0x68, 0xa8, 0x6f, 0x5f, 0x10,
            0xd6, 0x86, 0x83, 0x28, 0x79, 0x3e,
        ];

        const REPLY_FAIL: [u8; 10] = [0x80, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x01, 0x8b];

        let mut reply = [0u8; 4096];
        reply[..REPLY_SUCCEED.len()].copy_from_slice(&REPLY_SUCCEED);

        let response = NvReadPublicReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x0);

        reply[..REPLY_FAIL.len()].copy_from_slice(&REPLY_FAIL);

        let response = NvReadPublicReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x18b);
    }

    #[test]
    fn test_define_space() {
        const EXPECTED_CMD: [u8; 53] = [
            0x80, 0x02, 0x00, 0x00, 0x00, 0x35, 0x00, 0x00, 0x01, 0x2a, 0x40, 0x00, 0x00, 0x0c,
            0x00, 0x00, 0x00, 0x09, 0x40, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x08, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x0e, 0x01, 0xc1, 0x01,
            0xd0, 0x00, 0x0b, 0x42, 0x06, 0x00, 0x04, 0x00, 0x00, 0x10, 0x00,
        ];

        let auth_value: u64 = 0x7766554433221100;

        let result = TpmsNvPublic::new(
            0x1c101d0,
            AlgIdEnum::SHA256.into(),
            platform_akcert_attributes(),
            &[],
            4096,
        );
        assert!(result.is_ok());
        let nv_public = result.unwrap();

        let result = NvDefineSpaceCmd::new(
            SessionTagEnum::Sessions.into(),
            TPM20_RH_PLATFORM,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            auth_value,
            nv_public,
        );
        assert!(result.is_ok());
        let cmd = result.unwrap();

        let bytes = cmd.serialize();
        assert_eq!(bytes, EXPECTED_CMD);
    }

    #[test]
    fn test_nv_write_authwrite() {
        const EXPECTED_CMD: [u8; 171] = [
            0x80, 0x02, 0x00, 0x00, 0x00, 0xab, 0x00, 0x00, 0x01, 0x37, 0x01, 0xc1, 0x01, 0xd0,
            0x01, 0xc1, 0x01, 0xd0, 0x00, 0x00, 0x00, 0x11, 0x40, 0x00, 0x00, 0x09, 0x00, 0x00,
            0x00, 0x00, 0x08, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x80, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x00, 0x00,
        ];
        let auth_value: u64 = 0x7766554433221100;

        let result = NvWriteCmd::new(
            SessionTagEnum::Sessions.into(),
            ReservedHandle(0x1c101d0.into()),
            CmdAuth::new(TPM20_RS_PW, 0, 0, size_of_val(&auth_value) as u16),
            auth_value,
            0x1c101d0,
            &[1u8; 128],
            0,
        );
        assert!(result.is_ok());
        let cmd = result.unwrap();

        let bytes = cmd.serialize();
        assert_eq!(bytes, EXPECTED_CMD);
    }

    #[test]
    fn test_nv_write_ownerwrite() {
        const EXPECTED_CMD: [u8; 163] = [
            0x80, 0x02, 0x00, 0x00, 0x00, 0xa3, 0x00, 0x00, 0x01, 0x37, 0x40, 0x00, 0x00, 0x01,
            0x01, 0xc1, 0x01, 0xd0, 0x00, 0x00, 0x00, 0x09, 0x40, 0x00, 0x00, 0x09, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x80, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
        ];

        let result = NvWriteCmd::new(
            SessionTagEnum::Sessions.into(),
            TPM20_RH_OWNER,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            0,
            0x1c101d0,
            &[1u8; 128],
            0,
        );
        assert!(result.is_ok());
        let cmd = result.unwrap();

        let bytes = cmd.serialize();
        assert_eq!(bytes, EXPECTED_CMD);
    }

    #[test]
    fn test_nv_read() {
        const REPLY_SUCCEED: [u8; 85] = [
            0x80, 0x02, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42,
            0x00, 0x40, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
            0xdd, 0xee, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00,
        ];

        const EXPECTED_DATA: [u8; 64] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let mut reply = [0u8; 4096];
        reply[..REPLY_SUCCEED.len()].copy_from_slice(&REPLY_SUCCEED);

        let response = NvReadReply::deserialize(&reply);
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.header.response_code.get(), 0x0);
        assert_eq!(response.data.buffer[..EXPECTED_DATA.len()], EXPECTED_DATA);
    }
}
