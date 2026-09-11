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
            tee: tee.into(),
            config: Arc::new(config),
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
                    .map(|_| Event::Reseal)
            })
            .await;
            match event {
                Event::State(Some(req)) => req.apply(&mut self).await,
                Event::State(None) => break,
                Event::Reseal => {
                    // Await the whole attempt, including offloaded hardware
                    // calls. Stop is acknowledged only after hardware work and
                    // VMGS I/O drain; it must not detach a pending write.
                    let result = self.reseal().await;
                    match &result {
                        Ok(()) => {
                            tracelimit::info_ratelimited!(
                                CVM_ALLOWED,
                                "VMGS hardware protector resealed"
                            );
                        }
                        Err(error) => {
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

    /// Reseal the active DEK, durably publish its protector, and verify it
    /// against fresh hardware derivations before and after persistence.
    async fn reseal(&self) -> anyhow::Result<()> {
        let key = self.vmgs.active_encryption_key().await?;
        // TEE report/key ioctls are synchronous. Keep their latency off the VP
        // executors (and the GET thread). Only one blocking job per worker is
        // in flight, and each is awaited before advancing the attempt.
        let tee = self.tee.clone();
        let config = self.config.clone();
        let span = tracing::Span::current();
        let protector = blocking::unblock(move || {
            span.in_scope(|| -> anyhow::Result<Vec<u8>> {
                let protector = runtime_sealing::create_protector(&*tee, &config, &key)?;
                // Validate with a second derivation, not the seal-time keys.
                anyhow::ensure!(
                    runtime_sealing::protector_matches(&*tee, &config, &protector, &key)?,
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
        anyhow::ensure!(
            self.vmgs
                .write_file_if_active_key_matches(FileId::HW_KEY_PROTECTOR, protector.clone(), key)
                .await?,
            "VMGS key changed while constructing the protector"
        );
        // Migration can happen during the write/flush, too. An event arriving
        // here remains latched for another attempt regardless of this result.
        let tee = self.tee.clone();
        let config = self.config.clone();
        let span = tracing::Span::current();
        blocking::unblock(move || {
            span.in_scope(|| -> anyhow::Result<()> {
                anyhow::ensure!(
                    runtime_sealing::protector_matches(&*tee, &config, &protector, &key)?,
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
