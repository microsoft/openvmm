// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use mesh::rpc::RpcSend;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV3;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Condvar;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use std::pin::pin;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::task::Wake;
use std::task::Waker;
use tee_call::GetAttestationReportResult;
use tee_call::HW_DERIVED_KEY_LENGTH;
use tee_call::KeyDerivationPolicy;
use tee_call::KeyDerivationSvn;
use tee_call::REPORT_DATA_SIZE;
use tee_call::TeeCallGetDerivedKey;
use tee_call::TeeType;
use test_with_tracing::test;
use vmgs::Vmgs;
use vmgs_broker::spawn_vmgs_broker;
use vmgs_format::EncryptionAlgorithm;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const DEK: [u8; 32] = [0xab; 32];

fn protector_svn(protector: &[u8]) -> u64 {
    let protector = HardwareKeyProtectorV3::read_from_bytes(protector).unwrap();
    u64::from_le_bytes(protector.header.svn[..8].try_into().unwrap())
}

#[test]
fn successful_event_clears_pending_recovery_without_scheduling_more_work() {
    let now = Instant::from_nanos(1_000_000_000);
    for jitter in 0..=u8::MAX {
        let mut schedule = Schedule::new(now);
        assert!(!schedule.running);
        assert!(!schedule.force_reseal);
        assert_eq!(schedule.due(), now);
        schedule.failures = 5;
        schedule.force_reseal = true;
        schedule.completed(now, true, jitter);
        assert_eq!(schedule.failures, 0);
        assert!(!schedule.force_reseal);
        assert_eq!(schedule.not_before, now + MIN_RESEAL_INTERVAL);
        assert_eq!(schedule.deadline, now);
    }
}

#[test]
fn retries_back_off_exponentially_and_saturate() {
    let now = Instant::from_nanos(1_000_000_000);
    for jitter in [0, 1, 127, u8::MAX] {
        let mut schedule = Schedule::new(now);
        let mut completed_at = now;
        for (index, seconds) in [1, 2, 4, 8, 16, 32, 60, 60, 60].into_iter().enumerate() {
            schedule.completed(completed_at, false, jitter);
            let delay = (Duration::from_secs(seconds)
                + Duration::from_millis(u64::from(jitter) * 4))
            .min(MAX_RETRY_INTERVAL);
            assert_eq!(schedule.failures, index as u32 + 1);
            assert!(schedule.force_reseal);
            assert_eq!(schedule.not_before, completed_at + delay);
            assert_eq!(schedule.due(), schedule.not_before);
            completed_at = schedule.due();
        }
        schedule.failures = u32::MAX;
        schedule.completed(completed_at, false, jitter);
        assert_eq!(schedule.failures, u32::MAX);
        assert_eq!(schedule.due(), completed_at + MAX_RETRY_INTERVAL);

        schedule.completed(completed_at, true, jitter);
        assert_eq!(schedule.failures, 0);
        schedule.completed(completed_at, false, 0);
        assert_eq!(schedule.due(), completed_at + MIN_RESEAL_INTERVAL);
    }
}

