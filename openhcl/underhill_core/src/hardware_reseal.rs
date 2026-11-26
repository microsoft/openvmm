// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Event-triggered hardware resealing. GET is only a hint: the current hardware
//! must authenticate the persisted protector. There is no periodic verification
//! to cover missed events or close the crash window before a durable reseal.
//! Serialized restore separately forces one durable rewrite: hardware may have
//! changed without a notification during downtime or after the save cutoff.

pub(crate) mod saved_state;

pub(crate) use saved_state::SavedHardwareResealState;

use anyhow::Context as _;
use cvm_tracing::CVM_ALLOWED;
use futures::StreamExt;
use futures::task::AtomicWaker;
use inspect::Inspect;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use state_unit::StateRequest;
use state_unit::StateUnit;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tee_call::TeeCall;
use tracing::Instrument;
use underhill_attestation::runtime_sealing;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmgs::FileId;
use vmgs_broker::VmgsClient;

const MIN_RESEAL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Stable servicing state-unit identifier.
pub const STATE_UNIT_NAME: &str = "hardware_reseal";

/// Detect enrollment from the unit list before VM construction. Do not infer
/// enrollment from a host setting or the destination's VMGS protector.
pub(crate) fn has_saved_state(units: &[state_unit::SavedStateUnit]) -> anyhow::Result<bool> {
    let mut matches = units.iter().filter(|unit| unit.name == STATE_UNIT_NAME);
    let found = matches.next().is_some();
    anyhow::ensure!(
        matches.next().is_none(),
        "duplicate hardware reseal saved state"
    );
    Ok(found)
}

/// Restore provenance and enrollment information, not a replacement TCB floor.
pub(crate) struct RestoreContext {
    pub has_saved_state: bool,
    pub from_host: bool,
}

/// Select boot enrollment or saved enrollment without observing hardware.
/// Hardware eligibility includes policy, key-derivation support and encrypted VMGS.
pub(crate) fn should_enable(
    eligible: bool,
    boot_floor_available: bool,
    restore: Option<RestoreContext>,
) -> anyhow::Result<bool> {
    if let Some(restore) = restore {
        anyhow::ensure!(
            !(restore.has_saved_state && restore.from_host),
            "cannot restore hardware resealing from unauthenticated host servicing state"
        );
        anyhow::ensure!(
            !restore.has_saved_state || eligible,
            "saved hardware resealing requires compatible hardware, policy and encrypted VMGS"
        );
        // Never use boot observations to invent enrollment on a restore path.
        Ok(restore.has_saved_state)
    } else {
        Ok(eligible && boot_floor_available)
    }
}

/// A bounded, level-triggered notification. Events before startup or during an
/// in-flight attempt stay pending; duplicate events never allocate queue entries.
#[derive(Default)]
pub(crate) struct MigrationNotification {
    pending: AtomicBool,
    waker: AtomicWaker,
}

impl MigrationNotification {
    pub fn notify(&self) {
        self.pending.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn take(&self, cx: &Context<'_>) -> bool {
        self.waker.register(cx.waker());
        self.pending.swap(false, Ordering::AcqRel)
    }
}

/// Scheduling is separate from I/O so retries and notification races can be
/// tested without sleeping or real hardware. No field contains secret material.
#[derive(Inspect)]
struct Schedule {
    running: bool,
    // Pending recovery, including retries. Deadlines are ignored when false.
    force_reseal: bool,
    failures: u32,
    #[inspect(skip)]
    deadline: Instant,
    #[inspect(skip)]
    not_before: Instant,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            running: false,
            force_reseal: false,
            failures: 0,
            deadline: now,
            not_before: now,
        }
    }

    fn notified(&mut self, now: Instant) {
        self.force_reseal = true;
        self.deadline = now;
    }

    fn due(&self) -> Instant {
        self.deadline.max(self.not_before)
    }

    fn completed(&mut self, now: Instant, success: bool, jitter: u8) {
        if success {
            self.failures = 0;
            self.force_reseal = false;
            self.not_before = now + MIN_RESEAL_INTERVAL;
            // No new deadline: stay idle until another notification.
        } else {
            self.failures = self.failures.saturating_add(1);
            // A failed flush may leave a valid protector in cache but not on
            // durable storage. Retry the write, not just a cached verification.
            self.force_reseal = true;
            let backoff = Duration::from_secs(1 << self.failures.saturating_sub(1).min(6));
            let delay =
                (backoff + Duration::from_millis(u64::from(jitter) * 4)).min(MAX_RETRY_INTERVAL);
            self.not_before = now + delay;
            self.deadline = self.not_before;
        }
    }
}

