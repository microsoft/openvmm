// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Notification and parking protocol for one HVF vCPU.
//!
//! Hypervisor.framework gives a vCPU two distinct wake paths:
//!
//! - `hv_vcpus_exit` cancels a blocking `hv_vcpu_run`. Apple's `hv_vcpu.h`
//!   documents that cancellation is sticky: if the vCPU is not running, its
//!   next `hv_vcpu_run` returns without entering the guest.
//! - A Rust [`Waker`] schedules another poll after the VP future has parked
//!   outside `hv_vcpu_run`.
//!
//! [`VpActor`] joins those paths around one contract: producers publish
//! persistent work before [`VpActor::notify`], and the sole consumer may park
//! only after scanning work and proving that the notification sequence did not
//! change during that scan.
//!
//! ## Synchronization model
//!
//! `notify` linearizes at the release increment of `sequence`. `begin_scan` and
//! the final `try_park` check acquire that publication. The parked-waker mutex
//! closes the remaining window: a notification either changes the sequence
//! before the consumer commits to parking, or takes the already-registered
//! waker. Duplicate notifications may coalesce because the underlying work is
//! persistent and is rescanned.
//!
//! The `vcpu` mutex separately serializes `hv_vcpus_exit` with vCPU replacement
//! and destruction, as required by Apple's owning-thread lifecycle contract.
//! The `loom_notification_cannot_leave_published_work_parked` model explores
//! the publish/scan/register/check interleavings against this implementation.

use crate::abi;
#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
#[cfg(all(test, feature = "loom"))]
use loom::sync::MutexGuard;
#[cfg(all(test, feature = "loom"))]
use loom::sync::atomic::AtomicU64;
#[cfg(all(test, feature = "loom"))]
use loom::sync::atomic::Ordering;
#[cfg(not(all(test, feature = "loom")))]
use parking_lot::Mutex;
#[cfg(not(all(test, feature = "loom")))]
use parking_lot::MutexGuard;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::atomic::AtomicU64;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::atomic::Ordering;
use std::task::Waker;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(all(test, feature = "loom"))]
    {
        mutex.lock().unwrap()
    }
    #[cfg(not(all(test, feature = "loom")))]
    {
        mutex.lock()
    }
}

/// The outcome of a consumer's attempt to idle-park.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParkDecision {
    /// The caller must return `Poll::Pending`.
    Parked,
    /// Work raced with the park transition; the caller must rescan.
    Rescan,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct ScanToken(u64);

#[derive(Debug)]
pub(crate) struct VpActor {
    sequence: AtomicU64,
    vcpu: Mutex<Option<u64>>,
    park: Mutex<Option<Waker>>,
}

