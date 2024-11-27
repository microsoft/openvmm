// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module includes the definition of the VM Metadata blob (VMMD)
//! issued by CVM Provisioning Service (CPS) that is used for parsing
//! the response of the `WRAPPED_KEY_REQUEST`.

use base64_serde::base64_serde_type;
use serde::Deserialize;
use serde::Serialize;

base64_serde_type!(Base64, base64::engine::general_purpose::STANDARD);

/// The `VMMD` blob format (JSON object) defined by CPS.
/// Only include the fields that include the base64-encoded
/// wrapped DiskEncryptionSettings key and the key reference (in JSON).
/// The JSON object looks like
/// ```ignore
/// {
///   "DiskEncryptionSettings" {
///     "encryption_info": {
///       "ase_info": {
///         "ciphertext": <base64-encoded wrapped DiskEncryptionSettings key>
///         ..
///       }
///       "key_reference": <JSON object>
///       ..
///     }
///     ..
///   }
/// }
/// ```
#[derive(Deserialize, Serialize)]
pub struct VmmdBlob {
    /// JSON data
    #[serde(rename = "DiskEncryptionSettings")]
    pub disk_encryption_settings: DiskEncryptionSettings,
}

/// Only include the relevant fields that include base64-encoded wrapped
/// DiskEncryptionSettings key and the key reference JSON object.
#[derive(Deserialize, Serialize)]
pub struct DiskEncryptionSettings {
    /// Encryption info
    pub encryption_info: EncryptionInfo,
}

/// Only include the relevant fields that include base64-encoded wrapped DiskEncryptionSettings
/// key and the key reference JSON object (held in the `key_reference` field).
#[derive(Deserialize, Serialize)]
pub struct EncryptionInfo {
    /// AES information that includes the wrapped key.
    pub aes_info: AesInfo,
    /// JSON object used by the agent in the SKR process
    pub key_reference: serde_json::Value,
}

/// Only include the relevant field the includes the base64-encoded wrapped DiskEncryptionSettings
/// key (held in the `ciphertext` filed).
#[derive(Deserialize, Serialize)]
pub struct AesInfo {
    /// Base64-encoded symmetric key wrapped in RSA-OAEP.
    #[serde(with = "Base64")]
    pub ciphertext: Vec<u8>,
}