/// Managed with VM state units, so stop drains hardware work and broker I/O
/// before VMGS is snapshotted. Keys are borrowed/copied only during an attempt.
#[derive(Inspect)]
pub(crate) struct HardwareReseal {
    #[inspect(flatten)]
    schedule: Schedule,
    #[inspect(skip)]
    notification: Arc<MigrationNotification>,
    #[inspect(skip)]
    timer: PolledTimer,
    #[inspect(skip)]
    vmgs: VmgsClient,
    #[inspect(skip)]
    tee: Arc<dyn TeeCall>,
    #[inspect(skip)]
    config: Arc<AttestationVmConfig>,
    // Trusted local reports or validated servicing state initialize this floor.
    // A shared mutex preserves advances even when a blocking job returns Err.
    #[inspect(skip)]
    tcb_floor: Arc<Mutex<Option<runtime_sealing::RuntimeTcbFloor>>>,
}

impl HardwareReseal {
    pub fn new(
        notification: Arc<MigrationNotification>,
        timer: PolledTimer,
        vmgs: VmgsClient,
        tee: Box<dyn TeeCall>,
        config: AttestationVmConfig,
        tcb_floor: runtime_sealing::RuntimeTcbFloor,
    ) -> Self {
        let worker = Self::new_for_restore(notification, timer, vmgs, tee, config);
        *worker.tcb_floor.lock() = Some(tcb_floor);
        worker
    }

    /// Construct without observing hardware or bootstrapping a potentially lower
    /// floor. Only use when servicing state is available, then restore before
    /// starting. Until restore succeeds, no reseal I/O is allowed.
    pub fn new_for_restore(
        notification: Arc<MigrationNotification>,
        timer: PolledTimer,
        vmgs: VmgsClient,
        tee: Box<dyn TeeCall>,
        config: AttestationVmConfig,
    ) -> Self {
        Self {
            schedule: Schedule::new(Instant::now()),
            notification,
            timer,
            vmgs,
            tee: tee.into(),
            config: Arc::new(config),
            tcb_floor: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run(mut self, mut recv: mesh::Receiver<StateRequest>) -> Self {
        loop {
            enum Event {
                State(Option<StateRequest>),
                Reseal,
            }
            let event = poll_fn(|cx| {
                // State transitions win over a timer or an event storm.
                if let Poll::Ready(req) = recv.poll_next_unpin(cx) {
                    return Poll::Ready(Event::State(req));
                }
                if !self.schedule.running || self.tcb_floor.lock().is_none() {
                    return Poll::Pending;
                }
                if self.notification.take(cx) {
                    let now = Instant::now();
                    self.schedule.notified(now);
                    tracelimit::info_ratelimited!(
                        CVM_ALLOWED,
                        delay_ms = self.schedule.due().saturating_sub(now).as_millis() as u64,
                        failures = self.schedule.failures,
                        "hardware reseal notification consumed; work scheduled"
                    );
                }
                if !self.schedule.force_reseal {
                    return Poll::Pending;
                }
                self.timer
                    .poll_until(cx, self.schedule.due())
                    .map(|_| Event::Reseal)
            })
            .await;
            match event {
                Event::State(Some(req)) => req.apply(&mut self).await,
                Event::State(None) => break,
                Event::Reseal => {
                    let span = tracing::info_span!("hardware_reseal_attempt", CVM_ALLOWED);
                    async {
                        // Await the whole attempt, including offloaded hardware
                        // calls. Stop is acknowledged only after hardware work and
                        // VMGS I/O drain; it must not detach a pending write.
                        let started = Instant::now();
                        let previous_failures = self.schedule.failures;
                        tracelimit::info_ratelimited!(
                            CVM_ALLOWED,
                            previous_failures,
                            "VMGS hardware protector reseal started"
                        );
                        let result = self.reseal().await;
                        let completed = Instant::now();
                        let elapsed_ms = completed.saturating_sub(started).as_millis() as u64;
                        let pending_notification =
                            self.notification.pending.load(Ordering::Acquire);
                        let mut jitter = [0];
                        // Jitter is scheduling only, not key material. RNG failure
                        // must not prevent retrying a potentially stale protector.
                        let _ = getrandom::fill(&mut jitter);
                        self.schedule
                            .completed(completed, result.is_ok(), jitter[0]);
                        match &result {
                            Ok(()) => {
                                tracelimit::info_ratelimited!(
                                    CVM_ALLOWED,
                                    elapsed_ms,
                                    previous_failures,
                                    pending_notification,
                                    "VMGS hardware protector resealed"
                                );
                            }
                            Err(error) => {
                                tracelimit::warn_ratelimited!(
                                    CVM_ALLOWED,
                                    error = error.as_ref() as &dyn std::error::Error,
                                    elapsed_ms,
                                    failures = self.schedule.failures,
                                    retry_delay_ms =
                                        self.schedule.due().saturating_sub(completed).as_millis()
                                            as u64,
                                    pending_notification,
                                    "VMGS hardware protector recovery pending; retrying"
                                );
                            }
                        }
                    }
                    .instrument(span)
                    .await;
                }
            }
        }
        self
    }

    /// Reseal the active DEK, durably publish its protector, and verify it
    /// against fresh hardware derivations before and after persistence.
    async fn reseal(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tcb_floor.lock().is_some(),
            "hardware reseal floor is not initialized"
        );
        tracelimit::info_ratelimited!(CVM_ALLOWED, "requesting active VMGS key");
        let key = self
            .vmgs
            .active_encryption_key()
            .await
            .context("hardware reseal: obtaining active VMGS key")?;
        // TEE report/key ioctls are synchronous. Keep their latency off the VP
        // executors (and the GET thread). Only one blocking job per worker is
        // in flight, and each is awaited before advancing the attempt.
        let tee = self.tee.clone();
        let config = self.config.clone();
        let tcb_floor = self.tcb_floor.clone();
        let span = tracing::Span::current();
        tracelimit::info_ratelimited!(CVM_ALLOWED, "queueing hardware protector preparation");
        let protector = blocking::unblock(move || {
            span.in_scope(|| -> anyhow::Result<Vec<u8>> {
                tracelimit::info_ratelimited!(CVM_ALLOWED, "preparing hardware protector");
                let mut floor = tcb_floor.lock();
                let floor = floor
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("hardware reseal floor is not initialized"))?;
                let protector = floor
                    .create_protector(&*tee, &config, &key)
                    .context("hardware reseal: checking TCB floor and creating protector")?;
                // Validate with a second derivation, not the seal-time keys.
                tracelimit::info_ratelimited!(
                    CVM_ALLOWED,
                    "verifying hardware protector before write"
                );
                anyhow::ensure!(
                    floor
                        .verify_protector(&*tee, &config, &protector, &key)
                        .context("hardware reseal: verifying before write")?,
                    "hardware changed while constructing the protector"
                );
                Ok(protector)
            })
        })
        .await?;
        // HW_KEY_PROTECTOR is written without VMGS-level encryption so it can
        // be read before unlocking VMGS. The active-DEK comparison is defensive:
        // although the broker currently cannot rotate the DEK, future concurrent
        // rotation must not let us publish a protector for a stale key.
        tracelimit::info_ratelimited!(CVM_ALLOWED, "requesting hardware protector write and flush");
        anyhow::ensure!(
            self.vmgs
                .write_file_if_active_key_matches(FileId::HW_KEY_PROTECTOR, protector.clone(), key)
                .await
                .context("hardware reseal: writing and flushing protector")?,
            "VMGS key changed while constructing the protector"
        );
        // Migration can happen during the write/flush, too. An event arriving
        // here remains latched for another attempt regardless of this result.
        let tee = self.tee.clone();
        let config = self.config.clone();
        let tcb_floor = self.tcb_floor.clone();
        let span = tracing::Span::current();
        tracelimit::info_ratelimited!(CVM_ALLOWED, "queueing post-flush hardware verification");
        blocking::unblock(move || {
            span.in_scope(|| -> anyhow::Result<()> {
                tracelimit::info_ratelimited!(
                    CVM_ALLOWED,
                    "verifying hardware protector after flush"
                );
                anyhow::ensure!(
                    tcb_floor
                        .lock()
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("hardware reseal floor is not initialized"))?
                        .verify_protector(&*tee, &config, &protector, &key)
                        .context("hardware reseal: verifying after flush")?,
                    "hardware changed while persisting the protector"
                );
                Ok(())
            })
        })
        .await
    }
}

