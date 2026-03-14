// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

///! Marshal TPM structures: selected ones only for our use case in cvmutil.
///! TPM reference documents such as TPM-Rev-2.0-Part-2-Structures-01.38.pdf is a good source.

use tpm::tpm20proto::AlgId;
use tpm::tpm20proto::protocol::Tpm2bBuffer;
use crate::Tpm2bPublic;
//use tpm::tpm20proto::protocol::Tpm2bPublic;
use zerocopy::{FromZeros, IntoBytes};
use std::io::{self, Read, Cursor};

// Constants for sealed key data format (from Canonical Go secboot package)
pub const KEY_DATA_HEADER: u32 = 0x55534b24; // "USK$" magic bytes
pub const KEY_POLICY_UPDATE_DATA_HEADER: u32 = 0x55534b50;
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
pub struct AfSplitData {
    pub stripes: u32,
    pub hash_alg: u16,  // TPM hash algorithm ID is 2 bytes
    pub size: u32,
    pub data: Vec<u8>,
}

/// TPM Key Data structure matching Go's tpmKeyData
#[derive(Debug)]
pub struct TpmKeyData {
    pub version: u32,
    pub key_private: Tpm2bBuffer,        // Parsed TPM2B_PRIVATE
    pub key_public: Tpm2bPublic,         // Parsed TPM2B_PUBLIC  
    pub auth_mode_hint: u8,
    pub import_sym_seed: Tpm2bBuffer,    // Parsed TPM2B_ENCRYPTED_SECRET
    pub static_policy_data: Option<Vec<u8>>,   // Placeholder for static policy data
    pub dynamic_policy_data: Option<Vec<u8>>,  // Placeholder for dynamic policy data
}

/// Sealed key import blob that matches TPM2B import format (TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET)
#[derive(Debug)]
pub struct SealedKeyImportBlob {
    pub object_public: Tpm2bPublic,
    pub duplicate: Tpm2bBuffer, 
    pub in_sym_seed: Tpm2bBuffer,
}

impl AfSplitData {
    /// Create AF split data from payload using proper AFIS algorithm
    pub fn create(payload: &[u8]) -> Self {
        use sha2::{Digest, Sha256};

        // Use Canonical's approach: target 128KB minimum size
        let min_size = 128 * 1024; // 128KB like Canonical
        let stripes = (min_size / payload.len()).max(1) + 1;

        tracing::info!(
            "AF split: payload {} bytes, {} stripes, target size ~{}KB",
            payload.len(),
            stripes,
            (payload.len() * stripes) / 1024
        );

        let block_size = payload.len();
        let mut result = Vec::new();
        let mut block = vec![0u8; block_size];

        // Generate stripes-1 random blocks and XOR/hash them
        for _i in 0..(stripes - 1) {
            let mut random_block = vec![0u8; block_size];
            getrandom::fill(&mut random_block).expect("Failed to generate random data");

            result.extend_from_slice(&random_block);

            // XOR with accumulated block
            for j in 0..block_size {
                block[j] ^= random_block[j];
            }

            // Diffuse the block using hash (same as in merge)
            let mut hasher = Sha256::new();
            hasher.update(&block);
            let hash = hasher.finalize();

            // Simple diffusion: XOR block with repeated hash
            for j in 0..block_size {
                block[j] ^= hash[j % 32];
            }
        }

        // Final stripe: XOR the accumulated block with original data
        let mut final_stripe = vec![0u8; block_size];
        for i in 0..block_size {
            final_stripe[i] = block[i] ^ payload[i];
        }
        result.extend_from_slice(&final_stripe);

        AfSplitData {
            stripes: stripes as u32,
            hash_alg: 8, // SHA256 hash algorithm ID
            size: result.len() as u32,
            data: result,
        }
    }

