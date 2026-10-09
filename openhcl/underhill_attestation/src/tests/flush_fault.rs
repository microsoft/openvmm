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
    failed_flush_count: usize,
    write_count: usize,
    writes_at_flush: Vec<usize>,
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
            let write_count = state.write_count;
            state.writes_at_flush.push(write_count);
            if state.fail_flush_at == Some(state.flush_count) {
                state.failed_flush_count += 1;
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

async fn new_flush_fault_vmgs() -> (Vmgs, Disk, Arc<Mutex<FlushState>>) {
    let state = Arc::new(Mutex::new(FlushState::default()));
    let disk = Disk::new(FlushFaultDisk {
        disk: new_test_file(),
        state: state.clone(),
    })
    .unwrap();
    let vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
    *state.lock() = FlushState::default();
    (vmgs, disk, state)
}

async fn provisioned_vmgs() -> (Vmgs, Disk, Arc<Mutex<FlushState>>) {
    let (mut vmgs, disk, state) = new_flush_fault_vmgs().await;
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

    assert!(
        !finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), false)
            .await
            .unwrap()
    );
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
    assert!(
        finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), false)
            .await
            .unwrap()
    );
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
    assert!(
        !finalize_hardware_sealing(&mut vmgs, None, false)
            .await
            .unwrap()
    );
    for index in [0, AES_GCM_KEY_LENGTH - 1] {
        let mut mismatched_key = ACTIVE_DEK;
        mismatched_key[index] ^= 1;
        assert!(
            !finalize_hardware_sealing(&mut vmgs, Some(mismatched_key), false)
                .await
                .unwrap()
        );
    }
    assert_eq!(state.lock().flush_count, 0);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);

    // Skipped candidates must not consume the armed fault.
    assert!(
        !finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), false)
            .await
            .unwrap()
    );
    assert_eq!(state.lock().flush_count, 1);
    assert!(
        finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), false)
            .await
            .unwrap()
    );
    assert_eq!(state.lock().flush_count, 2);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);
}

#[async_test]
async fn required_absent_or_mismatched_sealed_key_fails_without_io() {
    let (mut vmgs, _disk, state) = provisioned_vmgs().await;
    assert!(matches!(
        finalize_hardware_sealing(&mut vmgs, None, true).await,
        Err(FinalizeHardwareSealingError::MissingSealedKey)
    ));
    for index in [0, AES_GCM_KEY_LENGTH - 1] {
        let mut mismatched_key = ACTIVE_DEK;
        mismatched_key[index] ^= 1;
        assert!(matches!(
            finalize_hardware_sealing(&mut vmgs, Some(mismatched_key), true).await,
            Err(FinalizeHardwareSealingError::ActiveKeyMismatch)
        ));
    }
    assert_eq!(state.lock().flush_count, 0);
    assert_eq!(state.lock().write_count, 0);
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);

    // Precondition failures must not consume the armed final-flush fault.
    assert!(matches!(
        finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), true).await,
        Err(FinalizeHardwareSealingError::Flush(
            ::vmgs::Error::FlushDisk(_)
        ))
    ));
    assert_eq!(state.lock().flush_count, 1);
    assert_eq!(state.lock().failed_flush_count, 1);
    assert_eq!(state.lock().write_count, 0);
}