impl inspect::InspectMut for HardwareReseal {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.inspect(req);
    }
}

impl StateUnit for HardwareReseal {
    async fn start(&mut self) {
        // Starting or resuming does not create work or reset retry backoff.
        // Pending notifications and recovery survive a normal stop/start.
        self.schedule.running = true;
        tracelimit::info_ratelimited!(
            CVM_ALLOWED,
            floor_initialized = self.tcb_floor.lock().is_some(),
            recovery_pending = self.schedule.force_reseal,
            pending_notification = self.notification.pending.load(Ordering::Acquire),
            "hardware reseal worker started"
        );
    }

    async fn stop(&mut self) {
        self.schedule.running = false;
        tracelimit::info_ratelimited!(
            CVM_ALLOWED,
            recovery_pending = self.schedule.force_reseal,
            pending_notification = self.notification.pending.load(Ordering::Acquire),
            "hardware reseal worker stopped; in-flight work drained"
        );
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        // Reset is not a migration notification. Preserve pending work, retry
        // timing, and the resident floor without introducing recovery.
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        self.save_stopped()
            .map(|state| Some(SavedStateBlob::new(state)))
            .map_err(SaveError::Other)
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        if self.schedule.running {
            return Err(RestoreError::Other(anyhow::anyhow!(
                "hardware reseal must be stopped before restore"
            )));
        }
        self.restore_stopped(state.parse::<SavedHardwareResealState>()?)
            .map_err(RestoreError::InvalidSavedState)
    }
}

#[cfg(test)]
mod tests;
