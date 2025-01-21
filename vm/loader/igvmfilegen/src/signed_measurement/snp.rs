// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for creating SNP ID blocks

use super::SHA_384_OUTPUT_SIZE_BYTES;
use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmInitializationHeader;
use igvm_defs::IgvmPageDataType;
use igvm_defs::PAGE_SIZE_4K;
use sha2::Digest;
use sha2::Sha384;
use std::collections::HashMap;
use thiserror::Error;
use x86defs::snp::SnpPageInfo;
use x86defs::snp::SnpPageType;
use x86defs::snp::SnpPspIdBlock;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid parameter area index")]
    InvalidParameterAreaIndex,
}

/// Iterate through all headers, creating a launch digest which is then signed,
/// returning an [`IgvmDirectiveHeader::SnpIdBlock`]
pub fn generate_snp_measurement(
    initialization_headers: &[IgvmInitializationHeader],
    directive_headers: &[IgvmDirectiveHeader],
    svn: u32,
) -> Result<[u8; SHA_384_OUTPUT_SIZE_BYTES], Error> {
    let mut parameter_area_table = HashMap::new();
    const PAGE_SIZE_4K_USIZE: usize = PAGE_SIZE_4K as usize;
    let snp_compatibility_mask = DEFAULT_COMPATIBILITY_MASK;

    let mut launch_digest: [u8; SHA_384_OUTPUT_SIZE_BYTES] = [0; SHA_384_OUTPUT_SIZE_BYTES];
    let zero_page: [u8; PAGE_SIZE_4K as usize] = [0; PAGE_SIZE_4K as usize];
    let mut hasher = Sha384::new();

    // Hash the contents of empty 4K page, used when file does not carry data
    hasher.update(zero_page.as_bytes());
    let zero_digest = hasher.finalize();

    // Reuse the same vec for padding out data to 4k.
    let mut padding_vec = vec![0; PAGE_SIZE_4K_USIZE];

    let mut measure_page = |page_type: SnpPageType, gpa: u64, page_data: Option<&[u8]>| {
        let mut hash = Sha384::new();
        let hash_contents = match page_data {
            Some(data) => {
                match data.len() {
                    0 => zero_digest,
                    _ if data.len() < PAGE_SIZE_4K_USIZE => {
                        padding_vec.fill(0);
                        padding_vec[..data.len()].copy_from_slice(data);
                        hash.update(&padding_vec);
                        hash.finalize()
                    }
                    PAGE_SIZE_4K_USIZE => {
                        hash.update(data);
                        hash.finalize()
                    }
                    _ => {
                        // TODO SNP: Need to check the PSP spec how to measure 2MB
                        // pages. Fail for now, as they shouldn't exist.
                        todo!(
                            "unable to measure greater than 4k pages, len: {}",
                            data.len()
                        )
                    }
                }
            }
            None => [0; SHA_384_OUTPUT_SIZE_BYTES].into(),
        };

        let info = SnpPageInfo {
            digest_current: launch_digest,
            contents: hash_contents.into(),
            length: size_of::<SnpPageInfo>() as u16,
            page_type,
            imi_page_bit: 0,
            lower_vmpl_permissions: 0,
            gpa,
        };

        let mut hash = Sha384::new();
        hash.update(info.as_bytes());
        launch_digest = hash.finalize().into();
    };

    let mut policy: u64 = 0;

    for header in initialization_headers {
        if let IgvmInitializationHeader::GuestPolicy {
            policy: snp_policy,
            compatibility_mask,
        } = header
        {
            assert_eq!(
                compatibility_mask & snp_compatibility_mask,
                snp_compatibility_mask
            );
            policy = *snp_policy;
        }
    }
    assert_ne!(policy, 0);

    // Loop over all the page data to build the digest
    for header in directive_headers {
        // Skip headers that have compatibility masks that do not match snp.
        if header
            .compatibility_mask()
            .map(|mask| mask & snp_compatibility_mask != snp_compatibility_mask)
            .unwrap_or(false)
        {
            continue;
        }

        match header {
            IgvmDirectiveHeader::ErrorRange { .. } => todo!("error range not implemented"),
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
                data_type,
                data,
            } => {
                assert_eq!(
                    compatibility_mask & snp_compatibility_mask,
                    snp_compatibility_mask
                );

                // Skip shared pages.
                if flags.shared() {
                    continue;
                }

                let (page_type, data) = match *data_type {
                    IgvmPageDataType::SECRETS => (SnpPageType::SECRETS, None),
                    IgvmPageDataType::CPUID_DATA | IgvmPageDataType::CPUID_XF => {
                        (SnpPageType::CPUID, None)
                    }
                    _ => {
                        if flags.unmeasured() {
                            (SnpPageType::UNMEASURED, None)
                        } else {
                            (SnpPageType::NORMAL, Some(data.as_bytes()))
                        }
                    }
                };

                measure_page(page_type, *gpa, data);
            }
            IgvmDirectiveHeader::ParameterInsert(param) => {
                assert_eq!(
                    param.compatibility_mask & snp_compatibility_mask,
                    snp_compatibility_mask
                );

                let parameter_area_size = parameter_area_table
                    .get(&param.parameter_area_index)
                    .ok_or(Error::InvalidParameterAreaIndex)?;

                for gpa in (param.gpa..param.gpa + *parameter_area_size).step_by(PAGE_SIZE_4K_USIZE)
                {
                    measure_page(SnpPageType::UNMEASURED, gpa, None)
                }
            }
            IgvmDirectiveHeader::SnpVpContext {
                gpa,
                compatibility_mask,
                vp_index: _,
                vmsa,
            } => {
                assert_eq!(
                    compatibility_mask & snp_compatibility_mask,
                    snp_compatibility_mask
                );

                let vmsa_bytes = vmsa.as_ref().as_bytes();
                measure_page(SnpPageType::VMSA, *gpa, Some(vmsa_bytes));
            }
            _ => {}
        }
    }

    let family_id = *b"msft\0\0\0\0\0\0\0\0\0\0\0\0";
    let image_id = *b"underhill\0\0\0\0\0\0\0";

    // Generate the PSP ID block format, hash with SHA-384
    let psp_id_block = SnpPspIdBlock {
        ld: launch_digest,
        version: 0x1,
        guest_svn: svn,
        policy,
        family_id,
        image_id,
    };
    // Print the ID block for reference, not currently used.
    tracing::info!("SNP ID Block {:x?}", psp_id_block);
    Ok(psp_id_block.ld)
}