#[test]
fn notifications_do_not_bypass_success_or_retry_not_before() {
    let now = Instant::from_nanos(1_000_000_000);
    for success in [false, true] {
        let mut schedule = Schedule::new(now);
        schedule.failures = 5;
        schedule.completed(now, success, u8::MAX);
        let not_before = schedule.not_before;
        for offset in [0, 1, 50, 999] {
            schedule.notified(now + Duration::from_millis(offset));
            assert!(schedule.force_reseal);
            assert_eq!(schedule.not_before, not_before);
            assert_eq!(schedule.due(), not_before);
        }
        let late = not_before + Duration::from_secs(1);
        schedule.notified(late);
        assert_eq!(schedule.due(), late);
    }
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn notification_latches_coalesces_and_registers_the_latest_waker() {
    let notification = MigrationNotification::default();
    let first = Arc::new(WakeCount::default());
    let second = Arc::new(WakeCount::default());
    let first_waker = Waker::from(first.clone());
    let second_waker = Waker::from(second.clone());
    let first_cx = Context::from_waker(&first_waker);
    let second_cx = Context::from_waker(&second_waker);

    // Notifications before registration are latched, not queued.
    notification.notify();
    notification.notify();
    assert!(notification.take(&first_cx));
    assert!(!notification.take(&first_cx));
    assert!(!notification.take(&second_cx));
    notification.notify();
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    assert!(notification.take(&second_cx));
    assert!(!notification.take(&second_cx));
    notification.notify();
    assert_eq!(second.0.load(Ordering::SeqCst), 2);
}

#[test]
fn completion_does_not_clear_a_notification_received_during_the_attempt() {
    let now = Instant::from_nanos(1_000_000_000);
    let cx = Context::from_waker(Waker::noop());
    for success in [false, true] {
        let notification = MigrationNotification::default();
        let mut schedule = Schedule::new(now);
        notification.notify();
        assert!(notification.take(&cx));
        schedule.notified(now);

        notification.notify();
        schedule.completed(now, success, 0);
        assert!(notification.take(&cx));
        schedule.notified(now);
        assert!(schedule.force_reseal);
        assert_eq!(schedule.due(), now + MIN_RESEAL_INTERVAL);
        assert!(!notification.take(&cx));
    }
}

// No Debug/Inspect: the mock hardware identity is secret material.
struct Hardware {
    identity: AtomicU8,
    tcb_version: AtomicU64,
    fail_report: AtomicBool,
    fail_derive: AtomicBool,
    reports: AtomicUsize,
    derivations: AtomicUsize,
    progress: AtomicWaker,
}

impl Hardware {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            identity: AtomicU8::new(0x42),
            tcb_version: AtomicU64::new(7),
            fail_report: AtomicBool::new(false),
            fail_derive: AtomicBool::new(false),
            reports: AtomicUsize::new(0),
            derivations: AtomicUsize::new(0),
            progress: AtomicWaker::new(),
        })
    }

    async fn reached(&self, reports: usize, derivations: usize) {
        poll_fn(|cx| {
            self.progress.register(cx.waker());
            if self.reports.load(Ordering::SeqCst) >= reports
                && self.derivations.load(Ordering::SeqCst) >= derivations
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

struct MockTee(Arc<Hardware>);

impl TeeCall for MockTee {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, tee_call::Error> {
        assert_eq!(report_data, &[0; REPORT_DATA_SIZE]);
        self.0.reports.fetch_add(1, Ordering::SeqCst);
        self.0.progress.wake();
        if self.0.fail_report.load(Ordering::SeqCst) {
            return Err(tee_call::Error::AllZeroKey);
        }
        let tcb_version = self.0.tcb_version.load(Ordering::SeqCst);
        // A valid v3 Milan/Genoa comparison domain, with the raw reported TCB
        // and the separately returned derivation SVN from the same snapshot.
        let mut report = vec![0; 1184];
        report[..4].copy_from_slice(&3u32.to_le_bytes());
        report[0x180..0x188].copy_from_slice(&tcb_version.to_le_bytes());
        report[0x188..0x18a].copy_from_slice(&[0x19, 0x01]);
        Ok(GetAttestationReportResult {
            report,
            key_derivation_svn: Some(KeyDerivationSvn::Snp { tcb_version }),
        })
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        Some(self)
    }

    fn tee_type(&self) -> TeeType {
        TeeType::Snp
    }
}

impl TeeCallGetDerivedKey for MockTee {
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], tee_call::Error> {
        let KeyDerivationSvn::Snp { tcb_version } = policy.svn else {
            panic!("expected SNP derivation policy");
        };
        assert!(policy.mix_measurement);
        self.0.derivations.fetch_add(1, Ordering::SeqCst);
        self.0.progress.wake();
        if self.0.fail_derive.load(Ordering::SeqCst) {
            return Err(tee_call::Error::AllZeroKey);
        }
        // Older requested SVNs remain derivable after an upgrade. Only the
        // resident floor, not this mock, enforces the report/header comparison.
        // Keep identity migration independent of SVN, but bind keys to both.
        let mut key = [self.0.identity.load(Ordering::SeqCst); HW_DERIVED_KEY_LENGTH];
        for (byte, svn_byte) in key.iter_mut().zip(tcb_version.to_le_bytes()) {
            *byte ^= svn_byte;
        }
        Ok(key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HardwareCall {
    Report,
    Derivation(usize),
}

#[derive(Default)]
struct HardwareGateState {
    calls: Vec<HardwareCall>,
    active: bool,
    reached: bool,
    released: bool,
}

// Unlike GatedDisk, this blocks a synchronous hardware call, not an async
// future. Only call-site metadata is recorded here, never key material.
struct HardwareGate {
    executor_thread: std::thread::ThreadId,
    block_at: HardwareCall,
    state: Mutex<HardwareGateState>,
    release: Condvar,
    progress: AtomicWaker,
}

impl HardwareGate {
    fn new(block_at: HardwareCall) -> Arc<Self> {
        Arc::new(Self {
            executor_thread: std::thread::current().id(),
            block_at,
            state: Mutex::new(HardwareGateState::default()),
            release: Condvar::new(),
            progress: AtomicWaker::new(),
        })
    }

    fn call<T>(&self, call: HardwareCall, hardware: impl FnOnce() -> T) -> T {
        // Fail BEFORE waiting if synchronous TEE work regresses onto the test's
        // single-thread executor: otherwise neither Stop nor release can run.
        assert_ne!(std::thread::current().id(), self.executor_thread);
        {
            let mut state = self.state.lock();
            assert!(!state.active, "parallel hardware calls: {call:?}");
            state.active = true;
            state.calls.push(call);
            if call == self.block_at && !state.released {
                state.reached = true;
                self.progress.wake();
                // A failure elsewhere in the test must not strand a pool
                // thread. This deadline is a fail-safe, not synchronization.
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !state.released {
                    let timed_out = self.release.wait_until(&mut state, deadline).timed_out();
                    assert!(!timed_out || state.released, "hardware gate not released");
                }
            }
        }
        let result = hardware();
        self.state.lock().active = false;
        result
    }

    async fn reached(&self) {
        poll_fn(|cx| {
            self.progress.register(cx.waker());
            if self.state.lock().reached {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn unblock(&self) {
        self.state.lock().released = true;
        self.release.notify_all();
    }
}

struct BlockingTee {
    mock: MockTee,
    gate: Arc<HardwareGate>,
    derivations: AtomicUsize,
}

impl TeeCall for BlockingTee {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, tee_call::Error> {
        self.gate.call(HardwareCall::Report, || {
            self.mock.get_attestation_report(report_data)
        })
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        Some(self)
    }

    fn tee_type(&self) -> TeeType {
        self.mock.tee_type()
    }
}

impl TeeCallGetDerivedKey for BlockingTee {
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], tee_call::Error> {
        let count = self.derivations.fetch_add(1, Ordering::SeqCst) + 1;
        self.gate.call(HardwareCall::Derivation(count), || {
            self.mock.get_derived_key(policy)
        })
    }
}

fn config() -> AttestationVmConfig {
    AttestationVmConfig {
        current_time: None,
        root_cert_thumbprint: String::new(),
        console_enabled: false,
        interactive_console_enabled: false,
        secure_boot: false,
        tpm_enabled: false,
        tpm_version: AttestationTpmVersion::V138,
        tpm_persisted: false,
        hardware_sealing_policy: HardwareSealingPolicy::Hash,
        filtered_vpci_devices_allowed: true,
        vm_unique_id: String::new(),
        vmgs_provisioner: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Read,
    Write,
    Flush,
}

#[derive(Default)]
struct IoState {
    reads: usize,
    writes: usize,
    flushes: usize,
    block_at: Option<(Operation, usize)>,
    blocked: bool,
    released: bool,
    fail_flush_at: Option<usize>,
}

#[derive(Default)]
struct IoControl {
    state: Mutex<IoState>,
    progress: AtomicWaker,
    release: AtomicWaker,
}

impl IoControl {
    async fn before(&self, operation: Operation) -> Result<(), DiskError> {
        let (block, fail) = {
            let mut state = self.state.lock();
            let count = match operation {
                Operation::Read => &mut state.reads,
                Operation::Write => &mut state.writes,
                Operation::Flush => &mut state.flushes,
            };
            *count += 1;
            let count = *count;
            let block = state.block_at == Some((operation, count));
            state.blocked |= block;
            let fail = operation == Operation::Flush && state.fail_flush_at == Some(count);
            (block, fail)
        };
        self.progress.wake();
        if block {
            poll_fn(|cx| {
                self.release.register(cx.waker());
                if self.state.lock().released {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        if fail {
            return Err(DiskError::Io(std::io::Error::other(
                "injected flush failure",
            )));
        }
        Ok(())
    }

    async fn blocked(&self) {
        poll_fn(|cx| {
            self.progress.register(cx.waker());
            if self.state.lock().blocked {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn unblock(&self) {
        self.state.lock().released = true;
        self.release.wake();
    }
}

#[derive(Inspect)]
struct GatedDisk {
    disk: Disk,
    #[inspect(skip)]
    io: Arc<IoControl>,
}

impl DiskIo for GatedDisk {
    fn disk_type(&self) -> &str {
        "hardware-reseal-test"
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

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.disk.unmap_behavior()
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.disk.unmap(sector, count, block_level_only).await
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.io.before(Operation::Read).await?;
        self.disk.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.io.before(Operation::Write).await?;
        self.disk.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.io.before(Operation::Flush).await?;
        self.disk.sync_cache().await
    }
}

struct Fixture {
    worker: HardwareReseal,
    hardware: Arc<Hardware>,
    io: Arc<IoControl>,
    disk: Disk,
    broker: Task<()>,
}

impl Fixture {
    async fn new(driver: &DefaultDriver, existing: bool) -> Self {
        let hardware = Hardware::new();
        let io = Arc::new(IoControl::default());
        let disk = Disk::new(GatedDisk {
            disk: disklayer_ram::ram_disk(4 * 1024 * 1024, false).unwrap(),
            io: io.clone(),
        })
        .unwrap();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        vmgs.update_encryption_key(&DEK, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        if existing {
            let protector =
                runtime_sealing::create_protector(&MockTee(hardware.clone()), &config(), &DEK)
                    .unwrap();
            vmgs.write_file(FileId::HW_KEY_PROTECTOR, &protector)
                .await
                .unwrap();
            vmgs.flush().await.unwrap();
        }
        let tcb_floor =
            runtime_sealing::RuntimeTcbFloor::new(&MockTee(hardware.clone()), &config()).unwrap();
        // Initialization observes hardware once; idle-worker assertions count
        // only work after that required observation, not fixture provisioning.
        hardware.reports.store(0, Ordering::SeqCst);
        hardware.derivations.store(0, Ordering::SeqCst);
        *io.state.lock() = IoState::default();
        let (client, broker) = spawn_vmgs_broker(driver.clone(), vmgs);
        let worker = HardwareReseal::new(
            Arc::new(MigrationNotification::default()),
            PolledTimer::new(driver),
            client,
            Box::new(MockTee(hardware.clone())),
            config(),
            tcb_floor,
        );
        Self {
            worker,
            hardware,
            io,
            disk,
            broker,
        }
    }

    async fn close(self) {
        drop(self.worker);
        self.broker.await;
    }
}

// Poll the real worker alongside an explicit milestone or state RPC. No task
// scheduling guesses, busy loops, sleeps, or production timer delays are needed.
async fn drive_until<F: Future>(
    mut run: Pin<&mut impl Future<Output = HardwareReseal>>,
    milestone: F,
) -> F::Output {
    let mut milestone = pin!(milestone);
    poll_fn(|cx| {
        assert!(run.as_mut().poll(cx).is_pending(), "worker exited early");
        milestone.as_mut().poll(cx)
    })
    .await
}

// Return ownership at a Stop barrier so tests can advance private schedule
// timestamps instead of sleeping. Start preserves those timestamps, and
// already-running workers exercise polling without a Start request.
// Counter milestones only prove an attempt has entered hardware work. Stop
// drains the entire attempt before assertions inspect completion or I/O.
async fn finish_attempt(
    worker: HardwareReseal,
    milestone: impl Future<Output = ()>,
) -> HardwareReseal {
    let start = !worker.schedule.running;
    let (send, recv) = mesh::mpsc_channel();
    let mut run = pin!(worker.run(recv));
    if start {
        drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
            .await
            .unwrap();
    }
    drive_until(run.as_mut(), milestone).await;
    drive_until(run.as_mut(), send.call(StateRequest::Stop, ()))
        .await
        .unwrap();
    drop(send);
    run.await
}

#[async_test]
async fn start_and_resume_without_event_do_no_hardware_or_io_even_when_due(driver: DefaultDriver) {
    for existing in [false, true] {
        let mut fixture = Fixture::new(&driver, existing).await;
        // Even stale or missing protectors must not create work without an event.
        fixture.hardware.identity.store(0x73, Ordering::SeqCst);
        let expired = Instant::from_nanos(0);
        fixture.worker.schedule.deadline = expired;
        fixture.worker.schedule.not_before = expired;
        for _ in 0..2 {
            fixture.worker = finish_attempt(fixture.worker, std::future::ready(())).await;
            assert!(!fixture.worker.schedule.running);
            assert!(!fixture.worker.schedule.force_reseal);
            assert_eq!(fixture.worker.schedule.failures, 0);
            assert_eq!(fixture.worker.schedule.deadline, expired);
            assert_eq!(fixture.worker.schedule.not_before, expired);
            assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.io.state.lock().reads, 0);
            assert_eq!(fixture.io.state.lock().writes, 0);
            assert_eq!(fixture.io.state.lock().flushes, 0);
        }
        assert!(matches!(
            fixture.worker.save().await,
            Err(SaveError::NotSupported)
        ));
        fixture.close().await;
    }
}

#[async_test]
async fn explicit_notification_reseals_even_matching_hardware_with_same_dek(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, true).await;
    // Only the notification makes the distant deadline due.
    fixture.worker.schedule.running = true;
    fixture.worker.schedule.not_before = Instant::now();
    fixture.worker.schedule.deadline = Instant::now() + Duration::from_secs(86400);
    fixture.worker.notification.notify();
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(3, 3)).await;
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    let protector = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert!(
        runtime_sealing::protector_matches(&*fixture.worker.tee, &config(), &protector, &DEK)
            .unwrap()
    );
    fixture.close().await;
}

#[async_test]
async fn event_recovers_migration_and_success_stays_idle_without_more_events(
    driver: DefaultDriver,
) {
    let mut fixture = Fixture::new(&driver, true).await;
    fixture.hardware.identity.store(0x73, Ordering::SeqCst);
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(3, 3)).await;
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);

    *fixture.io.state.lock() = IoState::default();
    // Advance past all deadlines without sleeping. Check both the running
    // polling path and a normal restart: neither should repeat successful work.
    for running in [true, false] {
        fixture.worker.schedule.running = running;
        fixture.worker.schedule.deadline = Instant::from_nanos(0);
        fixture.worker.schedule.not_before = Instant::from_nanos(0);
        fixture.worker = finish_attempt(fixture.worker, std::future::ready(())).await;
        assert!(!fixture.worker.schedule.running);
        assert!(!fixture.worker.schedule.force_reseal);
        assert!(!fixture.worker.notification.pending.load(Ordering::SeqCst));
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.io.state.lock().reads, 0);
        assert_eq!(fixture.io.state.lock().writes, 0);
        assert_eq!(fixture.io.state.lock().flushes, 0);
    }

    // Check persisted bytes through a freshly opened store, not the broker cache.
    let disk = fixture.disk.clone();
    let hardware = fixture.hardware.clone();
    fixture.close().await;
    let mut reopened = Vmgs::open(disk, None).await.unwrap();
    reopened.unlock_with_encryption_key(&DEK).await.unwrap();
    assert!(reopened.active_encryption_key().unwrap() == &DEK);
    let protector = reopened.read_file(FileId::HW_KEY_PROTECTOR).await.unwrap();
    assert!(
        runtime_sealing::protector_matches(&MockTee(hardware), &config(), &protector, &DEK)
            .unwrap()
    );
}

#[async_test]
async fn stopped_worker_keeps_notification_and_start_honors_not_before(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, true).await;
    let notification = fixture.worker.notification.clone();
    // Far in the future solely to prove that Start/notify do not bypass the gate.
    let not_before = Instant::now() + Duration::from_secs(86400);
    fixture.worker.schedule.not_before = not_before;
    let (send, recv) = mesh::mpsc_channel();
    let mut run = pin!(fixture.worker.run(recv));
    assert!(futures::poll!(run.as_mut()).is_pending());
    notification.notify();
    notification.notify();
    assert!(futures::poll!(run.as_mut()).is_pending());
    assert!(notification.pending.load(Ordering::SeqCst));
    drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
        .await
        .unwrap();
    assert!(!notification.pending.load(Ordering::SeqCst));
    drive_until(run.as_mut(), send.call(StateRequest::Stop, ()))
        .await
        .unwrap();
    drop(send);
    fixture.worker = run.await;
    assert!(!fixture.worker.schedule.running);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.not_before, not_before);
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.io.state.lock().reads, 0);
    assert_eq!(fixture.io.state.lock().writes, 0);
    assert_eq!(fixture.io.state.lock().flushes, 0);
    fixture.worker.schedule.not_before = Instant::now();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(3, 3)).await;
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    fixture.close().await;
}

#[async_test]
async fn stop_drains_blocked_write_and_final_flush_and_retains_late_event(driver: DefaultDriver) {
    for block_at in [(Operation::Write, 1), (Operation::Flush, 2)] {
        let mut fixture = Fixture::new(&driver, false).await;
        fixture.io.state.lock().block_at = Some(block_at);
        let notification = fixture.worker.notification.clone();
        notification.notify();
        let (send, recv) = mesh::mpsc_channel();
        let mut run = pin!(fixture.worker.run(recv));
        drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
            .await
            .unwrap();
        drive_until(run.as_mut(), fixture.io.blocked()).await;
        let mut stop = pin!(send.call(StateRequest::Stop, ()));
        assert!(futures::poll!(run.as_mut()).is_pending());
        assert!(
            futures::poll!(stop.as_mut()).is_pending(),
            "Stop acknowledged blocked I/O"
        );
        // The successful completion must not erase an event received during I/O.
        notification.notify();
        fixture.io.unblock();
        drive_until(run.as_mut(), stop).await.unwrap();
        drop(send);
        fixture.worker = run.await;
        assert!(!fixture.worker.schedule.running);
        assert!(!fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, 0);
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.io.state.lock().flushes, 2);
        assert!(notification.pending.load(Ordering::SeqCst));
        assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);

        fixture.worker.schedule.not_before = Instant::now();
        fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(6, 6)).await;
        assert!(!fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, 0);
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 6);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 6);
        assert!(!notification.pending.load(Ordering::SeqCst));
        assert_eq!(fixture.io.state.lock().flushes, 4);
        fixture.close().await;
    }
}

#[async_test]
async fn blocked_hardware_keeps_executor_responsive_and_stop_drains_attempt(driver: DefaultDriver) {
    let calls = [
        HardwareCall::Report,
        HardwareCall::Derivation(1), // Protector creation.
        HardwareCall::Report,
        HardwareCall::Derivation(2), // Pre-write verification (first job).
        HardwareCall::Report,
        HardwareCall::Derivation(3), // Post-flush verification (second job).
    ];
    // The report gate blocks only its first matching call, not all three.
    for index in [0, 1, 3, 5] {
        let block_at = calls[index];
        let mut fixture = Fixture::new(&driver, false).await;
        let gate = HardwareGate::new(block_at);
        fixture.worker.tee = Arc::new(BlockingTee {
            mock: MockTee(fixture.hardware.clone()),
            gate: gate.clone(),
            derivations: AtomicUsize::new(0),
        });
        if index < 4 {
            // After releasing the first hardware job, independently prove
            // that Stop also waits for the remaining durable write/flush.
            fixture.io.state.lock().block_at = Some((Operation::Flush, 2));
        }
        let notification = fixture.worker.notification.clone();
        notification.notify();
        let (send, recv) = mesh::mpsc_channel();
        let mut run = pin!(fixture.worker.run(recv));
        drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
            .await
            .unwrap();
        drive_until(run.as_mut(), gate.reached()).await;
        {
            let io = fixture.io.state.lock();
            if index < 4 {
                assert_eq!(io.writes, 0);
                assert_eq!(io.flushes, 0);
            } else {
                assert!(io.writes > 0);
                assert_eq!(io.flushes, 2);
            }
        }

        let mut stop = pin!(send.call(StateRequest::Stop, ()));
        assert!(futures::poll!(stop.as_mut()).is_pending());
        // Each heartbeat is a separately scheduled task on the same executor,
        // not merely another future polled inline by drive_until.
        for _ in 0..3 {
            notification.notify();
            notification.notify();
            let heartbeat_gate = gate.clone();
            let heartbeat = driver.spawn("hardware-reseal-heartbeat", async move {
                assert_eq!(std::thread::current().id(), heartbeat_gate.executor_thread);
                let state = heartbeat_gate.state.lock();
                assert!(state.reached && state.active && !state.released);
                assert_eq!(state.calls, calls[..=index]);
            });
            drive_until(run.as_mut(), heartbeat).await;
            assert!(
                futures::poll!(stop.as_mut()).is_pending(),
                "Stop acknowledged blocked hardware at {block_at:?}"
            );
            assert!(notification.pending.load(Ordering::SeqCst));
        }

        gate.unblock();
        if index < 4 {
            drive_until(run.as_mut(), fixture.io.blocked()).await;
            assert!(futures::poll!(stop.as_mut()).is_pending());
            assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
            assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);
            assert_eq!(gate.state.lock().calls, calls[..4]);
            notification.notify();
            fixture.io.unblock();
        }
        drive_until(run.as_mut(), stop).await.unwrap();
        drop(send);
        fixture.worker = run.await;
        assert!(!fixture.worker.schedule.running);
        assert!(!fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, 0);
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
        assert_eq!(gate.state.lock().calls, calls);
        assert!(!gate.state.lock().active);
        assert!(fixture.io.state.lock().writes > 0);
        assert_eq!(fixture.io.state.lock().flushes, 2);
        assert!(notification.pending.load(Ordering::SeqCst));
        assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
        let protector = fixture
            .worker
            .vmgs
            .read_file(FileId::HW_KEY_PROTECTOR)
            .await
            .unwrap();
        // Verify outside the instrumented wrapper: only worker calls are
        // required to run off-thread. Stateless verification adds a derivation
        // but no report, and does not affect BlockingTee's call ordinals.
        assert!(
            runtime_sealing::protector_matches(
                &MockTee(fixture.hardware.clone()),
                &config(),
                &protector,
                &DEK,
            )
            .unwrap()
        );
        let derivations = fixture.hardware.derivations.load(Ordering::SeqCst);

        // The event storm was one latched event, not parallel/queued jobs.
        // Resume immediately without waiting for the production rate limit.
        fixture.worker.schedule.not_before = Instant::from_nanos(0);
        fixture.worker =
            finish_attempt(fixture.worker, fixture.hardware.reached(6, derivations + 3)).await;
        assert!(!notification.pending.load(Ordering::SeqCst));
        assert!(!fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, 0);
        assert_eq!(fixture.io.state.lock().flushes, 4);
        fixture.worker.schedule.running = true;
        fixture.worker.schedule.not_before = Instant::from_nanos(0);
        fixture.worker = finish_attempt(fixture.worker, std::future::ready(())).await;
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 6);
        assert_eq!(
            fixture.hardware.derivations.load(Ordering::SeqCst),
            derivations + 3
        );
        assert_eq!(fixture.io.state.lock().flushes, 4);
        assert_eq!(
            gate.state.lock().calls,
            [
                HardwareCall::Report,
                HardwareCall::Derivation(1),
                HardwareCall::Report,
                HardwareCall::Derivation(2),
                HardwareCall::Report,
                HardwareCall::Derivation(3),
                HardwareCall::Report,
                HardwareCall::Derivation(4),
                HardwareCall::Report,
                HardwareCall::Derivation(5),
                HardwareCall::Report,
                HardwareCall::Derivation(6),
            ]
        );
        assert!(!gate.state.lock().active);
        fixture.close().await;
    }
}

#[async_test]
async fn failed_report_keeps_recovery_pending_and_retries_successfully(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, false).await;
    fixture.hardware.fail_report.store(true, Ordering::SeqCst);
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(1, 0)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 0);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    assert_eq!(
        fixture.worker.schedule.due(),
        fixture.worker.schedule.not_before
    );
    assert_eq!(fixture.io.state.lock().writes, 0);
    assert_eq!(fixture.io.state.lock().flushes, 0);

    let deadline = fixture.worker.schedule.deadline;
    let not_before = fixture.worker.schedule.not_before;
    fixture.worker.start().await;
    fixture.worker.stop().await;
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    assert_eq!(fixture.worker.schedule.deadline, deadline);
    assert_eq!(fixture.worker.schedule.not_before, not_before);

    fixture.hardware.fail_report.store(false, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::now();
    fixture.worker.schedule.not_before = Instant::now();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(4, 3)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 4);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    fixture.close().await;
}