impl VpActor {
    pub(crate) fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            vcpu: Mutex::new(None),
            park: Mutex::new(None),
        }
    }

    pub(crate) fn set_vcpu(&self, vcpu: u64) {
        *lock(&self.vcpu) = Some(vcpu);
    }

    /// Serializes vCPU destruction and replacement against concurrent exits.
    pub(crate) fn replace_vcpu<T, E>(
        &self,
        replace: impl FnOnce() -> Result<(u64, T), E>,
    ) -> Result<T, E> {
        let mut published = lock(&self.vcpu);
        *published = None;
        let (vcpu, value) = replace()?;
        *published = Some(vcpu);
        Ok(value)
    }

    /// Unpublishes and destroys the vCPU while excluding concurrent exits.
    pub(crate) fn remove_vcpu<E>(&self, remove: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        let mut published = lock(&self.vcpu);
        *published = None;
        lock(&self.park).take();
        remove()
    }

    pub(crate) fn try_cancel_run(&self) -> Result<(), abi::HvfError> {
        let vcpu = lock(&self.vcpu);
        if let Some(vcpu) = *vcpu {
            // SAFETY: `&vcpu` points to a list of vcpu ids of length 1.
            unsafe { abi::hv_vcpus_exit(&vcpu, 1) }.chk()?;
        }
        Ok(())
    }

    pub(crate) fn cancel_run(&self) {
        if let Err(err) = self.try_cancel_run() {
            tracing::error!(?err, "failed to force vcpu exit");
        }
    }

    /// Notifies the vCPU after work has been published.
    pub(crate) fn notify(&self) {
        self.sequence.fetch_add(1, Ordering::Release);
        self.cancel_run();

        let waker = lock(&self.park).take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Captures the notification sequence before scanning persistent work.
    pub(crate) fn begin_scan(&self) -> ScanToken {
        ScanToken(self.sequence.load(Ordering::Acquire))
    }

    fn clear_park(&self, waker: &Waker) {
        let mut park = lock(&self.park);
        if park
            .as_ref()
            .is_some_and(|registered| registered.will_wake(waker))
        {
            *park = None;
        }
    }

    /// Attempts to park after scanning persistent work and rechecking the
    /// caller's immediate idle predicate.
    pub(crate) fn try_park(
        &self,
        scan: ScanToken,
        waker: &Waker,
        still_idle: impl FnOnce() -> bool,
    ) -> ParkDecision {
        *lock(&self.park) = Some(waker.clone());

        let idle = still_idle();
        let unchanged = self.sequence.load(Ordering::Acquire) == scan.0;
        if idle && unchanged {
            ParkDecision::Parked
        } else {
            self.clear_park(waker);
            ParkDecision::Rescan
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::task::Wake;

    /// A test waker that records how many times it was fired.
    struct CountingWaker(AtomicU32);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counting() -> (Waker, Arc<CountingWaker>) {
        let inner = Arc::new(CountingWaker(AtomicU32::new(0)));
        (Waker::from(inner.clone()), inner)
    }

    fn fired(w: &Arc<CountingWaker>) -> u32 {
        w.0.load(Ordering::Relaxed)
    }

    #[test]
    fn parks_cleanly_when_idle() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        assert_eq!(a.try_park(scan, &waker, || true), ParkDecision::Parked);
        assert_eq!(fired(&count), 0);
    }

    #[test]
    fn rescans_when_work_present() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        assert_eq!(a.try_park(scan, &waker, || false), ParkDecision::Rescan);
        a.notify();
        assert_eq!(fired(&count), 0);
    }

    #[test]
    fn notify_wakes_a_parked_vcpu() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        assert_eq!(a.try_park(scan, &waker, || true), ParkDecision::Parked);
        a.notify();
        assert_eq!(fired(&count), 1);
    }

    #[test]
    fn notification_before_park_forces_rescan() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        a.notify();
        assert_eq!(a.try_park(scan, &waker, || true), ParkDecision::Rescan);
        assert_eq!(fired(&count), 0);
    }

    #[test]
    fn notify_during_idle_recheck_forces_rescan() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        assert_eq!(
            a.try_park(scan, &waker, || {
                a.notify();
                true
            }),
            ParkDecision::Rescan
        );
        assert_eq!(fired(&count), 1);
    }

    #[test]
    fn double_notify_on_parked_wakes_once() {
        let a = VpActor::new();
        let (waker, count) = counting();
        let scan = a.begin_scan();

        assert_eq!(a.try_park(scan, &waker, || true), ParkDecision::Parked);
        a.notify();
        a.notify();
        assert_eq!(fired(&count), 1);
    }

    #[test]
    fn re_park_after_external_wake_uses_the_fresh_waker() {
        let a = VpActor::new();
        let (w1, c1) = counting();
        let scan = a.begin_scan();

        assert_eq!(a.try_park(scan, &w1, || true), ParkDecision::Parked);
        w1.wake_by_ref();
        assert_eq!(fired(&c1), 1);

        let (w2, c2) = counting();
        let scan = a.begin_scan();
        assert_eq!(a.try_park(scan, &w2, || true), ParkDecision::Parked);
        a.notify();

        assert_eq!(fired(&c2), 1);
        assert_eq!(fired(&c1), 1);
    }

    #[test]
    fn remove_vcpu_clears_parked_waker() {
        let actor = VpActor::new();
        let (waker, _) = counting();
        let scan = actor.begin_scan();

        assert_eq!(actor.try_park(scan, &waker, || true), ParkDecision::Parked);
        actor.remove_vcpu(|| Ok::<_, ()>(())).unwrap();

        assert!(lock(&actor.park).is_none());
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::sync::atomic::AtomicBool;
    use loom::thread;
    use std::sync::Arc as StdArc;
    use std::task::Wake;

    struct ModelWaker;

    impl Wake for ModelWaker {
        fn wake(self: StdArc<Self>) {}
    }

    #[test]
    fn loom_notification_cannot_leave_published_work_parked() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let actor = Arc::new(VpActor::new());
            let work = Arc::new(AtomicBool::new(false));

            let consumer = {
                let actor = actor.clone();
                let work = work.clone();
                thread::spawn(move || {
                    let waker = Waker::from(StdArc::new(ModelWaker));
                    let scan = actor.begin_scan();
                    if !work.load(Ordering::Relaxed) {
                        // Model a latch-only work source such as a SynIC
                        // message: the run loop scanned it before parking, but
                        // the final `still_idle` closure does not re-read it.
                        if actor.try_park(scan, &waker, || true) == ParkDecision::Rescan {
                            // Observing the changed sequence must also publish
                            // the producer's preceding work store.
                            assert!(work.load(Ordering::Relaxed));
                        }
                    }
                })
            };
            let producer = {
                let actor = actor.clone();
                let work = work.clone();
                thread::spawn(move || {
                    work.store(true, Ordering::Relaxed);
                    actor.notify();
                })
            };

            consumer.join().unwrap();
            producer.join().unwrap();
            assert!(work.load(Ordering::Relaxed));
            assert!(lock(&actor.park).is_none());
        });
    }
}
