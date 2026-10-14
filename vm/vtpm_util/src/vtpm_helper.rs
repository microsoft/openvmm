// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper for creating and managing a TPM engine with in-memory NV state.

use ms_tcg_tpm_sys::MsTpm185Platform;
use ms_tpm_20_ref::MsTpm20RefPlatform;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;
use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineError;

use crate::TpmVersion;

pub enum CvmTpmEngine {
    V138(MsTpm20RefPlatform),
    V185(MsTpm185Platform),
}

impl CvmTpmEngine {
    pub fn reset(&mut self, nvram: Option<&[u8]>) -> anyhow::Result<()> {
        match self {
            Self::V138(engine) => engine.reset(nvram).map_err(Into::into),
            Self::V185(engine) => engine.reset(nvram).map_err(Into::into),
        }
    }
}

impl TpmEngine for CvmTpmEngine {
    fn execute_command(
        &mut self,
        command: &mut [u8],
        response: &mut [u8],
    ) -> Result<(), TpmEngineError> {
        match self {
            Self::V138(engine) => MsTpm20RefPlatform::execute_command(engine, command, response)
                .map(|_| ())
                .map_err(TpmEngineError::from_error),
            Self::V185(engine) => MsTpm185Platform::execute_command(engine, command, response)
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

pub type TpmEngineHelper = tpm_lib::TpmEngineHelper<CvmTpmEngine>;
struct TestPlatformCallbacks {
    blob: Vec<u8>,
    time: Instant,
    // Add shared access to the blob
    shared_blob: Arc<Mutex<Vec<u8>>>,
}

impl TestPlatformCallbacks {
    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let shared_blob = Arc::new(Mutex::new(Vec::new()));
        let callbacks = TestPlatformCallbacks {
            blob: vec![],
            time: Instant::now(),
            shared_blob: shared_blob.clone(),
        };
        (callbacks, shared_blob)
    }
}

impl ms_tpm_20_ref::PlatformCallbacks for TestPlatformCallbacks {
    fn commit_nv_state(&mut self, state: &[u8]) -> ms_tpm_20_ref::DynResult<()> {
        tracing::trace!("committing nv state with len {}", state.len());
        self.blob = state.to_vec();
        // Also update the shared blob
        *self.shared_blob.lock() = state.to_vec();

        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> ms_tpm_20_ref::DynResult<usize> {
        getrandom::fill(buf)?;

        Ok(buf.len())
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        &[]
    }
}

impl ms_tcg_tpm_sys::PlatformCallbacks for TestPlatformCallbacks {
    fn commit_nv_state(&mut self, state: &[u8]) -> ms_tcg_tpm_sys::DynResult<()> {
        tracing::trace!("committing nv state with len {}", state.len());
        self.blob = state.to_vec();
        *self.shared_blob.lock() = state.to_vec();

        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> ms_tcg_tpm_sys::DynResult<usize> {
        getrandom::fill(buf)?;

        Ok(buf.len())
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        &[]
    }
}

/// Create a new TPM engine with blank state and return its NV state blob.
pub fn create_tpm_engine_helper(
    version: TpmVersion,
) -> anyhow::Result<(TpmEngineHelper, Arc<Mutex<Vec<u8>>>)> {
    let (callbacks, nv_blob_accessor) = TestPlatformCallbacks::new();

    let tpm_engine = match version {
        TpmVersion::V138 => MsTpm20RefPlatform::initialize(
            Box::new(callbacks),
            ms_tpm_20_ref::InitKind::ColdInitWithSize(version.nvram_size()),
        )
        .map(CvmTpmEngine::V138)
        .map_err(anyhow::Error::from),
        TpmVersion::V185 => MsTpm185Platform::initialize(
            Box::new(callbacks),
            ms_tcg_tpm_sys::InitKind::ColdInitWithSize(version.nvram_size()),
        )
        .map(CvmTpmEngine::V185)
        .map_err(anyhow::Error::from),
    }?;

    let tpm_helper = TpmEngineHelper {
        tpm_engine,
        reply_buffer: [0u8; 4096],
    };

    Ok((tpm_helper, nv_blob_accessor))
}