#[test]
fn initialization_report_failure_returns_no_floor_or_fallback() {
    let hardware = Hardware::new();
    let tee = MockTee(hardware.clone());
    hardware.fail_report.store(true, Ordering::SeqCst);
    assert!(runtime_sealing::RuntimeTcbFloor::new(&tee, &config()).is_err());
    assert_eq!(hardware.reports.load(Ordering::SeqCst), 1);
    assert_eq!(hardware.derivations.load(Ordering::SeqCst), 0);

    // A floor can only be obtained by a later successful fresh observation,
    // not by falling back to a default SVN or a cached protector.
    hardware.fail_report.store(false, Ordering::SeqCst);
    let _floor = runtime_sealing::RuntimeTcbFloor::new(&tee, &config()).unwrap();
    assert_eq!(hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(hardware.derivations.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn lower_report_is_rejected_before_derivation_or_write_and_remains_pending(
    driver: DefaultDriver,
) {
    let mut fixture = Fixture::new(&driver, true).await;
    fixture.hardware.tcb_version.store(6, Ordering::SeqCst);
    fixture.worker.notification.notify();
    for attempt in 1..=2 {
        fixture.worker.schedule.deadline = Instant::from_nanos(0);
        fixture.worker.schedule.not_before = Instant::from_nanos(0);
        fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(attempt, 0)).await;
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), attempt);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.io.state.lock().writes, 0);
        assert_eq!(fixture.io.state.lock().flushes, 0);
        assert!(fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, attempt as u32);
        assert!(!fixture.worker.notification.pending.load(Ordering::SeqCst));
    }

    fixture.hardware.tcb_version.store(7, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(5, 3)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 5);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    fixture.close().await;
}

