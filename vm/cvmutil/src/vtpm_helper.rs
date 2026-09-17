// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use ms_tpm_20_ref::DynResult;
use ms_tpm_20_ref::MsTpm20RefPlatform;
use std::sync::{Arc, Mutex};
use std::time::Instant;
///! Helper to create and manage a TPM engine instance with in-memory NV state for testing.
use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineError;

pub struct CvmTpmEngine(MsTpm20RefPlatform);

impl std::ops::Deref for CvmTpmEngine {
    type Target = MsTpm20RefPlatform;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for CvmTpmEngine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl TpmEngine for CvmTpmEngine {
    fn execute_command(
        &mut self,
        command: &mut [u8],
        response: &mut [u8],
    ) -> Result<(), TpmEngineError> {
        MsTpm20RefPlatform::execute_command(&mut self.0, command, response)
            .map(|_| ())
            .map_err(TpmEngineError::from_error)
    }

    fn max_nv_index_size(&self) -> u16 {
        tpm_lib::TPM_V138_MAX_NV_INDEX_SIZE
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
    fn commit_nv_state(&mut self, state: &[u8]) -> DynResult<()> {
        tracing::trace!("committing nv state with len {}", state.len());
        self.blob = state.to_vec();
        // Also update the shared blob
        *self.shared_blob.lock().unwrap() = state.to_vec();

        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> DynResult<usize> {
        getrandom::fill(buf).expect("rng failure");

        Ok(buf.len())
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        // Return a deterministic value for Ubuntu CVM compatibility
        // Ubuntu expects an empty unique value for reproducible key generation
        &[]
    }
}

/// Create a new TPM engine with blank state and return the helper and NV state blob.
pub fn create_tpm_engine_helper() -> (TpmEngineHelper, Arc<Mutex<Vec<u8>>>) {
    let (callbacks, nv_blob_accessor) = TestPlatformCallbacks::new();

    let result =
        MsTpm20RefPlatform::initialize(Box::new(callbacks), ms_tpm_20_ref::InitKind::ColdInit);
    assert!(result.is_ok());

    let tpm_engine: MsTpm20RefPlatform = result.unwrap();

    let tpm_helper = TpmEngineHelper {
        tpm_engine: CvmTpmEngine(tpm_engine),
        reply_buffer: [0u8; 4096],
    };

    (tpm_helper, nv_blob_accessor)
}
