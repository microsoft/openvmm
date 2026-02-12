// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

///! Helper to create and manage a TPM engine instance with in-memory NV state for testing.
use tpm::tpm_helper::{self, TpmEngineHelper};
use std::time::Instant;
use std::sync::{Arc, Mutex};
use ms_tpm_20_ref::MsTpm20RefPlatform;
use ms_tpm_20_ref::DynResult;
use tpm::tpm20proto::protocol::Tpm2bBuffer;
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
        tpm_engine,
        reply_buffer: [0u8; 4096],
    };

    (tpm_helper, nv_blob_accessor)
}