#[async_test]
async fn failed_prepare_keeps_the_floor_raised_by_its_report(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, false).await;
    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.hardware.fail_derive.store(true, Ordering::SeqCst);
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(1, 1)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 1);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    assert_eq!(fixture.io.state.lock().writes, 0);
    assert_eq!(fixture.io.state.lock().flushes, 0);

    fixture.hardware.fail_derive.store(false, Ordering::SeqCst);
    fixture.hardware.tcb_version.store(8, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(2, 1)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 1);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 2);
    assert_eq!(fixture.io.state.lock().writes, 0);
    assert_eq!(fixture.io.state.lock().flushes, 0);

    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(5, 4)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 5);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 4);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    fixture.close().await;
}

#[async_test]
async fn forged_cached_protector_metadata_cannot_lower_the_resident_floor(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, true).await;
    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(3, 3)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    let protector = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert_eq!(protector_svn(&protector), 9);

    // Root-controlled metadata advertises an older SVN but retains the old
    // ciphertext/MAC. It must never initialize or replace the resident floor.
    let mut forged = HardwareKeyProtectorV3::read_from_bytes(&protector).unwrap();
    forged.header.svn[..8].copy_from_slice(&6u64.to_le_bytes());
    assert!(
        !runtime_sealing::protector_matches(
            &*fixture.worker.tee,
            &config(),
            forged.as_bytes(),
            &DEK,
        )
        .unwrap()
    );
    // Stateless verification derives from the header; it obtains no report.
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 4);
    assert!(
        fixture
            .worker
            .vmgs
            .write_file_if_active_key_matches(
                FileId::HW_KEY_PROTECTOR,
                forged.as_bytes().to_vec(),
                DEK,
            )
            .await
            .unwrap()
    );
    *fixture.io.state.lock() = IoState::default();

    // SVN 8 is above the initialization floor (7), but below the resident 9.
    // Then try the forged header's 6 as well, without another notification.
    fixture.worker.notification.notify();
    for (index, tcb_version) in [8, 6].into_iter().enumerate() {
        fixture
            .hardware
            .tcb_version
            .store(tcb_version, Ordering::SeqCst);
        fixture.worker.schedule.deadline = Instant::from_nanos(0);
        fixture.worker.schedule.not_before = Instant::from_nanos(0);
        fixture.worker =
            finish_attempt(fixture.worker, fixture.hardware.reached(4 + index, 4)).await;
        assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 4 + index);
        assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 4);
        assert_eq!(fixture.io.state.lock().writes, 0);
        assert_eq!(fixture.io.state.lock().flushes, 0);
        assert!(fixture.worker.schedule.force_reseal);
        assert_eq!(fixture.worker.schedule.failures, index as u32 + 1);
        assert!(!fixture.worker.notification.pending.load(Ordering::SeqCst));
    }
    let cached = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert!(cached.as_slice() == forged.as_bytes());

    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(8, 7)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 8);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 7);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    fixture.close().await;
}

