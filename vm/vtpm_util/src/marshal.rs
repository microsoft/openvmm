// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Marshal selected TPM structures used by `vtpm_util`.
//! TPM reference documents such as TPM-Rev-2.0-Part-2-Structures-01.38.pdf are a good source.

#[cfg(feature = "experimental")]
use crate::Tpm2bPublic;
use std::io;
use tpm_protocol::tpm20proto::AlgId;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use zerocopy::IntoBytes;

// Constants for sealed key data format (from Canonical Go secboot package)
#[cfg(feature = "experimental")]
pub const KEY_DATA_HEADER: u32 = 0x55534b24; // "USK$" magic bytes
#[cfg(feature = "experimental")]
pub const CURRENT_METADATA_VERSION: u32 = 2;

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

/// Anti-Forensic Information Splitter data structure
#[derive(Debug)]
#[cfg(feature = "experimental")]
pub struct AfSplitData {
    pub stripes: u32,
    pub data: Vec<u8>,
}

/// TPM Key Data structure matching Go's tpmKeyData
#[derive(Debug)]
#[expect(dead_code)]
#[cfg(feature = "experimental")]
pub struct TpmKeyData {
    pub version: u32,
    pub key_private: Tpm2bBuffer, // Parsed TPM2B_PRIVATE
    pub key_public: Tpm2bPublic,  // Parsed TPM2B_PUBLIC
    pub auth_mode_hint: u8,
    pub import_sym_seed: Tpm2bBuffer, // Parsed TPM2B_ENCRYPTED_SECRET
    pub static_policy_data: Option<Vec<u8>>, // Placeholder for static policy data
    pub dynamic_policy_data: Option<Vec<u8>>, // Placeholder for dynamic policy data
}

/// Sealed key import blob that matches TPM2B import format (TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET)
#[derive(Debug)]
#[cfg(feature = "experimental")]
pub struct SealedKeyImportBlob {
    pub object_public: Tpm2bPublic,
    pub duplicate: Tpm2bBuffer,
    pub in_sym_seed: Tpm2bBuffer,
}