#[async_test]
async fn locked_and_plaintext_stores_cannot_finalize_hardware_sealing() {
    for encrypted in [false, true] {
        let (mut vmgs, _disk, state) = if encrypted {
            let (vmgs, disk, state) = provisioned_vmgs().await;
            drop(vmgs);
            let reopened = Vmgs::open(disk.clone(), None).await.unwrap();
            (reopened, disk, state)
        } else {
            new_flush_fault_vmgs().await
        };
        *state.lock() = FlushState {
            fail_flush_at: Some(1),
            ..Default::default()
        };
        assert_eq!(vmgs.encrypted(), encrypted);
        assert!(
            !finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), false)
                .await
                .unwrap()
        );
        let error = finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), true)
            .await
            .unwrap_err();
        if encrypted {
            assert!(matches!(
                error,
                FinalizeHardwareSealingError::ActiveKey(::vmgs::Error::NeedsUnlock)
            ));
        } else {
            assert!(matches!(
                error,
                FinalizeHardwareSealingError::ActiveKey(::vmgs::Error::NotEncrypted)
            ));
        }
        assert_eq!(state.lock().flush_count, 0);
        assert_eq!(state.lock().failed_flush_count, 0);
        assert_eq!(state.lock().write_count, 0);
        assert_eq!(vmgs.encrypted(), encrypted);
        assert!(vmgs.active_encryption_key().is_err());
    }
}

#[async_test]
async fn required_final_flush_failure_preserves_store_and_can_retry_without_rekey() {
    let (mut vmgs, disk, state) = provisioned_vmgs().await;
    let protector = vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap();

    assert!(matches!(
        finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), true).await,
        Err(FinalizeHardwareSealingError::Flush(
            ::vmgs::Error::FlushDisk(_)
        ))
    ));
    assert_eq!(state.lock().flush_count, 1);
    assert_eq!(state.lock().failed_flush_count, 1);
    assert_eq!(state.lock().write_count, 0);
    assert!(vmgs.encrypted());
    assert_eq!(vmgs.active_encryption_key().unwrap(), &ACTIVE_DEK);
    assert_eq!(vmgs.read_file(FileId::ATTEST).await.unwrap(), PAYLOAD);
    assert_eq!(
        vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap(),
        protector
    );

    // Retry just finalization, not unlock, protector persistence, or key rotation.
    assert!(
        finalize_hardware_sealing(&mut vmgs, Some(ACTIVE_DEK), true)
            .await
            .unwrap()
    );
    assert_eq!(state.lock().flush_count, 2);
    assert_eq!(state.lock().failed_flush_count, 1);
    assert_eq!(state.lock().write_count, 0);
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
    assert_eq!(state.lock().write_count, 0);
}

#[derive(Clone, Copy, Debug)]
enum InitScenario {
    RequiredFirstBoot,
    RequiredSubsequentBoot,
    OptionalFirstBoot,
    OptionalSubsequentBoot,
    SkrFailureHardwareFallback,
}

impl InitScenario {
    fn required_stateless(self) -> bool {
        matches!(self, Self::RequiredFirstBoot | Self::RequiredSubsequentBoot)
    }

    fn subsequent_boot(self) -> bool {
        matches!(
            self,
            Self::RequiredSubsequentBoot
                | Self::OptionalSubsequentBoot
                | Self::SkrFailureHardwareFallback
        )
    }
}