#[async_test]
async fn changed_report_during_pre_or_post_verification_keeps_recovery_pending(
    driver: DefaultDriver,
) {
    for post_flush in [false, true] {
        for changed_svn in [6, 9] {
            let mut fixture = Fixture::new(&driver, false).await;
            let gate = HardwareGate::new(HardwareCall::Derivation(1));
            if post_flush {
                fixture.io.state.lock().block_at = Some((Operation::Flush, 2));
            } else {
                fixture.worker.tee = Arc::new(BlockingTee {
                    mock: MockTee(fixture.hardware.clone()),
                    gate: gate.clone(),
                    derivations: AtomicUsize::new(0),
                });
            }
            fixture.worker.notification.notify();
            let (send, recv) = mesh::mpsc_channel();
            let mut run = pin!(fixture.worker.run(recv));
            drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
                .await
                .unwrap();
            if post_flush {
                drive_until(run.as_mut(), fixture.io.blocked()).await;
                assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
                assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);
            } else {
                // Creation has captured SVN 7, but its first derivation is
                // gated so the following verification must observe the change.
                drive_until(run.as_mut(), gate.reached()).await;
                assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 1);
                assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 0);
                assert_eq!(fixture.io.state.lock().writes, 0);
            }
            fixture
                .hardware
                .tcb_version
                .store(changed_svn, Ordering::SeqCst);
            let mut stop = pin!(send.call(StateRequest::Stop, ()));
            assert!(futures::poll!(stop.as_mut()).is_pending());
            if post_flush {
                fixture.io.unblock();
            } else {
                gate.unblock();
            }
            drive_until(run.as_mut(), stop).await.unwrap();
            drop(send);
            fixture.worker = run.await;

            let reports = if post_flush { 3 } else { 2 };
            let derivations = if post_flush { 2 } else { 1 };
            assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), reports);
            assert_eq!(
                fixture.hardware.derivations.load(Ordering::SeqCst),
                derivations
            );
            assert!(fixture.worker.schedule.force_reseal);
            assert_eq!(fixture.worker.schedule.failures, 1);
            assert!(!fixture.worker.notification.pending.load(Ordering::SeqCst));
            let writes = fixture.io.state.lock().writes;
            let flushes = fixture.io.state.lock().flushes;
            if post_flush {
                // A post-flush mismatch cannot undo publication, but must
                // schedule recovery even without another migration event.
                assert!(writes > 0);
                assert_eq!(flushes, 2);
            } else {
                // Even an upgrade must not publish a candidate with stale SVN.
                assert_eq!(writes, 0);
                assert_eq!(flushes, 0);
            }

            // A mismatching upgraded report still ratchets the floor to 9.
            // A rejected downgrade leaves it at 7. Neither permits this retry.
            let rejected_svn = if changed_svn == 9 { 8 } else { 6 };
            fixture
                .hardware
                .tcb_version
                .store(rejected_svn, Ordering::SeqCst);
            fixture.worker.schedule.deadline = Instant::from_nanos(0);
            fixture.worker.schedule.not_before = Instant::from_nanos(0);
            fixture.worker = finish_attempt(
                fixture.worker,
                fixture.hardware.reached(reports + 1, derivations),
            )
            .await;
            assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), reports + 1);
            assert_eq!(
                fixture.hardware.derivations.load(Ordering::SeqCst),
                derivations
            );
            assert_eq!(fixture.io.state.lock().writes, writes);
            assert_eq!(fixture.io.state.lock().flushes, flushes);
            assert!(fixture.worker.schedule.force_reseal);
            assert_eq!(fixture.worker.schedule.failures, 2);

            let retained_svn = if changed_svn == 9 { 9 } else { 7 };
            fixture
                .hardware
                .tcb_version
                .store(retained_svn, Ordering::SeqCst);
            fixture.worker.schedule.deadline = Instant::from_nanos(0);
            fixture.worker.schedule.not_before = Instant::from_nanos(0);
            fixture.worker = finish_attempt(
                fixture.worker,
                fixture.hardware.reached(reports + 4, derivations + 3),
            )
            .await;
            assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), reports + 4);
            assert_eq!(
                fixture.hardware.derivations.load(Ordering::SeqCst),
                derivations + 3
            );
            assert!(!fixture.worker.schedule.force_reseal);
            assert_eq!(fixture.worker.schedule.failures, 0);
            assert!(fixture.io.state.lock().writes > writes);
            assert_eq!(fixture.io.state.lock().flushes, flushes + 2);
            let protector = fixture
                .worker
                .vmgs
                .read_file(FileId::HW_KEY_PROTECTOR)
                .await
                .unwrap();
            assert_eq!(protector_svn(&protector), retained_svn);
            assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
            fixture.close().await;
        }
    }
}

