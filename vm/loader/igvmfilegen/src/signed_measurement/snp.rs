// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for creating SNP ID blocks

use super::SHA_384_OUTPUT_SIZE_BYTES;
use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmInitializationHeader;
use igvm_defs::IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY;
use igvm_defs::IGVM_VHS_SNP_ID_BLOCK_SIGNATURE;
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
    #[error("failed to sign temporary SNP ID block: {0}")]
    TempSigning(String),
}

const SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384: u32 = 1;
const SNP_ECDSA_CURVE_P384: u32 = 2;
const SNP_ECC_KEY_SIZE_BYTES: usize = 48;
const SNP_ECC_COMPONENT_SIZE_BYTES: usize = 72;

/// Iterate through all headers, creating a launch digest which is then signed,
/// returning the launch digest. Also emits a temporarily-signed
/// [`IgvmDirectiveHeader::SnpIdBlock`] directive (the presence of this directive
/// signals the IGVM loader to set `id_block_en = 1` at launch time).
pub fn generate_snp_measurement(
    initialization_headers: &[IgvmInitializationHeader],
    directive_headers: &mut Vec<IgvmDirectiveHeader>,
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
    for header in directive_headers.iter() {
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

    // Underhill family ID for the SNP ID block.
    const UNDERHILL_FAMILY_ID: [u8; 16] = [
        0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let family_id = UNDERHILL_FAMILY_ID;
    let image_id = *b"underhill\0\0\0\0\0\0\0";

    // Generate the PSP ID block format, hash with SHA-384.
    let psp_id_block = SnpPspIdBlock {
        ld: launch_digest,
        version: 0x1,
        guest_svn: svn,
        policy,
        family_id,
        image_id,
    };

    // Print the ID block for reference.
    tracing::info!("SNP ID Block {:x?}", psp_id_block);

    // Generate a temporary key and sign the ID block hash.
    let (id_key_signature, id_public_key) = sign_id_block_with_temp_key(&psp_id_block)?;
    directive_headers.push(IgvmDirectiveHeader::SnpIdBlock {
        compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
        author_key_enabled: 0,
        reserved: [0; 3],
        ld: psp_id_block.ld,
        family_id: psp_id_block.family_id,
        image_id: psp_id_block.image_id,
        version: psp_id_block.version,
        guest_svn: psp_id_block.guest_svn,
        id_key_algorithm: SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384,
        author_key_algorithm: 0,
        id_key_signature: Box::new(id_key_signature),
        id_public_key: Box::new(id_public_key),
        author_key_signature: Box::new(IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
            r_comp: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
            s_comp: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
        }),
        author_public_key: Box::new(IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
            curve: 0,
            reserved: 0,
            qx: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
            qy: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
        }),
    });

    Ok(psp_id_block.ld)
}

/// Zero-pads and reverses a big-endian ECC component into a 72-byte
/// little-endian array as required by the PSP ID block format.
fn padded_le_component(input_be: &[u8]) -> [u8; SNP_ECC_COMPONENT_SIZE_BYTES] {
    let mut out = [0u8; SNP_ECC_COMPONENT_SIZE_BYTES];
    for (dst, src) in out.iter_mut().zip(input_be.iter().rev()) {
        *dst = *src;
    }
    out
}

/// Generate a temporary ECDSA P-384 key pair using the selected `crypto`
/// backend, sign the SHA-384 hash of the ID block, and return the signature
/// + public key in the format expected by `IGVM_VHS_SNP_ID_BLOCK`.
fn sign_id_block_with_temp_key(
    id_block: &SnpPspIdBlock,
) -> Result<
    (
        IGVM_VHS_SNP_ID_BLOCK_SIGNATURE,
        IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY,
    ),
    Error,
> {
    use crypto::ecdsa::{EcdsaCurve, EcdsaKeyPair};

    // Generate a random P-384 key pair for ECDSA signing.
    let key = EcdsaKeyPair::generate(EcdsaCurve::P384)
        .map_err(|e| Error::TempSigning(format!("EcdsaKeyPair::generate: {e}")))?;

    // Hash the ID block with SHA-384.
    let mut hash = Sha384::new();
    hash.update(id_block.as_bytes());
    let id_block_hash: [u8; SHA_384_OUTPUT_SIZE_BYTES] = hash.finalize().into();

    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    tracing::info!("Input Hash Base64: {}", b64.encode(id_block_hash));
    tracing::info!("Using Temporary Signing Key");

    // Sign the hash. Returns r || s in big-endian, each 48 bytes for P-384.
    let signature = key
        .sign_prehash(&id_block_hash)
        .map_err(|e| Error::TempSigning(format!("sign_prehash: {e}")))?;

    if signature.len() != SNP_ECC_KEY_SIZE_BYTES * 2 {
        return Err(Error::TempSigning(format!(
            "unexpected signature size {}",
            signature.len()
        )));
    }

    let (sig_r_be, sig_s_be) = signature.split_at(SNP_ECC_KEY_SIZE_BYTES);
    let id_key_signature = IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
        r_comp: padded_le_component(sig_r_be),
        s_comp: padded_le_component(sig_s_be),
    };

    tracing::info!("Signature R Base64: {}", b64.encode(sig_r_be));
    tracing::info!("Signature S Base64: {}", b64.encode(sig_s_be));

    // Export the public key as Qx || Qy in big-endian, each 48 bytes for P-384.
    let public_key = key
        .public_key_bytes()
        .map_err(|e| Error::TempSigning(format!("public_key_bytes: {e}")))?;

    if public_key.len() != SNP_ECC_KEY_SIZE_BYTES * 2 {
        return Err(Error::TempSigning(format!(
            "unexpected public key size {}",
            public_key.len()
        )));
    }

    let (qx_be, qy_be) = public_key.split_at(SNP_ECC_KEY_SIZE_BYTES);

    tracing::info!("Public Key Qx Base64: {}", b64.encode(qx_be));
    tracing::info!("Public Key Qy Base64: {}", b64.encode(qy_be));
    let id_public_key = IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
        curve: SNP_ECDSA_CURVE_P384,
        reserved: 0,
        qx: padded_le_component(qx_be),
        qy: padded_le_component(qy_be),
    };

    Ok((id_key_signature, id_public_key))
}