#[cfg(feature = "experimental")]
impl AfSplitData {
    /// Split data using the AFIS algorithm.
    pub fn create(payload: &[u8]) -> Result<Self, io::Error> {
        use sha2::{Digest, Sha256};

        if payload.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot AF-split an empty payload",
            ));
        }

        const MIN_SPLIT_SIZE: usize = 128 * 1024;
        let stripes = MIN_SPLIT_SIZE / payload.len() + 1;
        let split_size = payload.len().checked_mul(stripes).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "AF split data is too large")
        })?;
        let mut data = Vec::with_capacity(split_size);
        let mut block = vec![0u8; payload.len()];

        for _ in 0..stripes - 1 {
            let mut random_block = vec![0u8; payload.len()];
            getrandom::fill(&mut random_block)
                .map_err(|error| io::Error::other(error.to_string()))?;
            data.extend_from_slice(&random_block);

            for index in 0..payload.len() {
                block[index] ^= random_block[index];
            }

            let hash = Sha256::digest(&block);
            for index in 0..payload.len() {
                block[index] ^= hash[index % hash.len()];
            }
        }

        for index in 0..payload.len() {
            block[index] ^= payload[index];
        }
        data.extend_from_slice(&block);

        Ok(Self {
            stripes: stripes.try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Too many AF split stripes")
            })?,
            data,
        })
    }

    /// Serialize the AF split data and its header.
    pub fn to_bytes(&self) -> Result<Vec<u8>, io::Error> {
        let size: u32 = self.data.len().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "AF split data is too large")
        })?;
        let mut output = Vec::with_capacity(12 + self.data.len());
        output.extend_from_slice(&self.stripes.to_le_bytes());
        output.extend_from_slice(&8u32.to_le_bytes());
        output.extend_from_slice(&size.to_le_bytes());
        output.extend_from_slice(&self.data);
        Ok(output)
    }

    /// Parse AF split data from raw bytes using TPM2 binary format
    pub fn from_bytes(data: &[u8]) -> Result<Self, io::Error> {
        tracing::debug!("AF Split parsing: total data length = {}", data.len());

        // Read stripes (4 bytes, LITTLE endian to match our export format)
        if data.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for stripes",
            ));
        }
        let stripes = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if stripes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid number of stripes",
            ));
        }

        // Read hash algorithm ID (4 bytes, LITTLE endian - we export as u32, not u16)
        if data.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for hash algorithm",
            ));
        }
        let hash_alg_u32 = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let hash_alg = hash_alg_u32 as u16; // Convert to u16 for compatibility

        // Read size (2 bytes, LITTLE endian to match our export format)
        if data.len() < 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for size",
            ));
        }
        // Read size (4 bytes, LITTLE endian to match our export format)
        if data.len() < 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for size",
            ));
        }
        let size = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);

        tracing::debug!(
            "AF Split header: stripes={}, hash_alg=0x{:04x}, size={}",
            stripes,
            hash_alg,
            size
        );
        tracing::debug!(
            "Expected AF data: {} stripes * {} bytes/stripe = {} total bytes",
            stripes,
            size as usize / stripes as usize,
            size
        );

        // The data follows immediately after the header
        let data_start = 12; // 4 + 4 + 4 bytes for stripes, hash_alg, size
        if data.len() < data_start + size as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "AF split data truncated: expected {} bytes, got {} bytes",
                    data_start + size as usize,
                    data.len()
                ),
            ));
        }

        let split_data = data[data_start..data_start + size as usize].to_vec();

        tracing::debug!(
            "AF split validation: split_data.len()={}, stripes={}, remainder={}",
            split_data.len(),
            stripes,
            split_data.len() % stripes as usize
        );

        Ok(AfSplitData {
            stripes,
            data: split_data,
        })
    }

    /// Merge the AF split data to recover original data using proper AFIS algorithm
    pub fn merge(&self) -> Result<Vec<u8>, io::Error> {
        use sha2::{Digest, Sha256};

        // Basic validation
        if self.stripes < 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid number of stripes",
            ));
        }

        tracing::info!(
            "AF Split merge debug: stripes={}, data.len()={}, remainder={}",
            self.stripes,
            self.data.len(),
            self.data.len() % self.stripes as usize
        );

        if !self.data.len().is_multiple_of(self.stripes as usize) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Data length {} is not multiple of stripes {}, remainder {}",
                    self.data.len(),
                    self.stripes,
                    self.data.len() % self.stripes as usize
                ),
            ));
        }

        let block_size = self.data.len() / self.stripes as usize;
        let mut block = vec![0u8; block_size];

        tracing::info!(
            "AF Split merge: {} stripes, {} bytes total, {} bytes per block",
            self.stripes,
            self.data.len(),
            block_size
        );

        // Reverse the AF split algorithm:
        // 1. XOR and hash-diffuse the first (stripes-1) blocks
        for i in 0..(self.stripes - 1) as usize {
            let offset = i * block_size;
            let stripe_data = &self.data[offset..offset + block_size];

            // XOR with accumulated block
            for j in 0..block_size {
                block[j] ^= stripe_data[j];
            }

            // Diffuse the block using hash (same as in create_af_split_data)
            let mut hasher = Sha256::new();
            hasher.update(&block);
            let hash = hasher.finalize();

            // Simple diffusion: XOR block with repeated hash
            for j in 0..block_size {
                block[j] ^= hash[j % 32];
            }
        }

        // 2. XOR the final stripe with the accumulated block to recover original data
        let final_stripe_offset = ((self.stripes - 1) as usize) * block_size;
        let final_stripe = &self.data[final_stripe_offset..final_stripe_offset + block_size];

        let mut original_data = vec![0u8; block_size];
        for i in 0..block_size {
            original_data[i] = block[i] ^ final_stripe[i];
        }

        tracing::info!(
            "AF split merge successful: recovered {} bytes",
            original_data.len()
        );
        Ok(original_data)
    }
}