    /// Parse AF split data from raw bytes using TPM2 binary format
    pub fn from_bytes(data: &[u8]) -> Result<Self, io::Error> {
        let mut cursor = Cursor::new(data);
        
        tracing::debug!("AF Split parsing: total data length = {}", data.len());
        
        // Read stripes (4 bytes, LITTLE endian to match our export format)
        if data.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for stripes"));
        }
        let stripes = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        
        // Read hash algorithm ID (4 bytes, LITTLE endian - we export as u32, not u16)
        if data.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for hash algorithm"));
        }
        let hash_alg_u32 = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let hash_alg = hash_alg_u32 as u16;  // Convert to u16 for compatibility
        
        // Read size (2 bytes, LITTLE endian to match our export format)
        if data.len() < 10 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for size"));
        }
                // Read size (4 bytes, LITTLE endian to match our export format)
        if data.len() < 12 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for size"));
        }
        let size = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);

        tracing::debug!("AF Split header: stripes={}, hash_alg=0x{:04x}, size={}", stripes, hash_alg, size);
        tracing::debug!("Expected AF data: {} stripes * {} bytes/stripe = {} total bytes", 
            stripes, size as usize / stripes as usize, size);

        // The data follows immediately after the header
        let data_start = 12; // 4 + 4 + 4 bytes for stripes, hash_alg, size
        if data.len() < data_start + size as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, 
                format!("AF split data truncated: expected {} bytes, got {} bytes", 
                    data_start + size as usize, data.len())));
        }

        let split_data = data[data_start..data_start + size as usize].to_vec();
        
        tracing::debug!("AF split validation: split_data.len()={}, stripes={}, remainder={}", 
            split_data.len(), stripes, split_data.len() % stripes as usize);

        Ok(AfSplitData {
            stripes,
            hash_alg,
            size,
            data: split_data,
        })
    }

    /// Serialize the AF split data to bytes in the format expected by Ubuntu secboot
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut af_data = Vec::new();
        af_data.extend_from_slice(&self.stripes.to_le_bytes()); // 4 bytes: stripe count
        af_data.extend_from_slice(&(self.hash_alg as u32).to_le_bytes()); // 4 bytes: hash algorithm ID
        af_data.extend_from_slice(&self.size.to_le_bytes()); // 4 bytes: AF data length
        af_data.extend_from_slice(&self.data);
        af_data
    }
    
    /// Merge the AF split data to recover original data using proper AFIS algorithm
    pub fn merge(&self) -> Result<Vec<u8>, io::Error> {
        use sha2::{Sha256, Digest};
        
        // Basic validation
        if self.stripes < 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid number of stripes"));
        }
        
        tracing::info!("AF Split merge debug: stripes={}, data.len()={}, remainder={}", 
            self.stripes, self.data.len(), self.data.len() % self.stripes as usize);
        
        if self.data.len() % self.stripes as usize != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, 
                format!("Data length {} is not multiple of stripes {}, remainder {}", 
                    self.data.len(), self.stripes, self.data.len() % self.stripes as usize)));
        }
        
        let block_size = self.data.len() / self.stripes as usize;
        let mut block = vec![0u8; block_size];
        
        tracing::info!("AF Split merge: {} stripes, {} bytes total, {} bytes per block", 
            self.stripes, self.data.len(), block_size);
        
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
        
        tracing::info!("AF split merge successful: recovered {} bytes", original_data.len());
        Ok(original_data)
    }
}

