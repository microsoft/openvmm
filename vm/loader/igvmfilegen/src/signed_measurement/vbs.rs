// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for VBS measurements

use super::SHA_256_OUTPUT_SIZE_BYTES;
use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use igvm::IgvmDirectiveHeader;
use igvm_defs::IgvmPageDataType;
use igvm_defs::VbsDigestAlgorithm;
use igvm_defs::VbsSigningAlgorithm;
use igvm_defs::VbsVpContextRegister;
use igvm_defs::PAGE_SIZE_4K;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use thiserror::Error;
use vbs_defs::BootMeasurementType;
use vbs_defs::VbsChunkHeader;
use vbs_defs::VbsRegisterChunk;
use vbs_defs::VpGpaPageChunk;
use vbs_defs::VBS_POLICY_FLAGS;
use vbs_defs::VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA;
use vbs_defs::VBS_VM_GPA_PAGE_BOOT_METADATA;
use vbs_defs::VBS_VP_CHUNK_SIZE_BYTES;
use vbs_defs::VM_GPA_PAGE_READABLE;
use vbs_defs::VM_GPA_PAGE_WRITABLE;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid parameter area index")]
    InvalidParameterAreaIndex,
}

/// Iterate through all headers, creating a boot measurement which is then signed,
/// returning an [`IgvmDirectiveHeader::VbsMeasurement`]
pub fn generate_vbs_measurement(
    directive_headers: &[IgvmDirectiveHeader],
    enable_debug: bool,
    svn: u32,
) -> Result<[u8; SHA_256_OUTPUT_SIZE_BYTES], Error> {
    const VBS_COMPATIBILITY_MASK: u32 = DEFAULT_COMPATIBILITY_MASK;

    let mut digest = VbsDigestor::new()?;
    let mut parameter_area_table = HashMap::new();
    let mut bsp_regs = Vec::new();

    for header in directive_headers {
        // Skip headers that have compatibility masks that do not match vbs.
        if header
            .compatibility_mask()
            .map(|mask| mask & VBS_COMPATIBILITY_MASK != VBS_COMPATIBILITY_MASK)
            .unwrap_or(false)
        {
            continue;
        }

        match header {
            IgvmDirectiveHeader::PageData {
                gpa,
                compatibility_mask,
                flags,
                data_type,
                data,
            } => {
                assert_eq!(
                    compatibility_mask & VBS_COMPATIBILITY_MASK,
                    VBS_COMPATIBILITY_MASK
                );

                assert_eq!(*data_type, IgvmPageDataType::NORMAL);

                // Skip shared pages.
                if flags.shared() {
                    continue;
                }

                let boot_metadata = VBS_VM_GPA_PAGE_BOOT_METADATA::new()
                    .with_acceptance(0)
                    .with_data_unmeasured(flags.unmeasured());
                digest.record_gpa_page(gpa / PAGE_SIZE_4K, 1, boot_metadata, data)?;
            }
            IgvmDirectiveHeader::ParameterInsert(param) => {
                let page_metadata = VBS_VM_GPA_PAGE_BOOT_METADATA::new()
                    .with_acceptance(0)
                    .with_data_unmeasured(true);
                let parameter_area_size = parameter_area_table
                    .get(&param.parameter_area_index)
                    .ok_or(Error::InvalidParameterAreaIndex)?;
                digest.record_gpa_page(
                    param.gpa / PAGE_SIZE_4K,
                    parameter_area_size / PAGE_SIZE_4K,
                    page_metadata,
                    &[],
                )?;
            }
            IgvmDirectiveHeader::X64VbsVpContext {
                vtl,
                registers,
                compatibility_mask,
            } => {
                assert_eq!(
                    compatibility_mask & VBS_COMPATIBILITY_MASK,
                    VBS_COMPATIBILITY_MASK
                );
                // The Vbs measurement format requires the cpu context to be measured last, measure at end
                let vtl_registers: Vec<VbsVpContextRegister> = registers
                    .iter()
                    .map(|r| r.into_vbs_vp_context_reg(*vtl))
                    .collect();
                bsp_regs.push(vtl_registers);
            }
            IgvmDirectiveHeader::AArch64VbsVpContext {
                vtl,
                registers,
                compatibility_mask,
            } => {
                assert_eq!(
                    compatibility_mask & VBS_COMPATIBILITY_MASK,
                    VBS_COMPATIBILITY_MASK
                );
                // The Vbs measurement format requires the cpu context to be measured last, measure at end
                let vtl_registers: Vec<VbsVpContextRegister> = registers
                    .iter()
                    .map(|r| r.into_vbs_vp_context_reg(*vtl))
                    .collect();
                bsp_regs.push(vtl_registers);
            }
            IgvmDirectiveHeader::ErrorRange {
                gpa,
                compatibility_mask,
                size_bytes,
            } => {
                assert_eq!(
                    compatibility_mask & VBS_COMPATIBILITY_MASK,
                    VBS_COMPATIBILITY_MASK
                );
                let page_metadata = VBS_VM_GPA_PAGE_BOOT_METADATA::new()
                    .with_acceptance(VM_GPA_PAGE_READABLE | VM_GPA_PAGE_WRITABLE)
                    .with_data_unmeasured(true);
                digest.record_gpa_page(
                    *gpa / PAGE_SIZE_4K,
                    (*size_bytes as u64).div_ceil(PAGE_SIZE_4K),
                    page_metadata,
                    &[],
                )?;
            }
            IgvmDirectiveHeader::ParameterArea {
                number_of_bytes,
                parameter_area_index,
                initial_data: _,
            } => {
                if parameter_area_table.contains_key(parameter_area_index) {
                    return Err(Error::InvalidParameterAreaIndex);
                }
                parameter_area_table.insert(parameter_area_index, *number_of_bytes);
            }
            _ => {}
        }
    }

    // Measure all registers in each VTL as last step in measurement
    for set in bsp_regs {
        for reg in set {
            digest.record_vp_register(reg)?;
        }
    }

    // Identifier constants chosen to maintain compatibility with internal tooling
    const MSFT_PRODUCT_ID: u32 = u32::from_le_bytes(*b"msft");
    const VBS_MODULE_ID: u32 = u32::from_le_bytes(*b"vbs\0");
    const VBS_VM_BOOT_MEASUREMENT_VERSION_CURRENT: u32 = 0x1;

    let boot_measurement = VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA {
        version: VBS_VM_BOOT_MEASUREMENT_VERSION_CURRENT,
        product_id: MSFT_PRODUCT_ID,
        module_id: VBS_MODULE_ID,
        security_version: svn,
        security_policy: VBS_POLICY_FLAGS::new().with_debug(enable_debug),
        boot_digest_algo: VbsDigestAlgorithm::SHA256.0,
        signing_algo: VbsSigningAlgorithm::ECDSA_P384.0,
        boot_measurement_digest: digest.finish_digest(),
    };
    // Print the signing data for reference, not currently used.
    tracing::info!("Boot Measurement {:x?}", boot_measurement);
    Ok(boot_measurement.boot_measurement_digest)
}