#[cfg(feature = "experimental")]
impl SealedKeyImportBlob {
    /// Create a SealedKeyImportBlob from raw bytes in TPM2B import format
    pub fn _from_bytes(data: &[u8]) -> Result<Self, io::Error> {
        // Parse TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET format
        let mut offset = 0;

        // Parse TPM2B_PUBLIC
        let object_public = Tpm2bPublic::deserialize(&data[offset..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PUBLIC")
        })?;
        offset += object_public.payload_size();

        // Parse TPM2B_PRIVATE (as TPM2B_BUFFER for the duplicate field)
        let duplicate = Tpm2bBuffer::deserialize(&data[offset..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PRIVATE")
        })?;
        offset += duplicate.payload_size();

        // Parse TPM2B_ENCRYPTED_SECRET
        let in_sym_seed = Tpm2bBuffer::deserialize(&data[offset..]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Failed to parse TPM2B_ENCRYPTED_SECRET",
            )
        })?;

        tracing::info!("Successfully parsed sealed key import blob:");
        tracing::info!(
            "  TPM2B_PUBLIC size: {} bytes",
            object_public.payload_size()
        );
        tracing::info!("  TPM2B_PRIVATE size: {} bytes", duplicate.payload_size());
        tracing::info!(
            "  TPM2B_ENCRYPTED_SECRET size: {} bytes",
            in_sym_seed.payload_size()
        );

        Ok(SealedKeyImportBlob {
            object_public,
            duplicate,
            in_sym_seed,
        })
    }
}

#[cfg(feature = "experimental")]
impl TpmKeyData {
    /// Parse TPM key data from bytes
    pub fn from_bytes(mut data: &[u8]) -> Result<Self, io::Error> {
        // Read header
        if data.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for header",
            ));
        }

        let header = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        data = &data[4..];

        if header != KEY_DATA_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Invalid header: expected 0x{:08X}, got 0x{:08X}",
                    KEY_DATA_HEADER, header
                ),
            ));
        }

        // Read version
        if data.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for version",
            ));
        }

        let version = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        data = &data[4..];

        tracing::info!("Parsing sealed key data version: {}", version);

        match version {
            2 => Self::parse_v2(data, version),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported version: {}", version),
            )),
        }
    }

    fn parse_v2(data: &[u8], version: u32) -> Result<Self, io::Error> {
        // Version 2 format - with AF split data and import symmetric seed
        tracing::info!("Parsing version 2 sealed key data");

        tracing::debug!("Raw data length: {} bytes", data.len());
        if data.len() >= 16 {
            tracing::debug!("First 16 bytes: {:02x?}", &data[..16]);
        }

        let af_split_data = AfSplitData::from_bytes(data)?;
        tracing::info!("Successfully parsed AF split data");

        let merged_data = af_split_data.merge()?;
        tracing::info!("AF split data merged, {} bytes", merged_data.len());

        // Parse the merged data which contains: TPM2B_PRIVATE || TPM2B_PUBLIC || auth_mode_hint || TPM2B_ENCRYPTED_SECRET
        let mut offset = 0;

        // Parse TPM2B_PRIVATE
        if merged_data.len() < offset + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for TPM2B_PRIVATE",
            ));
        }

        let key_private = Tpm2bBuffer::deserialize(&merged_data[offset..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PRIVATE")
        })?;
        offset += key_private.payload_size();

        tracing::debug!("Parsed TPM2B_PRIVATE: {} bytes", key_private.payload_size());

        // Parse TPM2B_PUBLIC
        if merged_data.len() < offset + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for TPM2B_PUBLIC",
            ));
        }

        let key_public = Tpm2bPublic::deserialize(&merged_data[offset..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PUBLIC")
        })?;
        offset += key_public.payload_size();

        tracing::debug!("Parsed TPM2B_PUBLIC: {} bytes", key_public.payload_size());

        // Parse auth_mode_hint (1 byte)
        if merged_data.len() < offset + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for auth_mode_hint",
            ));
        }

        let auth_mode_hint = merged_data[offset];
        offset += 1;

        tracing::debug!("Parsed auth_mode_hint: {}", auth_mode_hint);

        // Parse TPM2B_ENCRYPTED_SECRET
        if merged_data.len() < offset + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Data too short for TPM2B_ENCRYPTED_SECRET",
            ));
        }

        let import_sym_seed =
            Tpm2bBuffer::deserialize(&merged_data[offset..]).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Failed to parse TPM2B_ENCRYPTED_SECRET",
                )
            })?;
        offset += import_sym_seed.payload_size();

        tracing::debug!(
            "Parsed TPM2B_ENCRYPTED_SECRET: {} bytes",
            import_sym_seed.payload_size()
        );
        tracing::info!(
            "Successfully parsed all TPM structures from merged data, total offset: {}",
            offset
        );

        Ok(TpmKeyData {
            version,
            key_private,
            key_public,
            auth_mode_hint,
            import_sym_seed,
            static_policy_data: None,
            dynamic_policy_data: None,
        })
    }

    /// Extract TPM import blob format from the sealed key data
    pub fn to_import_blob(&self) -> SealedKeyImportBlob {
        SealedKeyImportBlob {
            object_public: self.key_public,
            duplicate: self.key_private,
            in_sym_seed: self.import_sym_seed,
        }
    }
}

