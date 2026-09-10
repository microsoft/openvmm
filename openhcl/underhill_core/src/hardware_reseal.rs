// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Event-triggered hardware resealing. GET is only a hint: the current hardware
//! must authenticate the persisted protector. There is no periodic verification
//! to cover missed events or close the crash window before a durable reseal.

use cvm_tracing::CVM_ALLOWED;
use futures::StreamExt;
use futures::task::AtomicWaker;
use inspect::Inspect;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
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
use underhill_attestation::runtime_sealing;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmgs::FileId;
use vmgs_broker::VmgsClient;

const MIN_RESEAL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(60);

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

/// Managed with VM state units, so stop drains any active broker operation
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
    tee: Box<dyn TeeCall>,
    #[inspect(skip)]
    config: AttestationVmConfig,
    checks: u64,
    reseals: u64,
    degraded: bool,
}

impl HardwareReseal {
    pub fn new(
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
            tee,
            config,
            checks: 0,
            reseals: 0,
            degraded: false,
        }
    }

    pub async fn run(mut self, mut recv: mesh::Receiver<StateRequest>) -> Self {
        loop {
            enum Event {
                State(Option<StateRequest>),
                Check,
            }
            let event = poll_fn(|cx| {
                // State transitions win over a timer or an event storm.
                if let Poll::Ready(req) = recv.poll_next_unpin(cx) {
                    return Poll::Ready(Event::State(req));
                }
                if !self.schedule.running {
                    return Poll::Pending;
                }
                if self.notification.take(cx) {
                    self.schedule.notified(Instant::now());
                }
                if !self.schedule.force_reseal {
                    return Poll::Pending;
                }
                self.timer
                    .poll_until(cx, self.schedule.due())
                    .map(|_| Event::Check)
            })
            .await;
            match event {
                Event::State(Some(req)) => req.apply(&mut self).await,
                Event::State(None) => break,
                Event::Check => {
                    // Do not cancel an in-flight VMGS write when Stop arrives.
                    // The state request is acknowledged only after I/O drains.
                    let result = self.check().await;
                    self.checks = self.checks.saturating_add(1);
                    match &result {
                        Ok(resealed) => {
                            if *resealed {
                                self.reseals = self.reseals.saturating_add(1);
                                tracelimit::info_ratelimited!(
                                    CVM_ALLOWED,
                                    "VMGS hardware protector resealed"
                                );
                            }
                            self.degraded = false;
                        }
                        Err(error) => {
                            self.degraded = true;
                            tracelimit::warn_ratelimited!(
                                CVM_ALLOWED,
                                error = error.as_ref() as &dyn std::error::Error,
                                "VMGS hardware protector recovery pending; retrying"
                            );
                        }
                    }
                    let mut jitter = [0];
                    // Jitter is scheduling only, not key material. RNG failure
                    // must not prevent retrying a potentially stale protector.
                    let _ = getrandom::fill(&mut jitter);
                    self.schedule
                        .completed(Instant::now(), result.is_ok(), jitter[0]);
                }
            }
        }
        self
    }

    async fn check(&self) -> anyhow::Result<bool> {
        let key = self.vmgs.active_encryption_key().await?;
        let protector = runtime_sealing::create_protector(&*self.tee, &self.config, &key)?;
        // Validate with a second hardware derivation, not the seal-time keys.
        anyhow::ensure!(
            runtime_sealing::protector_matches(&*self.tee, &self.config, &protector, &key)?,
            "hardware changed while constructing the protector"
        );
        anyhow::ensure!(
            self.vmgs
                .write_file_if_encryption_key_matches(
                    FileId::HW_KEY_PROTECTOR,
                    protector.clone(),
                    key,
                )
                .await?,
            "VMGS key changed while constructing the protector"
        );
        // Migration can happen during the write/flush, too. An event arriving
        // here remains latched for another attempt regardless of this result.
        anyhow::ensure!(
            runtime_sealing::protector_matches(&*self.tee, &self.config, &protector, &key)?,
            "hardware changed while persisting the protector"
        );
        Ok(true)
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
    }

    async fn stop(&mut self) {
        self.schedule.running = false;
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.schedule.force_reseal = true;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        // VMGS owns the DEK. No additional secret or scheduling state is saved;
        // saved-state reconstruction explicitly notifies the new worker to
        // rewrite for durability. Starting alone does not trigger recovery.
        Ok(None)
    }

    async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
        Err(RestoreError::SavedStateNotSupported)
    }
}

#[cfg(test)]
mod tests;
