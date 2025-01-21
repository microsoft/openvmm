// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for creating TDX MRTD

use super::SHA_384_OUTPUT_SIZE_BYTES;
use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use igvm::IgvmDirectiveHeader;
use igvm_defs::PAGE_SIZE_4K;
use sha2::Digest;
use sha2::Sha384;
use std::collections::HashMap;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid parameter area index")]
    InvalidParameterAreaIndex,
}

/// Measure adding a page to TD.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdxPageAdd {
    /// MEM.PAGE.ADD
    pub operation: [u8; 16],
    /// Must be aligned to a page size boundary.
    pub gpa: u64,
    /// Reserved mbz.
    pub mbz: [u8; 104],
}

const TDX_EXTEND_CHUNK_SIZE: usize = 256;

/// Measure adding a chunk of data to TD.
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TdxMrExtend {
    /// MR.EXTEND
    pub operation: [u8; 16],
    /// Aligned to a 256B boundary.
    pub gpa: u64,
    /// Reserved mbz.
    pub mbz: [u8; 104],
    /// Data to measure.
    pub data: [u8; TDX_EXTEND_CHUNK_SIZE],
}

/// Iterate through all headers to create the MRTD.
pub fn generate_tdx_measurement(
    directive_headers: &[IgvmDirectiveHeader],
) -> Result<[u8; SHA_384_OUTPUT_SIZE_BYTES], Error> {
    let mut parameter_area_table = HashMap::new();
    const PAGE_SIZE_4K_USIZE: usize = PAGE_SIZE_4K as usize;
    let tdx_compatibility_mask = DEFAULT_COMPATIBILITY_MASK;
    // Reuse the same vec for padding out data to 4k.
    let mut padding_vec = vec![0; PAGE_SIZE_4K_USIZE];
    let mut hasher = Sha384::new();

    let mut measure_page = |gpa: u64, page_data: Option<&[u8]>| {
        // Measure the page being added.
        let page_add = TdxPageAdd {
            operation: *b"MEM.PAGE.ADD\0\0\0\0",
            gpa,
            mbz: [0; 104],
        };
        hasher.update(page_add.as_bytes());

        // Possibly measure the page contents in chunks.
        if let Some(data) = page_data {
            let data = match data.len() {
                0 => None,
                PAGE_SIZE_4K_USIZE => Some(data),
                _ if data.len() < PAGE_SIZE_4K_USIZE => {
                    padding_vec.fill(0);
                    padding_vec[..data.len()].copy_from_slice(data);
                    Some(padding_vec.as_slice())
                }
                _ => {
                    panic!("Unexpected data size");
                }
            };

            // Hash the contents of the 4K page, 256 bytes at a time.
            for offset in (0..PAGE_SIZE_4K).step_by(TDX_EXTEND_CHUNK_SIZE) {
                let mut mr_extend = TdxMrExtend {
                    operation: *b"MR.EXTEND\0\0\0\0\0\0\0",
                    gpa: gpa + offset,
                    mbz: [0; 104],
                    data: [0; TDX_EXTEND_CHUNK_SIZE],
                };

                // Copy in data for chunk if it exists.
                if let Some(data) = data {
                    mr_extend.data.copy_from_slice(
                        &data[offset as usize..offset as usize + TDX_EXTEND_CHUNK_SIZE],
                    );
                }
                hasher.update(mr_extend.as_bytes());
            }
        };
    };

    // Loop over all the page data to build the digest
    for header in directive_headers {
        // Skip headers that have compatibility masks that do not match TDX.
        if header
            .compatibility_mask()
            .map(|mask| mask & tdx_compatibility_mask != tdx_compatibility_mask)
            .unwrap_or(false)
        {
            continue;
        }

        match header {
            IgvmDirectiveHeader::ParameterArea {
                number_of_bytes,
                parameter_area_index,
                initial_data: _,
            } => {
                assert_eq!(
                    parameter_area_table.contains_key(&parameter_area_index),
                    false
                );
                assert_eq!(number_of_bytes % PAGE_SIZE_4K, 0);
                parameter_area_table.insert(parameter_area_index, number_of_bytes);
            }
            IgvmDirectiveHeader::PageData {
                gpa,
                compatibility_mask,
                flags,
                data_type: _,
                data,
            } => {
                assert_eq!(
                    compatibility_mask & tdx_compatibility_mask,
                    tdx_compatibility_mask
                );

                // Skip shared pages.
                if flags.shared() {
                    continue;
                }

                // If data is unmeasured, only measure the GPA.
                let data = if flags.unmeasured() {
                    None
                } else {
                    Some(data.as_bytes())
                };

                measure_page(*gpa, data);
            }
            IgvmDirectiveHeader::ParameterInsert(param) => {
                assert_eq!(
                    param.compatibility_mask & tdx_compatibility_mask,
                    tdx_compatibility_mask
                );

                let parameter_area_size = parameter_area_table
                    .get(&param.parameter_area_index)
                    .ok_or(Error::InvalidParameterAreaIndex)?;

                for gpa in (param.gpa..param.gpa + *parameter_area_size).step_by(PAGE_SIZE_4K_USIZE)
                {
                    measure_page(gpa, None);
                }
            }
            _ => {}
        }
    }

    let mrtd: [u8; SHA_384_OUTPUT_SIZE_BYTES] = hasher.finalize().into();
    tracing::info!("MRTD: {}", hex::encode_upper(mrtd));
    Ok(mrtd)
}