struct VbsDigestor {
    digest: [u8; SHA_256_OUTPUT_SIZE_BYTES],
}

impl VbsDigestor {
    fn new() -> Result<VbsDigestor, Error> {
        Ok(VbsDigestor {
            digest: [0; SHA_256_OUTPUT_SIZE_BYTES],
        })
    }

    fn record_gpa_page(
        &mut self,
        gpa_page_base: u64,
        page_count: u64,
        page_metadata: VBS_VM_GPA_PAGE_BOOT_METADATA,
        mut data: &[u8],
    ) -> Result<(), Error> {
        for page in 0..page_count {
            let import_data_len: usize = match page_metadata.data_unmeasured() {
                true => 0,
                false => std::cmp::min(PAGE_SIZE_4K as usize, data.len()),
            };
            let (import_data, data_remaining) = data.split_at(import_data_len);
            data = data_remaining;

            // If page is under 4K bytes, pad to full length which will be hashed with page and chunk data
            let padding = vec![0; PAGE_SIZE_4K as usize - import_data.len()];
            let page_number = gpa_page_base + page;
            let chunk = VpGpaPageChunk {
                header: VbsChunkHeader {
                    byte_count: VBS_VP_CHUNK_SIZE_BYTES as u32,
                    chunk_type: BootMeasurementType::VP_GPA_PAGE,
                    reserved: 0,
                },
                metadata: page_metadata.into(),
                page_number,
            };
            self.create_record_entry(&[chunk.as_bytes(), import_data, &padding])?;
        }
        Ok(())
    }

    fn record_vp_register(&mut self, reg: VbsVpContextRegister) -> Result<(), Error> {
        let chunk = VbsRegisterChunk {
            header: VbsChunkHeader {
                byte_count: size_of::<VbsRegisterChunk>() as u32,
                chunk_type: BootMeasurementType::VP_REGISTER,
                reserved: 0,
            },
            reserved: 0,
            vtl: reg.vtl,
            reserved2: 0,
            reserved3: 0,
            reserved4: 0,
            name: reg.register_name.into(),
            value: reg.register_value,
        };
        self.create_record_entry(&[chunk.as_bytes()])?;
        Ok(())
    }

    fn create_record_entry(&mut self, chunks: &[&[u8]]) -> Result<(), Error> {
        let mut hasher = Sha256::new();
        hasher.update(self.digest.as_bytes());
        for chunk in chunks {
            hasher.update(chunk);
        }
        self.digest = hasher.finalize().into();
        Ok(())
    }

    fn finish_digest(&self) -> [u8; SHA_256_OUTPUT_SIZE_BYTES] {
        self.digest
    }
}
