// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for product policy validation and serialization.

#[cfg(feature = "manifest")]
/// Custom serialization and deserialization for UEFI JSON bytes which is picked from igvm recipe.
pub mod custom_uefi_json_serde {
    extern crate alloc;

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use base64::Engine as _;
    use serde::Deserialize as _;

    /// Serialize a slice of bytes as a base64-encoded string.
    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        s.serialize_str(&encoded)
    }

    /// Deserialize a base64-encoded string into a vector of bytes.
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(|e| serde::de::Error::custom(format!("failed to base64-decode bytes: {e}")))
    }
}