/// Run the real initialization path on a fresh fixture. A successful reference
/// run measures its I/O, then a second run fails only its last flush. No ordinal
/// from format, provisioning, SKR, rotation, or protector writes is hard-coded.
async fn run_init_scenario(
    driver: &DefaultDriver,
    ldriver: LocalDriver,
    scenario: InitScenario,
    reference_writes_at_flush: Option<&[usize]>,
) -> Vec<usize> {
    let required_stateless = scenario.required_stateless();
    let hardware_fallback = matches!(scenario, InitScenario::SkrFailureHardwareFallback);
    let plan = if hardware_fallback {
        let mut plan = IgvmAgentTestPlan::default();
        plan.insert(
            IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            VecDeque::from([
                IgvmAgentAction::RespondSuccess,
                // Unlike RespondFailure, this produces a retryable SKR error.
                // Finalization failure must override that retry indication.
                IgvmAgentAction::NoResponse,
            ]),
        );
        Some(plan)
    } else {
        None
    };
    let get_pair = new_test_get(driver.clone(), !required_stateless, plan).await;
    let (mut vmgs, disk, state) = new_flush_fault_vmgs().await;
    let bios_guid = Guid::new_random();
    let mut config = new_attestation_vm_config();
    config.hardware_sealing_policy = HardwareSealingPolicy::Hash;
    config.tpm_persisted = !required_stateless;
    let policy = if required_stateless {
        GuestStateEncryptionPolicy::HardwareSealing
    } else {
        GuestStateEncryptionPolicy::Auto
    };
    let previous_dek = if scenario.subsequent_boot() {
        let tee = BootReportTee::new();
        let provisioned = initialize_platform_security(
            &get_pair.client,
            bios_guid,
            &config,
            &mut vmgs,
            Some(&tee),
            required_stateless,
            ldriver.clone(),
            policy,
            true,
        )
        .await
        .unwrap();
        assert!(provisioned.runtime_tcb_floor.is_some());
        assert_eq!(tee.report_data.lock().len(), 1);
        let dek = *vmgs.active_encryption_key().unwrap();
        // ATTEST is the security profile, read before VMGS is unlocked. Keep
        // the encrypted sentinel in an entry initialization does not consume.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, PAYLOAD)
            .await
            .unwrap();
        vmgs.flush().await.unwrap();
        drop(vmgs);
        vmgs = Vmgs::open(disk.clone(), None).await.unwrap();
        assert!(matches!(
            vmgs.active_encryption_key(),
            Err(::vmgs::Error::NeedsUnlock)
        ));
        Some(dek)
    } else {
        assert!(!vmgs.encrypted());
        None
    };

    // Setup is complete, including reopening a locked VMGS on subsequent boots.
    // Only the initialization under test contributes to these counters.
    *state.lock() = FlushState {
        fail_flush_at: reference_writes_at_flush.map(<[usize]>::len),
        ..Default::default()
    };
    let tee = BootReportTee::new();
    let result = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &config,
        &mut vmgs,
        Some(&tee),
        required_stateless,
        ldriver,
        policy,
        true,
    )
    .await;

    if reference_writes_at_flush.is_some() && (required_stateless || hardware_fallback) {
        // An error returns no PlatformAttestationData and therefore no floor.
        // In the fallback case suppress_attestation=false, so the required
        // finalization comes from use_hardware_unlock, not the stateless flag.
        assert!(matches!(
            result,
            Err(Error(AttestationErrorInner::FinalizeHardwareSealing(
                FinalizeHardwareSealingError::Flush(::vmgs::Error::FlushDisk(_))
            )))
        ));
    } else {
        let result = result.unwrap();
        assert_eq!(
            result.runtime_tcb_floor.is_some(),
            reference_writes_at_flush.is_none()
        );
        assert!(!result.host_attestation_settings.refresh_tpm_seeds);
    }
    // One report means one unlock attempt, with no report-only fallback or
    // hidden retry. For SKR it must retain its nonzero claims hash.
    {
        let reports = tee.report_data.lock();
        assert_eq!(reports.len(), 1, "unexpected retry for {scenario:?}");
        assert_eq!(reports[0] == [0; REPORT_DATA_SIZE], required_stateless);
    }
    assert!(*tee.derivation_calls.lock() > 0);
    let (writes_at_flush, write_count) = {
        let state = state.lock();
        assert_eq!(state.flush_count, state.writes_at_flush.len());
        assert_eq!(
            state.failed_flush_count,
            usize::from(reference_writes_at_flush.is_some())
        );
        if let Some(reference) = reference_writes_at_flush {
            assert_eq!(state.writes_at_flush.as_slice(), reference);
        }
        // VMGS flushes *before* committing each header. The last flush must
        // include the final header write, with no writes after it. This guards
        // against accidentally injecting into an earlier persistence flush.
        assert!(state.writes_at_flush.len() >= 2);
        assert_eq!(state.writes_at_flush.last(), Some(&state.write_count));
        assert!(state.writes_at_flush[state.writes_at_flush.len() - 2] < state.write_count);
        (state.writes_at_flush.clone(), state.write_count)
    };

    assert!(vmgs.encrypted());
    let active_dek = *vmgs.active_encryption_key().unwrap();
    if hardware_fallback {
        // Recovery re-seals the existing DEK; only normal SKR/stateless boots
        // rotate it. This also proves that the hardware fallback was taken.
        assert_eq!(previous_dek, Some(active_dek));
    } else {
        assert_ne!(previous_dek, Some(active_dek));
    }
    if previous_dek.is_some() {
        assert_eq!(vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap(), PAYLOAD);
    }
    let protector_bytes = vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap();
    let protector = vmgs::read_hardware_key_protector(&mut vmgs).await.unwrap();
    let keys = HardwareDerivedKeys::derive_key(
        tee.supports_get_derived_key().unwrap(),
        &config,
        protector.key_derivation_policy().unwrap(),
    )
    .unwrap();
    assert_eq!(protector.unseal_key(&keys).unwrap(), active_dek);

    if reference_writes_at_flush.is_some() {
        // The failed boot already selected and persisted this key/protector.
        // A helper-only retry must succeed without rerunning SKR or full rekey.
        assert!(
            finalize_hardware_sealing(
                &mut vmgs,
                Some(active_dek),
                required_stateless || hardware_fallback,
            )
            .await
            .unwrap()
        );
        assert_eq!(state.lock().flush_count, writes_at_flush.len() + 1);
        assert_eq!(state.lock().failed_flush_count, 1);
        assert_eq!(vmgs.active_encryption_key().unwrap(), &active_dek);
        assert_eq!(
            vmgs.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap(),
            protector_bytes
        );
    }
    assert_eq!(state.lock().write_count, write_count);

    drop(vmgs);
    let mut reopened = Vmgs::open(disk, None).await.unwrap();
    assert!(reopened.encrypted());
    reopened
        .unlock_with_encryption_key(&active_dek)
        .await
        .unwrap();
    assert_eq!(reopened.active_encryption_key().unwrap(), &active_dek);
    assert_eq!(
        reopened.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap(),
        protector_bytes
    );
    if previous_dek.is_some() {
        assert_eq!(
            reopened.read_file(FileId::BIOS_NVRAM).await.unwrap(),
            PAYLOAD
        );
    }
    assert_eq!(state.lock().write_count, write_count);
    assert_eq!(tee.report_data.lock().len(), 1);
    writes_at_flush
}

