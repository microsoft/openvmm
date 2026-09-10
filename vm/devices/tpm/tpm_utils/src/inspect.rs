// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Inspection of vTPM NVRAM state blobs.

use crate::engine;
use anyhow::Context as _;
use tpm_lib::NvIndexState;
use tpm_protocol::TPM_AZURE_AIK_HANDLE;
use tpm_protocol::TPM_GUEST_SECRET_HANDLE;
use tpm_protocol::TPM_NV_INDEX_AIK_CERT;
use tpm_protocol::TPM_NV_INDEX_ATTESTATION_REPORT;
use tpm_protocol::TPM_NV_INDEX_MITIGATED;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::expected_ak_attributes;
use tpm_protocol::tpm20proto::ReservedHandle;
use tpm_protocol::tpm20proto::TpmaNvBits;
use tpm_resources::TpmVersion;

const OBJECTS: &[(&str, ReservedHandle)] = &[
    ("AK", TPM_AZURE_AIK_HANDLE),
    ("SRK", TPM_RSA_SRK_HANDLE),
    ("guest secret key", TPM_GUEST_SECRET_HANDLE),
];

const NV_INDICES: &[(&str, u32)] = &[
    ("AK cert", TPM_NV_INDEX_AIK_CERT),
    ("attestation report", TPM_NV_INDEX_ATTESTATION_REPORT),
    ("mitigation marker", TPM_NV_INDEX_MITIGATED),
];

/// Loads `blob` into `version` and prints a summary of its contents.
pub fn inspect(version: TpmVersion, blob: &[u8]) -> anyhow::Result<()> {
    let (mut helper, _nvram) = engine::create(version, blob.len(), Some(blob))?;

    println!("nvram size: {} bytes", blob.len());

    println!("persistent objects:");
    for (name, handle) in OBJECTS {
        let handle_str = format!("{:#010x}", handle.0.get());
        match helper
            .find_object(*handle)
            .with_context(|| format!("failed to read {name}"))?
        {
            None => println!("  {name} ({handle_str}): absent"),
            Some(reply) => {
                let attributes = reply.out_public.public_area.object_attributes;
                let mut line = format!("  {name} ({handle_str}): present, attrs {:#010x}", {
                    attributes.0.get()
                });
                if *handle == TPM_AZURE_AIK_HANDLE && attributes != expected_ak_attributes() {
                    line.push_str(" (does not match the expected AK attributes)");
                }
                println!("{line}");
            }
        }
    }

    println!("nv indices:");
    for (name, nv_index) in NV_INDICES {
        let index_str = format!("{nv_index:#010x}");
        let Some(reply) = helper
            .find_nv_index(*nv_index)
            .with_context(|| format!("failed to read the {name} nv index"))?
        else {
            println!("  {name} ({index_str}): absent");
            continue;
        };

        let nv_bits = TpmaNvBits::from(reply.nv_public.nv_public.attributes.0.get());
        let size = reply.nv_public.nv_public.data_size.get();
        let owner = if nv_bits.nv_platformcreate() {
            "platform-created"
        } else {
            "owner-defined"
        };

        // `read_from_nv_index` needs owner read access to tell initialized from
        // uninitialized.
        let state = if nv_bits.nv_ownerread() {
            let mut output = vec![0; size as usize];
            match helper.read_from_nv_index(*nv_index, &mut output) {
                Ok(NvIndexState::Available) => "initialized",
                Ok(NvIndexState::Uninitialized) => "uninitialized",
                Ok(NvIndexState::Unallocated) => "unallocated",
                Err(err) => {
                    tracing::warn!(
                        nv_index = *nv_index,
                        error = &err as &dyn std::error::Error,
                        "failed to read nv index contents"
                    );
                    "unreadable"
                }
            }
        } else {
            "no owner read"
        };

        println!(
            "  {name} ({index_str}): present, {size} bytes, {owner}, {state}, attrs {:#010x}",
            reply.nv_public.nv_public.attributes.0.get()
        );
    }

    Ok(())
}
