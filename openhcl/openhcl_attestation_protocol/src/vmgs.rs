// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Include modules that define the data structures of VMGS entries.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Number of the key protector entries.
/// One for ingress, and one for egress
pub const NUMBER_KP: usize = 2;

/// DEK buffer size
pub const DEK_BUFFER_SIZE: usize = 512;

/// GSP buffer size
pub const GSP_BUFFER_SIZE: usize = 512;

/// Size of the `FileId::KEY_PROTECTOR` VMGS file entry.
pub const KEY_PROTECTOR_SIZE: usize = size_of::<KeyProtector>();

/// DEK key protector entry.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DekKp {
    /// DEK buffer
    pub dek_buffer: [u8; DEK_BUFFER_SIZE],
}

/// GSP key protector entry.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GspKp {
    /// GSP data size
    pub gsp_length: u32,
    /// GSP buffer
    pub gsp_buffer: [u8; GSP_BUFFER_SIZE],
}

/// The data format of the `FileId::KEY_PROTECTOR` entry in the VMGS file.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeyProtector {
    /// Array of DEK entries
    pub dek: [DekKp; NUMBER_KP],
    /// Array of GSP entries
    pub gsp: [GspKp; NUMBER_KP],
    /// Index of the activate entry
    pub active_kp: u32,
}

/// The data format of the host/fabric-provided key protector.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KeyProtectorById {
    /// Id
    pub id_guid: guid::Guid,
    /// Ported (boolean)
    pub ported: u8,
    /// Padding
    pub pad: [u8; 3],
}

/// Maximum size of the `agent_data`.
pub const AGENT_DATA_MAX_SIZE: usize = 2048;

/// The data format of the `FileId::ATTEST` entry in the VMGS file.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SecurityProfile {
    /// the agent data used during attestation requests
    pub agent_data: [u8; AGENT_DATA_MAX_SIZE],
}

/// The header, IV, and last 256 bits of HMAC are fixed for this version.
/// The ciphertext is allowed to grow, though secrets should stay
/// in the same position to allow downlevel versions to continue to understand
/// that portion of the data.
pub const HW_KEY_VERSION: u32 = 1; // using AES-CBC-HMAC-SHA256

/// The size of the `FileId::HW_KEY_PROTECTOR` entry in the VMGS file.
pub const HW_KEY_PROTECTOR_SIZE: usize = size_of::<HardwareKeyProtector>();

/// AES-GCM key size
pub const AES_GCM_KEY_LENGTH: usize = 32;

/// AES-CBC key size
pub const AES_CBC_KEY_LENGTH: usize = AES_GCM_KEY_LENGTH;

/// AES-CVC IV size
pub const AES_CBC_IV_LENGTH: usize = 16;

/// HACK-SHA-256 key size
pub const HMAC_SHA_256_KEY_LENGTH: usize = 32;

/// The header of [`HardwareKeyProtector`].
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HardwareKeyProtectorHeader {
    /// Version of the format
    pub version: u32,
    /// Size of the [`HardwareKeyProtector`] data blob
    pub length: u32,
    /// TCB version obtained from the hardware
    pub tcb_version: u64,
    /// Reserved
    pub _reserved: [u8; 8],
}

impl HardwareKeyProtectorHeader {
    /// Create a `HardwareKeyProtectorHeader` instance.
    pub fn new(version: u32, length: u32, tcb_version: u64) -> Self {
        Self {
            version,
            length,
            tcb_version,
            _reserved: [0u8; 8],
        }
    }
}

/// The data format of the `FileId::HW_KEY_PROTECTOR` entry in the VMGS file.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HardwareKeyProtector {
    /// Header
    pub header: HardwareKeyProtectorHeader,
    /// Random IV for AES-CBC
    pub iv: [u8; AES_CBC_IV_LENGTH],
    /// Encrypted key
    pub ciphertext: [u8; AES_GCM_KEY_LENGTH],
    /// HMAC-SHA-256 of [`header`, `iv`, `ciphertext`]
    pub hmac: [u8; HMAC_SHA_256_KEY_LENGTH],
}

/// Maximum size of the `guest_secret_key`.
pub const GUEST_SECRET_KEY_MAX_SIZE: usize = 2048;

/// The data format of the `FileId::GUEST_SECRET_KEY` entry in the VMGS file.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GuestSecretKey {
    /// the guest secret key to be provisioned to vTPM
    pub guest_secret_key: [u8; GUEST_SECRET_KEY_MAX_SIZE],
}