impl SealedKeyImportBlob {
    /// Create a SealedKeyImportBlob from raw bytes in TPM2B import format
    pub fn _from_bytes(data: &[u8]) -> Result<Self, io::Error> {
        // Parse TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET format
        let mut offset = 0;
        
        // Parse TPM2B_PUBLIC
        let object_public = Tpm2bPublic::deserialize(&data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PUBLIC"))?;
        offset += object_public.payload_size();
        
        // Parse TPM2B_PRIVATE (as TPM2B_BUFFER for the duplicate field)
        let duplicate = Tpm2bBuffer::deserialize(&data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PRIVATE"))?;
        offset += duplicate.payload_size();
        
        // Parse TPM2B_ENCRYPTED_SECRET 
        let in_sym_seed = Tpm2bBuffer::deserialize(&data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_ENCRYPTED_SECRET"))?;
        
        tracing::info!("Successfully parsed sealed key import blob:");
        tracing::info!("  TPM2B_PUBLIC size: {} bytes", object_public.payload_size());
        tracing::info!("  TPM2B_PRIVATE size: {} bytes", duplicate.payload_size());  
        tracing::info!("  TPM2B_ENCRYPTED_SECRET size: {} bytes", in_sym_seed.payload_size());
        
        Ok(SealedKeyImportBlob {
            object_public,
            duplicate,
            in_sym_seed,
        })
    }
}

impl TpmKeyData {
    /// Parse TPM key data from bytes
    pub fn from_bytes(mut data: &[u8]) -> Result<Self, io::Error> {
        // Read header
        if data.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for header"));
        }
        
        let header = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        data = &data[4..];

        if header != KEY_DATA_HEADER {
            return Err(io::Error::new(io::ErrorKind::InvalidData, 
                format!("Invalid header: expected 0x{:08X}, got 0x{:08X}", KEY_DATA_HEADER, header)));
        }

        // Read version
        if data.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for version"));
        }
        
        let version = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        data = &data[4..];

        tracing::info!("Parsing sealed key data version: {}", version);

        match version {
            0 => Self::parse_v0(data, version),
            1 => Self::parse_v1(data, version),
            2 => Self::parse_v2(data, version),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, 
                format!("Unsupported version: {}", version))),
        }
    }

    fn parse_v0(data: &[u8], version: u32) -> Result<Self, io::Error> {
        // Version 0 format - direct marshaling without AF split
        // This is a simplified parser - full implementation would need detailed parsing
        tracing::info!("Parsing version 0 sealed key data");
        
        Ok(TpmKeyData {
            version,
            key_private: Tpm2bBuffer::new_zeroed(),  // Would parse TPM2B_PRIVATE
            key_public: Tpm2bPublic::new_zeroed(),   // Would parse TPM2B_PUBLIC
            auth_mode_hint: 0,
            import_sym_seed: Tpm2bBuffer::new_zeroed(),
            static_policy_data: None,
            dynamic_policy_data: None,
        })
    }

    fn parse_v1(data: &[u8], version: u32) -> Result<Self, io::Error> {
        // Version 1 format - with AF split data
        tracing::info!("Parsing version 1 sealed key data");
        
        let af_split_data = AfSplitData::from_bytes(data)?;
        let merged_data = af_split_data.merge()?;

        // Parse the merged data - simplified implementation
        Ok(TpmKeyData {
            version,
            key_private: Tpm2bBuffer::new_zeroed(),
            key_public: Tpm2bPublic::new_zeroed(), 
            auth_mode_hint: 0,
            import_sym_seed: Tpm2bBuffer::new_zeroed(),
            static_policy_data: None,
            dynamic_policy_data: None,
        })
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
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for TPM2B_PRIVATE"));
        }
        
        let key_private = Tpm2bBuffer::deserialize(&merged_data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PRIVATE"))?;
        offset += key_private.payload_size();
        
        tracing::debug!("Parsed TPM2B_PRIVATE: {} bytes", key_private.payload_size());
        
        // Parse TPM2B_PUBLIC
        if merged_data.len() < offset + 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for TPM2B_PUBLIC"));
        }
        
        let key_public = Tpm2bPublic::deserialize(&merged_data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_PUBLIC"))?;
        offset += key_public.payload_size();
        
        tracing::debug!("Parsed TPM2B_PUBLIC: {} bytes", key_public.payload_size());
        
        // Parse auth_mode_hint (1 byte)
        if merged_data.len() < offset + 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for auth_mode_hint"));
        }
        
        let auth_mode_hint = merged_data[offset];
        offset += 1;
        
        tracing::debug!("Parsed auth_mode_hint: {}", auth_mode_hint);
        
        // Parse TPM2B_ENCRYPTED_SECRET
        if merged_data.len() < offset + 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Data too short for TPM2B_ENCRYPTED_SECRET"));
        }
        
        let import_sym_seed = Tpm2bBuffer::deserialize(&merged_data[offset..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Failed to parse TPM2B_ENCRYPTED_SECRET"))?;
        offset += import_sym_seed.payload_size();
        
        tracing::debug!("Parsed TPM2B_ENCRYPTED_SECRET: {} bytes", import_sym_seed.payload_size());
        tracing::info!("Successfully parsed all TPM structures from merged data, total offset: {}", offset);
        
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
    tracing::trace!(
        "Marshaling sensitive_type: {} bytes = {:02X?}",
        sensitive_type_bytes.len(),
        sensitive_type_bytes
    );
    buffer.extend_from_slice(&sensitive_type_bytes);

    // Marshal auth_value (TPM2B_AUTH) - size + data
    let auth_value_bytes = source.auth_value.serialize();
    tracing::trace!(
        "Marshaling auth_value: {} bytes = {:02X?}",
        auth_value_bytes.len(),
        if auth_value_bytes.len() <= 8 {
            &auth_value_bytes[..]
        } else {
            &auth_value_bytes[..8]
        }
    );
    buffer.extend_from_slice(&auth_value_bytes);

    // Marshal seed_value (TPM2B_DIGEST) - size + data
    let seed_value_bytes = source.seed_value.serialize();
    tracing::trace!(
        "Marshaling seed_value: {} bytes = {:02X?}",
        seed_value_bytes.len(),
        if seed_value_bytes.len() <= 8 {
            &seed_value_bytes[..]
        } else {
            &seed_value_bytes[..8]
        }
    );
    buffer.extend_from_slice(&seed_value_bytes);

    // Marshal sensitive (TPMU_SENSITIVE_COMPOSITE) for RSA
    // Based on C++ TPM2B_PRIVATE_KEY_RSA_Marshal, this should be:
    // 1. uint16_t size (of the buffer data)
    // 2. byte array data (the actual prime data)
    let sensitive_bytes = source.sensitive.serialize();
    tracing::trace!(
        "Marshaling sensitive: {} bytes = {:02X?}",
        sensitive_bytes.len(),
        if sensitive_bytes.len() <= 8 {
            &sensitive_bytes[..]
        } else {
            &sensitive_bytes[..8]
        }
    );
    let data_size = sensitive_bytes.len() as u16;
    buffer.extend_from_slice(&data_size.to_be_bytes());
    buffer.extend_from_slice(&sensitive_bytes);

    tracing::trace!("Total marshaled TPMT_SENSITIVE: {} bytes", buffer.len());
    Ok(buffer)
}
