// This is a good source: https://msazure.visualstudio.com/One/_git/Azure-Compute-Move?path=%2Fsrc%2FServices%2Fsecurity%2FAzureDiskEncryption%2FConfidentialVMUtil%2Flib%2FMarshal.cpp&_a=contents&version=GBmaster

use tpm::tpm20proto::protocol::{Tpm2bBuffer, Tpm2bPublic};
use tpm::tpm20proto::AlgId;

//use std::mem::{size_of, ManuallyDrop};

use zerocopy::AsBytes;

// struct Tpm2b {
//     size: u16,
//     buffer: [u8; 1],
// }

// Table 67 -- TPMT_HA Structure <I/O>
// const SHA_1_OUTPUT_SIZE_BYTES: usize = 20;
// const SHA_256_OUTPUT_SIZE_BYTES: usize = 32;
// const SM3_256_DIGEST_SIZE: usize = 32;
// const SHA_384_OUTPUT_SIZE_BYTES: usize = 48;
// const SHA_512_OUTPUT_SIZE_BYTES: usize = 64;

// union TpmuHash {
//     sha1: [u8; SHA_1_OUTPUT_SIZE_BYTES],
//     sha256: [u8; SHA_256_OUTPUT_SIZE_BYTES],
//     sha384: [u8; SHA_384_OUTPUT_SIZE_BYTES],
//     sha512: [u8; SHA_512_OUTPUT_SIZE_BYTES],
//     sm3_256: [u8; SM3_256_DIGEST_SIZE],
// }

// pub struct TpmtHash {
//     hash_alg: AlgId,
//     digest: TpmuHash,
// }

// Table 68 -- TPM2B_DIGEST Structure <I/O>
// struct Tpm2bDigest {
//     size: u16,
//     buffer: [u8; size_of::<TpmuHash>()],
// }

// Table 69 -- TPM2B_DATA Structure <I/O>
// struct Data2b {
//     size: u16,
//     buffer: [u8; 64],
// }
// union Tpm2bData {
//     t: ManuallyDrop<Data2b>,
//     b: ManuallyDrop<Tpm2b>,
// }

// Table 71 -- TPM2B_AUTH Types <I/O>
// type Tpm2bAuth = Tpm2bDigest;


// Table 172 -- TPMT_SIGNATURE Structure <I/O>
// struct TpmtSignature {
//     sig_alg: AlgId,
//     signature: Tpm2bBuffer,
// }

/*
// Table 174 -- TPM2B_ENCRYPTED_SECRET Structure <I/O>
#[repr(C)]
struct EncryptedSecret2b {
    size: u16,
    secret: [u8;
        size_of::<TpmuEncryptedSecret>()
    ],
}

#[repr(C)]
union TpmuEncryptedSecret {
    sym: EncryptedSecret2b,
    asym: EncryptedSecret2b,
}
*/

#[repr(C)]
pub struct Tpm2bPrivate {
    size: u16,
    buffer: Vec<u8>, // Adjust the size as needed
}

impl Default for Tpm2bPrivate {
    fn default() -> Self {
        Tpm2bPrivate {
            size: 0,
            buffer: Vec::new(),
        }
    }
}

// Table 187 -- TPMT_SENSITIVE Structure <I/O>
#[repr(C)]
pub struct TpmtSensitive {
    /// TPMI_ALG_PUBLIC
    pub sensitive_type: AlgId,
    /// `TPM2B_AUTH`
    pub auth_value: Tpm2bBuffer,
    /// `TPM2B_DIGEST`
    pub seed_value: Tpm2bBuffer,
    /// `TPM2B_PRIVATE_KEY_RSA`
    pub sensitive: Tpm2bBuffer,
}

// impl TpmtSensitive {
//     fn serialize(&self) -> Vec<u8> {
//         let mut buffer = Vec::new();

//         buffer.extend_from_slice(self.sensitive_type.as_bytes());
//         buffer.extend_from_slice(&self.auth_value.serialize());
//         buffer.extend_from_slice(&self.seed_value.serialize());
//         buffer.extend_from_slice(&self.sensitive.serialize());

//         buffer
//     }
// }

/// Marshals the `TpmtSensitive` structure into a buffer.
pub fn tpmt_sensitive_marshal(
    source: &TpmtSensitive,
) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&source.sensitive_type.as_bytes());

    Ok(buffer)
}

/*
pub fn tpmi_alg_public_marshal(
    value: &AlgId,
    buffer: &mut &mut [u8],
    size: &mut i32,
) -> u16 {
    // Implement the marshaling logic here
    0
}

pub fn tpm2b_auth_marshal(value: &Tpm2bAuth, buffer: &mut &mut [u8], size: &mut i32) -> u16 {
    // Implement the marshaling logic here
    0
}

pub fn tpm2b_digest_marshal(value: &Tpm2bDi, buffer: &mut &mut [u8], size: &mut i32) -> u16 {
    // Implement the marshaling logic here
    0
}

pub fn tpmu_sensitive_composite_marshal(
    value: &TPMU_SENSITIVE_COMPOSITE,
    buffer: &mut &mut [u8],
    size: &mut i32,
    sensitive_type: u32,
) -> u16 {
    // Implement the marshaling logic here
    0
}
*/

pub fn marshal_tpm2b_import(
    tpm2b_public: &Tpm2bPublic,
    tpm2b_private: &Tpm2bBuffer,
) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&tpm2b_public.serialize());
    buffer.extend_from_slice(&tpm2b_private.serialize());
    Ok(buffer)
}