fn check_init_final_flush_failure(scenario: InitScenario) {
    // Keep both executors alive: a retry regression should fail assertions,
    // rather than hang because its LocalDriver's timer has no running pool.
    let (get_thread, driver) = pal_async::DefaultPool::spawn_on_thread("final-flush-get");
    pal_async::local::block_with_io(async |ldriver| {
        let reference = run_init_scenario(&driver, ldriver.clone(), scenario, None).await;
        run_init_scenario(&driver, ldriver, scenario, Some(&reference)).await;
    });
    drop(driver);
    get_thread.join().unwrap();
}

#[test]
fn init_sec_required_stateless_first_boot_final_flush_failure_is_fatal() {
    check_init_final_flush_failure(InitScenario::RequiredFirstBoot);
}

#[test]
fn init_sec_required_stateless_subsequent_boot_final_flush_failure_is_fatal() {
    check_init_final_flush_failure(InitScenario::RequiredSubsequentBoot);
}

#[test]
fn init_sec_optional_backup_final_flush_failure_succeeds_without_floor() {
    for scenario in [
        InitScenario::OptionalFirstBoot,
        InitScenario::OptionalSubsequentBoot,
    ] {
        check_init_final_flush_failure(scenario);
    }
}

#[test]
fn init_sec_retryable_skr_failure_hardware_fallback_final_flush_failure_is_fatal() {
    check_init_final_flush_failure(InitScenario::SkrFailureHardwareFallback);
}