#[async_test]
async fn migration_during_event_flush_is_detected_without_another_event(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, false).await;
    fixture.io.state.lock().block_at = Some((Operation::Flush, 2));
    fixture.worker.notification.notify();
    let (send, recv) = mesh::mpsc_channel();
    let mut run = pin!(fixture.worker.run(recv));
    drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
        .await
        .unwrap();
    drive_until(run.as_mut(), fixture.io.blocked()).await;
    fixture.hardware.identity.store(0x73, Ordering::SeqCst);
    let stop = send.call(StateRequest::Stop, ());
    fixture.io.unblock();
    drive_until(run.as_mut(), stop).await.unwrap();
    drop(send);
    fixture.worker = run.await;
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(!fixture.worker.notification.pending.load(Ordering::SeqCst));

    fixture.worker.schedule.deadline = Instant::now();
    fixture.worker.schedule.not_before = Instant::now();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(6, 6)).await;
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 6);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 6);
    assert_eq!(fixture.io.state.lock().flushes, 4);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    let protector = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert!(
        runtime_sealing::protector_matches(&*fixture.worker.tee, &config(), &protector, &DEK,)
            .unwrap()
    );
    fixture.close().await;
}

#[async_test]
async fn save_is_rejected_and_reset_preserves_upgrade_after_failed_flush(driver: DefaultDriver) {
    let mut fixture = Fixture::new(&driver, false).await;
    let resident_floor = fixture.worker.tcb_floor.clone();
    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.io.state.lock().fail_flush_at = Some(2);
    fixture.worker.notification.notify();
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(2, 2)).await;
    // The upgrade was accepted and reached publication, but failed durability.
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    let cached = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert_eq!(protector_svn(&cached), 9);

    // Serialized reconstruction must not silently replace the floor with a
    // new report or cached metadata. Even an empty restore is unsupported.
    fixture.hardware.tcb_version.store(8, Ordering::SeqCst);
    assert!(matches!(
        fixture.worker.save().await,
        Err(SaveError::NotSupported)
    ));
    assert!(matches!(
        fixture
            .worker
            .restore(SavedStateBlob::new(vmcore::save_restore::NoSavedState))
            .await,
        Err(RestoreError::SavedStateNotSupported)
    ));
    fixture.worker.reset().await.unwrap();
    fixture.worker.start().await;
    fixture.worker.stop().await;
    assert!(Arc::ptr_eq(&resident_floor, &fixture.worker.tcb_floor));
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);

    *fixture.io.state.lock() = IoState::default();
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(3, 2)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.io.state.lock().writes, 0);
    assert_eq!(fixture.io.state.lock().flushes, 0);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 2);

    // Returning to the retained floor retries the write, not just the valid
    // cached protector left by the failed flush. No new event is required.
    fixture.hardware.tcb_version.store(9, Ordering::SeqCst);
    fixture.worker.schedule.deadline = Instant::from_nanos(0);
    fixture.worker.schedule.not_before = Instant::from_nanos(0);
    fixture.worker = finish_attempt(fixture.worker, fixture.hardware.reached(6, 5)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 6);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 5);
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    fixture.close().await;
}