/// Marshals the `TpmtSensitive` structure into a buffer.
pub fn tpmt_sensitive_marshal(source: &TpmtSensitive) -> Result<Vec<u8>, io::Error> {
    let mut buffer = Vec::new();

    // Marshal sensitive_type (TPMI_ALG_PUBLIC) - 2 bytes
    let sensitive_type_bytes = source.sensitive_type.as_bytes();
    buffer.extend_from_slice(sensitive_type_bytes);

    // Marshal auth_value (TPM2B_AUTH) - size + data
    let auth_value_bytes = source.auth_value.serialize();
    buffer.extend_from_slice(&auth_value_bytes);

    // Marshal seed_value (TPM2B_DIGEST) - size + data
    let seed_value_bytes = source.seed_value.serialize();
    buffer.extend_from_slice(&seed_value_bytes);

    // Marshal sensitive (TPMU_SENSITIVE_COMPOSITE) for RSA
    let sensitive_bytes = source.sensitive.serialize();
    buffer.extend_from_slice(&sensitive_bytes);

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "experimental")]
    use super::AfSplitData;
    #[cfg(feature = "experimental")]
    use super::KEY_DATA_HEADER;
    #[cfg(feature = "experimental")]
    use super::TpmKeyData;
    use super::TpmtSensitive;
    use super::tpmt_sensitive_marshal;
    use test_with_tracing::test;
    use tpm_protocol::tpm20proto::AlgIdEnum;
    use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;

    #[test]
    #[cfg(feature = "experimental")]
    fn af_split_rejects_empty_payload() {
        let error = AfSplitData::create(&[]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    #[cfg(feature = "experimental")]
    fn af_split_round_trips() {
        let payload = b"AF split payload";
        let encoded = AfSplitData::create(payload).unwrap().to_bytes().unwrap();
        let decoded = AfSplitData::from_bytes(&encoded).unwrap();

        assert_eq!(decoded.merge().unwrap(), payload);
    }

    #[test]
    #[cfg(feature = "experimental")]
    fn af_split_rejects_zero_stripes() {
        let error = AfSplitData::from_bytes(&[0; 12]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(feature = "experimental")]
    fn tpm_key_data_rejects_unimplemented_versions() {
        for version in [0u32, 1] {
            let mut data = KEY_DATA_HEADER.to_be_bytes().to_vec();
            data.extend_from_slice(&version.to_be_bytes());

            let error = TpmKeyData::from_bytes(&data).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn tpmt_sensitive_has_one_size_prefix_per_buffer() {
        let sensitive = TpmtSensitive {
            sensitive_type: AlgIdEnum::RSA.into(),
            auth_value: Tpm2bBuffer::new(&[]).unwrap(),
            seed_value: Tpm2bBuffer::new(&[]).unwrap(),
            sensitive: Tpm2bBuffer::new(&[1, 2, 3]).unwrap(),
        };

        assert_eq!(
            tpmt_sensitive_marshal(&sensitive).unwrap(),
            [0, 1, 0, 0, 0, 0, 0, 3, 1, 2, 3]
        );
    }
}
