// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::sync::Arc;
use test_with_tracing::test;

#[derive(Default)]
struct FlushState {
    flush_count: usize,
    fail_flush_at: Option<usize>,
    write_count: usize,
}

/// Delegate RAM I/O, injecting failures only when explicitly armed by a test.
#[derive(Inspect)]
struct FlushFaultDisk {
    disk: Disk,
    #[inspect(skip)]
    state: Arc<Mutex<FlushState>>,
}

impl DiskIo for FlushFaultDisk {
    fn disk_type(&self) -> &str {
        "attestation-flush-fault-test"
    }

    fn sector_count(&self) -> u64 {
        self.disk.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.disk.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.disk.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.disk.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.disk.is_read_only()
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.disk.unmap(sector, count, block_level_only).await
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.disk.unmap_behavior()
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.disk.optimal_unmap_sectors()
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.disk.wait_resize(sector_count).await
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.disk.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.state.lock().write_count += 1;
        self.disk.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        {
            let mut state = self.state.lock();
            state.flush_count += 1;
            if state.fail_flush_at == Some(state.flush_count) {
                return Err(DiskError::Io(std::io::Error::other(
                    "injected enrollment flush failure",
                )));
            }
        }
        self.disk.sync_cache().await
    }
}

const ACTIVE_DEK: [u8; AES_GCM_KEY_LENGTH] = [0x33; AES_GCM_KEY_LENGTH];
const PAYLOAD: &[u8] = b"encrypted state survives enrollment flush failure";

async fn provisioned_vmgs() -> (Vmgs, Disk, Arc<Mutex<FlushState>>) {
    let state = Arc::new(Mutex::new(FlushState::default()));
    let disk = Disk::new(FlushFaultDisk {
        disk: new_test_file(),
        state: state.clone(),
    })
    .unwrap();
    let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
    let tee = BootReportTee::new();
    let mut config = new_attestation_vm_config();
    config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
    let hardware_keys = HardwareDerivedKeys::derive_key(
        tee.supports_get_derived_key().unwrap(),
        &config,
        KeyDerivationPolicy {
            svn: tee_call::KeyDerivationSvn::Snp {
                tcb_version: tee.inner.tcb_version,
            },
            mix_measurement: true,
        },
    )
    .unwrap();
    let protector = hardware_key_sealing::seal_key(&hardware_keys, &ACTIVE_DEK).unwrap();
    unlock_vmgs_data_store(
        &mut vmgs,
        false,
        &mut KeyProtector::new_zeroed(),
        &mut new_key_protector_by_id(None, None, false),
        Some(protector),
        Some(Keys {
            ingress: [0; AES_GCM_KEY_LENGTH],
            decrypt_egress: None,
            encrypt_egress: ACTIVE_DEK,
        }),
        KeyProtectorSettings {
            should_write_kp: true,
            use_gsp_by_id: false,
            use_hardware_unlock: false,
            decrypt_gsp_type: GspType::None,
            encrypt_gsp_type: GspType::None,
        },
        Guid::new_random(),
    )
    .await
    .unwrap();
    vmgs.write_file_encrypted(FileId::ATTEST, PAYLOAD)
        .await
        .unwrap();
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);

    // Do not depend on the number of flushes used by format/unlock/writes.
    // Arm only after provisioning and protector persistence have succeeded.
    *state.lock() = FlushState {
        fail_flush_at: Some(1),
        ..Default::default()
    };
    (vmgs, disk, state)
}

#[async_test]
async fn final_enrollment_flush_failure_preserves_store_and_can_retry() {
    let (mut vmgs, disk, state) = provisioned_vmgs().await;
    let protector = vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap();

    assert!(!finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK)).await);
    assert_eq!(state.lock().flush_count, 1);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);
    assert_eq!(vmgs.read_file(FileId::ATTEST).await.unwrap(), PAYLOAD);
    assert_eq!(
        vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap(),
        protector
    );

    // The fault is one-shot. The same candidate succeeds without reprovisioning
    // or changing the active DEK, and actually attempts another disk flush.
    assert!(finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK)).await);
    assert_eq!(state.lock().flush_count, 2);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);

    drop(vmgs);
    let mut reopened = Vmgs::open(disk, None).await.unwrap();
    assert!(reopened.encrypted());
    reopened
        .unlock_with_encryption_key(&ACTIVE_DEK)
        .await
        .unwrap();
    assert_eq!(reopened.active_encryption_key().unwrap(), &ACTIVE_DEK);
    assert_eq!(reopened.read_file(FileId::ATTEST).await.unwrap(), PAYLOAD);
    assert_eq!(
        reopened.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap(),
        protector
    );
}

#[async_test]
async fn absent_or_mismatched_sealed_key_does_not_flush() {
    let (mut vmgs, _disk, state) = provisioned_vmgs().await;
    assert!(!finalize_hardware_sealing(&mut vmgs, None).await);
    for index in [0, AES_GCM_KEY_LENGTH - 1] {
        let mut mismatched_key = ACTIVE_DEK;
        mismatched_key[index] ^= 1;
        assert!(!finalize_hardware_sealing(&mut vmgs, Some(mismatched_key)).await);
    }
    assert_eq!(state.lock().flush_count, 0);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);

    // Skipped candidates must not consume the armed fault.
    assert!(!finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK)).await);
    assert_eq!(state.lock().flush_count, 1);
    assert!(finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK)).await);
    assert_eq!(state.lock().flush_count, 2);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);
}