#[async_test]
async fn failed_final_flush_retries_write_even_when_cached_protector_matches(
    driver: DefaultDriver,
) {
    let mut fixture = Fixture::new(&driver, false).await;
    {
        let mut io = fixture.io.state.lock();
        io.block_at = Some((Operation::Flush, 2));
        io.fail_flush_at = Some(2);
    }
    fixture.worker.notification.notify();
    let (send, recv) = mesh::mpsc_channel();
    let mut run = pin!(fixture.worker.run(recv));
    drive_until(run.as_mut(), send.call(StateRequest::Start, ()))
        .await
        .unwrap();
    drive_until(run.as_mut(), fixture.io.blocked()).await;
    let stop = send.call(StateRequest::Stop, ());
    fixture.io.unblock();
    drive_until(run.as_mut(), stop).await.unwrap();
    drop(send);
    fixture.worker = run.await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 2);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 1);
    let cached = fixture
        .worker
        .vmgs
        .read_file(FileId::HW_KEY_PROTECTOR)
        .await
        .unwrap();
    assert!(
        runtime_sealing::protector_matches(&*fixture.worker.tee, &config(), &cached, &DEK).unwrap()
    );
    // The failed flush skipped post-verification; this stateless check adds
    // only one derivation, not the missing third report of that attempt.
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.hardware.derivations.load(Ordering::SeqCst), 3);
    let derivations = fixture.hardware.derivations.load(Ordering::SeqCst);
    *fixture.io.state.lock() = IoState::default();
    fixture.worker.schedule.deadline = Instant::now();
    fixture.worker.schedule.not_before = Instant::now();
    fixture.worker =
        finish_attempt(fixture.worker, fixture.hardware.reached(5, derivations + 3)).await;
    assert_eq!(fixture.hardware.reports.load(Ordering::SeqCst), 5);
    assert_eq!(
        fixture.hardware.derivations.load(Ordering::SeqCst),
        derivations + 3
    );
    assert!(!fixture.worker.schedule.force_reseal);
    assert_eq!(fixture.worker.schedule.failures, 0);
    assert!(fixture.io.state.lock().writes > 0);
    assert_eq!(fixture.io.state.lock().flushes, 2);
    assert!(fixture.worker.vmgs.active_encryption_key().await.unwrap() == DEK);
    fixture.close().await;
}
