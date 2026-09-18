// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The TPM reference implementation backends, wrapped behind [`TpmEngine`].

use anyhow::Context as _;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;
use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineError;
use tpm_lib::TpmEngineHelper;
use tpm_resources::TpmVersion;

/// Handle to the NVRAM blob captured from the library's commit callback.
#[derive(Clone, Default)]
pub(crate) struct NvramBlob(Arc<Mutex<Vec<u8>>>);

impl NvramBlob {
    /// Returns a copy of the most recently committed NVRAM state.
    pub(crate) fn get(&self) -> Vec<u8> {
        self.0.lock().clone()
    }
}

struct Callbacks {
    nvram: NvramBlob,
    time: Instant,
    /// Must match the value used by `tpm_device` for the same library, since it
    /// seeds the VENDOR_PERMANENT authorization value.
    unique_value: &'static [u8],
}

macro_rules! impl_callbacks {
    ($lib:ident) => {
        impl $lib::PlatformCallbacks for Callbacks {
            fn commit_nv_state(&mut self, state: &[u8]) -> $lib::DynResult<()> {
                *self.nvram.0.lock() = state.to_vec();
                Ok(())
            }

            fn get_crypt_random(&mut self, buf: &mut [u8]) -> $lib::DynResult<usize> {
                getrandom::fill(buf)?;
                Ok(buf.len())
            }

            fn monotonic_timer(&mut self) -> std::time::Duration {
                self.time.elapsed()
            }

            fn get_unique_value(&self) -> &'static [u8] {
                self.unique_value
            }
        }
    };
}

impl_callbacks!(ms_tpm_20_ref);
impl_callbacks!(ms_tcg_tpm_sys);

/// Wrapper around the TPM reference implementations.
pub(crate) enum TpmRefLib {
    /// The TPM 1.38 reference implementation.
    V138(ms_tpm_20_ref::MsTpm20RefPlatform),
    /// The TPM 1.85 reference implementation.
    V185(ms_tcg_tpm_sys::MsTpm185Platform),
}

impl TpmEngine for TpmRefLib {
    fn execute_command(
        &mut self,
        command: &mut [u8],
        response: &mut [u8],
    ) -> Result<(), TpmEngineError> {
        match self {
            Self::V138(inner) => inner
                .execute_command(command, response)
                .map(|_| ())
                .map_err(TpmEngineError::from_error),
            Self::V185(inner) => inner
                .execute_command(command, response)
                .map(|_| ())
                .map_err(TpmEngineError::from_error),
        }
    }

    fn max_nv_index_size(&self) -> u16 {
        match self {
            Self::V138(_) => tpm_lib::TPM_V138_MAX_NV_INDEX_SIZE,
            Self::V185(_) => tpm_lib::TPM_V185_MAX_NV_INDEX_SIZE,
        }
    }
}

impl TpmRefLib {
    pub(crate) fn reset(&mut self, nvram: Option<&[u8]>) -> anyhow::Result<()> {
        match self {
            Self::V138(inner) => inner.reset(nvram)?,
            Self::V185(inner) => inner.reset(nvram)?,
        }
        Ok(())
    }
}

/// Returns the NVRAM size the reference implementation was compiled for.
pub(crate) fn default_nvram_size(version: TpmVersion) -> usize {
    match version {
        TpmVersion::V138 => ms_tpm_20_ref::NV_MEMORY_SIZE,
        TpmVersion::V185 => ms_tcg_tpm_sys::NV_MEMORY_SIZE,
    }
}

/// Creates a TPM helper backed by `version`, optionally seeded with an existing
/// NVRAM blob.
///
/// The reference implementations are process-global singletons, so this may only
/// be called once per version per process.
pub(crate) fn create(
    version: TpmVersion,
    nvram_size: usize,
    existing_blob: Option<&[u8]>,
) -> anyhow::Result<(TpmEngineHelper<TpmRefLib>, NvramBlob)> {
    let nvram = NvramBlob::default();
    let callbacks = Callbacks {
        nvram: nvram.clone(),
        time: Instant::now(),
        unique_value: match version {
            TpmVersion::V138 => b"hvlite vtpm",
            TpmVersion::V185 => b"openvmm vtpm",
        },
    };

    let mut engine = match version {
        TpmVersion::V138 => ms_tpm_20_ref::MsTpm20RefPlatform::initialize(
            Box::new(callbacks),
            ms_tpm_20_ref::InitKind::ColdInitWithSize(nvram_size),
        )
        .map(TpmRefLib::V138)
        .context("failed to initialize the TPM 1.38 library")?,
        TpmVersion::V185 => ms_tcg_tpm_sys::MsTpm185Platform::initialize(
            Box::new(callbacks),
            ms_tcg_tpm_sys::InitKind::ColdInitWithSize(nvram_size),
        )
        .map(TpmRefLib::V185)
        .context("failed to initialize the TPM 1.85 library")?,
    };

    if let Some(blob) = existing_blob {
        engine
            .reset(Some(blob))
            .context("failed to load the existing NVRAM blob")?;
    }

    let mut helper = TpmEngineHelper::new(engine);
    helper
        .initialize_tpm_engine()
        .context("failed to start up the TPM")?;

    Ok((helper, nvram))
}
