// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provisioning of vTPM NVRAM state.

use crate::engine;
use anyhow::Context as _;
use tpm_lib::TpmEngine as _;
use tpm_protocol::TPM_AZURE_AIK_HANDLE;
use tpm_protocol::TPM_DEFAULT_AKCERT_SIZE;
use tpm_protocol::TPM_NV_INDEX_AIK_CERT;
use tpm_protocol::TPM_NV_INDEX_MITIGATED;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::TPM20_RH_PLATFORM;
use tpm_resources::TpmVersion;

/// Who owns the AK cert NV index.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AkCertIndexKind {
    /// Do not create the index.
    None,
    /// Owner-defined, matching how `vtpmservice` pre-provisions a vTPM.
    Owner,
    /// Platform-created, matching how OpenHCL provisions the index at boot.
    Platform,
}

/// What to provision into the TPM state.
pub struct ProvisionParams {
    /// The reference implementation to provision against.
    pub version: TpmVersion,
    /// Size of the NVRAM region to manufacture.
    pub nvram_size: usize,
    /// Persist an AK at `TPM_AZURE_AIK_HANDLE`.
    pub ak: bool,
    /// Persist an RSA SRK at `TPM_RSA_SRK_HANDLE`.
    pub srk: bool,
    /// Ownership of the AK cert NV index.
    pub ak_cert_index: AkCertIndexKind,
    /// Size of the AK cert NV index. Defaults to the cert size, or
    /// `TPM_DEFAULT_AKCERT_SIZE` when no cert is supplied.
    pub ak_cert_index_size: Option<u16>,
    /// Contents to write into the AK cert NV index. When absent, the index is
    /// created but left uninitialized.
    pub ak_cert: Option<Vec<u8>>,
    /// Password authorization for a platform-created AK cert index.
    pub auth_value: u64,
    /// Create the small-vTPM mitigation marker index.
    pub mitigation_marker: bool,
}

/// Runs the provisioning commands and returns the resulting NVRAM blob.
pub fn provision(params: &ProvisionParams) -> anyhow::Result<Vec<u8>> {
    let (mut helper, nvram) = engine::create(params.version, params.nvram_size, None)?;

    if params.ak {
        helper
            .create_ak_pub(false)
            .context("failed to create the AK")?;
        tracing::info!("created AK");
    }

    if params.ak_cert_index != AkCertIndexKind::None {
        let max_nv_index_size = helper.tpm_engine.max_nv_index_size();

        let requested_size = match (params.ak_cert_index_size, &params.ak_cert) {
            (Some(size), _) => size as usize,
            (None, Some(cert)) => cert.len(),
            (None, None) => TPM_DEFAULT_AKCERT_SIZE,
        };
        // A zero-size index is accepted by the TPM but is useless, so reject it
        // here rather than exporting a blob that cannot hold an AK cert.
        anyhow::ensure!(
            (1..=max_nv_index_size as usize).contains(&requested_size),
            "AK cert NV index size must be between 1 and {max_nv_index_size} bytes, \
             got {requested_size}"
        );
        let size = requested_size as u16;

        if let Some(cert) = &params.ak_cert {
            anyhow::ensure!(
                cert.len() <= size as usize,
                "AK cert ({} bytes) does not fit in a {size}-byte NV index",
                cert.len(),
            );
        }

        match params.ak_cert_index {
            AkCertIndexKind::Owner => {
                helper
                    .nv_define_space(TPM20_RH_OWNER, 0, TPM_NV_INDEX_AIK_CERT, size)
                    .context("failed to define the owner AK cert NV index")?;

                if let Some(cert) = &params.ak_cert {
                    // `write_to_nv_index` zero-pads to the index size, but only
                    // works on platform-created indices, so pad by hand here.
                    let mut padded = cert.clone();
                    padded.resize(size as usize, 0);
                    helper
                        .nv_write(TPM20_RH_OWNER, None, TPM_NV_INDEX_AIK_CERT, &padded)
                        .context("failed to write the owner AK cert NV index")?;
                }
            }
            AkCertIndexKind::Platform => {
                helper
                    .nv_define_space(
                        TPM20_RH_PLATFORM,
                        params.auth_value,
                        TPM_NV_INDEX_AIK_CERT,
                        size,
                    )
                    .context("failed to define the platform AK cert NV index")?;

                if let Some(cert) = &params.ak_cert {
                    helper
                        .write_to_nv_index(params.auth_value, TPM_NV_INDEX_AIK_CERT, cert)
                        .context("failed to write the platform AK cert NV index")?;
                }
            }
            // Excluded by the enclosing `if`.
            AkCertIndexKind::None => {}
        }

        tracing::info!(size, kind = ?params.ak_cert_index, "created AK cert NV index");
    }

    if params.mitigation_marker {
        helper
            .nv_define_space(
                TPM20_RH_PLATFORM,
                params.auth_value,
                TPM_NV_INDEX_MITIGATED,
                1,
            )
            .context("failed to define the mitigation marker NV index")?;
        tracing::info!("created mitigation marker NV index");
    }

    if params.srk {
        let template = tpm_lib::rsa_srk_template().context("failed to create the SRK template")?;
        let srk = helper
            .create_primary(TPM20_RH_OWNER, template)
            .context("failed to create the SRK")?;
        helper
            .evict_control(TPM20_RH_OWNER, srk.object_handle, TPM_RSA_SRK_HANDLE)
            .context("failed to persist the SRK")?;
        helper
            .flush_context(srk.object_handle)
            .context("failed to flush the SRK context")?;
        tracing::info!("created SRK");
    }

    let blob = nvram.get();
    anyhow::ensure!(
        blob.len() == params.nvram_size,
        "TPM committed a {}-byte NVRAM blob, expected {}",
        blob.len(),
        params.nvram_size
    );

    // Round-trip the blob so a broken export fails here rather than at VM boot.
    helper
        .tpm_engine
        .reset(Some(&blob))
        .context("failed to reload the exported blob")?;
    helper
        .initialize_tpm_engine()
        .context("failed to start up the TPM after reloading the exported blob")?;

    if params.ak {
        anyhow::ensure!(
            helper.find_object(TPM_AZURE_AIK_HANDLE)?.is_some(),
            "AK is missing from the exported blob"
        );
    }
    if params.srk {
        anyhow::ensure!(
            helper.find_object(TPM_RSA_SRK_HANDLE)?.is_some(),
            "SRK is missing from the exported blob"
        );
    }
    if params.ak_cert_index != AkCertIndexKind::None {
        anyhow::ensure!(
            helper.find_nv_index(TPM_NV_INDEX_AIK_CERT)?.is_some(),
            "AK cert NV index is missing from the exported blob"
        );
    }

    Ok(blob)
}
