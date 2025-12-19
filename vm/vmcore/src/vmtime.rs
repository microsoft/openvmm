// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for VM time.
//!
//! This is a VM-specific timeline, which monotonically increases when and only
//! when the VM is running. This module provides types used to access this time
//! and to wait for it to reach target times. This can be used in device
//! emulators to implement VM timers.
//!
//! This is related to the idea of the hypervisor reference time, but it is not
//! guaranteed to be the same value (and is likely not, except when the
//! hypervisor reference time is emulated using VM time).
//!
//! The root of VM time keeping is the [`VmTimeKeeper`]. It manages a clock that
//! can be shared via use of [`VmTimeAccess`] objects. Internally, this clock is
//! based on an offset from the OS's monotonic clock while the VM is running, and
//! a fixed time when the VM is not running.
//!
//! The infrastructure here supports access of VM time across multiple processes
//! in the same OS (but not across machines, virtual or physical). See the
//! comments on [`VmTimeSourceBuilder`] for more information.

pub use saved_state::SavedState;

use futures::StreamExt;
use futures::future::join_all;
use futures_concurrency::future::Race;
use futures_concurrency::stream::Merge;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::driver::Driver;
use pal_async::driver::PollImpl;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use pal_async::timer::Instant;
use pal_async::timer::PollTimer;
use parking_lot::RwLock;
use save_restore_derive::SavedStateRoot;
use slab::Slab;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use thiserror::Error;

/// Roughly analogous to [`std::time::Instant`], but for VM time.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[inspect(transparent)]
#[mesh(transparent)]
pub struct VmTime(#[inspect(hex)] u64);

impl VmTime {
    /// Converts from a time in 100ns units.
    pub fn from_100ns(n: u64) -> Self {
        Self(n)
    }

    /// Gets the time from VM boot (or some other origin) in 100ns units.
    pub const fn as_100ns(&self) -> u64 {
        self.0
    }

    /// Adds `d` to the time.
    pub fn wrapping_add(self, d: Duration) -> Self {
        Self((self.0 as u128).wrapping_add(d.as_nanos() / 100) as u64)
    }

    /// Returns whether `self` is before `t`.
    ///
    /// Note that this is a relative comparison in the 64-bit space and is not
    /// transitive: if `a` is before `b`, and `b` is before `c`, `a` still may
    /// be after `c`.
    pub fn is_before(self, t: Self) -> bool {
        let delta = self.0.wrapping_sub(t.0);
        (delta as i64) < 0
    }

    /// Returns whether `self` is after `t`.
    ///
    /// See the comment about transitivity in [`Self::is_before`].
    pub fn is_after(self, t: Self) -> bool {
        let delta = self.0.wrapping_sub(t.0);
        (delta as i64) > 0
    }

    /// Returns the time between `self` and `t`, returning `None` if `self` is
    /// before `t`.
    pub fn checked_sub(self, t: Self) -> Option<Duration> {
        let delta = self.0.wrapping_sub(t.0);
        if (delta as i64) >= 0 {
            Some(duration_from_100ns(delta))
        } else {
            None
        }
    }

    /// Returns `self` or `t`, whichever is earlier.
    pub fn min(self, t: Self) -> Self {
        if self.is_before(t) { self } else { t }
    }

    /// Returns `self` or `t`, whichever is later.
    pub fn max(self, t: Self) -> Self {
        if self.is_before(t) { t } else { self }
    }
}

fn duration_from_100ns(n: u64) -> Duration {
    const NUM_100NS_IN_SEC: u64 = 10 * 1000 * 1000;
    Duration::new(n / NUM_100NS_IN_SEC, (n % NUM_100NS_IN_SEC) as u32 * 100)
}

#[derive(Inspect)]
struct TimerState {
    time: TimeState,
    #[inspect(skip)]
    timer: PollImpl<dyn PollTimer>,
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|(_, w)| (&w.name, w)))")]
    waiters: Slab<WaiterState>,
    next: Option<VmTime>,
    last: VmTime,
}

#[derive(Debug, Inspect)]
struct WaiterState {
    #[inspect(skip)] // used as a key
    name: Arc<str>,
    next: Option<VmTime>,
    #[inspect(rename = "waiting", with = "Option::is_some")]
    waker: Option<Waker>,
}

impl WaiterState {
    fn new(name: Arc<str>) -> Self {
        Self {
            name,
            next: None,
            waker: None,
        }
    }
}

#[derive(Copy, Clone, Debug, Protobuf)]
struct Timestamp {
    vmtime: VmTime,
    os_time: u64, // Instant::as_nanos()
}

impl Timestamp {
    fn new(vmtime: VmTime, os_time: Instant) -> Self {
        Self {
            vmtime,
            os_time: os_time.as_nanos(),
        }
    }

    fn os_time(&self) -> Instant {
        Instant::from_nanos(self.os_time)
    }
}

impl TimerState {
    fn new(driver: &impl Driver, uptime: VmTime) -> Self {
        Self {
            time: TimeState::Stopped(uptime),
            timer: driver.new_dyn_timer(),
            waiters: Slab::new(),
            next: None,
            last: uptime,
        }
    }

    /// Starts the timer.
    fn start(&mut self, now: Timestamp) {
        let vmtime = self.time.stop_time().expect("should be stopped");
        assert_eq!(now.vmtime, vmtime);
        self.time = TimeState::Started(now);
        tracing::trace!(?now, "vmtime start");
        self.wake(now);
    }

    /// Stops the timer.
    fn stop(&mut self, now_os: Instant) -> VmTime {
        assert!(self.time.is_started());
        let now = self.now(now_os);
        self.time = TimeState::Stopped(now.vmtime);
        tracing::debug!(?now, "vmtime stop");
        now.vmtime
    }

    /// Resets the current time to `time`.
    fn reset(&mut self, time: VmTime) {
        assert!(!self.time.is_started());
        self.time = TimeState::Stopped(time);
        self.last = time;
        self.next = None;
        // Wake all the wakers to re-evaluate things.
        for (_, waiter) in &mut self.waiters {
            if let Some(waker) = waiter.waker.take() {
                waker.wake();
            }
        }
    }

    /// Returns the timestamp corresponding to the given VM time.
    ///
    /// If the VM time is before the last start time, then the timestamp is at
    /// the host time when the VM last started.
    fn timestamp(&self, time: VmTime) -> Option<Timestamp> {
        let start_time = self.time.start_time()?;
        let since = time
            .checked_sub(start_time.vmtime)
            .unwrap_or(Duration::ZERO);
        Some(Timestamp::new(time, start_time.os_time() + since))
    }

    /// Returns the current guest time given a host time.
    fn now(&self, now_os: Instant) -> Timestamp {
        self.time.now(now_os)
    }

    fn set_next(&mut self, next: VmTime) {
        if !self.time.is_started() {
            return;
        }
        if self
            .next
            .is_none_or(|current_next| next.is_before(current_next))
        {
            let deadline = self.timestamp(next).unwrap().os_time();
            tracing::trace!(?deadline, "updating deadline");
            self.timer.set_deadline(deadline);
            self.next = Some(next);
        }
    }

    fn wake(&mut self, now: Timestamp) {
        assert!(!now.vmtime.is_before(self.last));
        self.last = now.vmtime;
        let mut next = None;
        for (_, state) in &mut self.waiters {
            if let Some(this_next) = state.next {
                if this_next.is_after(now.vmtime) {
                    if next.is_none_or(|next| this_next.is_before(next)) {
                        next = Some(this_next);
                    }
                } else if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            }
        }
        if let Some(next) = next {
            self.set_next(next);
        }
    }

    fn cancel_timeout(&mut self, index: usize) {
        self.waiters[index].next = None;
    }

    /// Updates the next timeout for an individual waiter.
    fn update_timeout(&mut self, index: usize, time: VmTime) {
        let state = &mut self.waiters[index];
        tracing::trace!(vmtime = ?time, user = state.name.as_ref(), "timeout update");
        state.next = Some(time);
        if time.is_before(self.last) {
            // The wake time is even before the last timer wake, so just wake
            // the waiter and skip updating the timer.
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
            return;
        }

        // Update the timer if needed.
        if self.next.is_some_and(|next| next.is_before(time)) {
            return;
        }
        self.set_next(time);
    }

    /// Polls a single waiter.
    fn poll_timeout(
        &mut self,
        cx: &mut Context<'_>,
        index: usize,
        now_os: Instant,
        next: Option<VmTime>,
    ) -> Poll<Timestamp> {
        let now = self.now(now_os);
        let state = &mut self.waiters[index];
        if next.is_some_and(|next| next.is_before(now.vmtime)) {
            state.waker = None;
            state.next = None;
            return Poll::Ready(now);
        }
        state.next = next;
        state.waker = Some(cx.waker().clone());
        if let Some(next) = next {
            self.set_next(next);
        }
        Poll::Pending
    }

    /// Polls the timer, waking any waiters whose timeout has expired.
    fn poll(&mut self, cx: &mut Context<'_>) {
        while self.time.is_started() {
            let next = match self.next {
                Some(_) => {
                    // The timer's deadline is already set.
                    tracing::trace!("polling existing deadline");
                    None
                }
                None => {
                    // Set the timer far out in the future.
                    let deadline = Instant::now() + Duration::from_secs(86400);
                    tracing::trace!(?deadline, "polling with long timeout");
                    Some(deadline)
                }
            };
            if let Poll::Ready(now) = self.timer.poll_timer(cx, next) {
                self.next = None;
                self.wake(self.now(now));
            } else {
                return;
            }
        }
    }
}

/// A time keeper, which tracks the current time and all waiters.
#[derive(Debug, InspectMut)]
pub struct VmTimeKeeper {
    #[inspect(skip)]
    _task: Task<()>,
    #[inspect(flatten, send = "KeeperRequest::Inspect")]
    req_send: mesh::Sender<KeeperRequest>,
    #[inspect(skip)]
    builder: VmTimeSourceBuilder,
    #[inspect(skip)]
    time: TimeState,
}

// UNSAFETY: Needed to derive SavedStateRoot in the same crate it is declared
#[expect(unsafe_code)]
mod saved_state {
    use super::*;

    /// Saved state for [`VmTimeKeeper`].
    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "vmtime")]
    pub struct SavedState {
        #[mesh(1)]
        pub(super) vmtime: VmTime,
    }

    impl SavedState {
        /// Create a new instance of `SavedState` from an existing `VmTime`.
        pub fn from_vmtime(vmtime: VmTime) -> Self {
            SavedState { vmtime }
        }
    }
}

#[derive(Debug, MeshPayload, Copy, Clone)]
enum TimeState {
    Stopped(VmTime),
    Started(Timestamp),
}

impl Inspect for TimeState {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        let state = match *self {
            TimeState::Stopped(_time) => "stopped",
            TimeState::Started(time) => {
                resp.field("start_time", time.vmtime);
                "started"
            }
        };
        resp.field("state", state)
            .field("now", self.now(Instant::now()).vmtime);
    }
}

impl TimeState {
    fn is_started(&self) -> bool {
        self.start_time().is_some()
    }

    fn stop_time(&self) -> Option<VmTime> {
        match *self {
            TimeState::Stopped(time) => Some(time),
            TimeState::Started(_) => None,
        }
    }

    fn start_time(&self) -> Option<Timestamp> {
        match *self {
            TimeState::Stopped(_) => None,
            TimeState::Started(time) => Some(time),
        }
    }

    fn now(&self, now_os: Instant) -> Timestamp {
        match *self {
            TimeState::Stopped(time) => Timestamp::new(time, now_os),
            TimeState::Started(start_time) => {
                if now_os >= start_time.os_time() {
                    Timestamp::new(
                        start_time
                            .vmtime
                            .wrapping_add(now_os - start_time.os_time()),
                        now_os,
                    )
                } else {
                    // `now` can be before `running.start_host` if it was captured
                    // outside the lock and raced with the call to `start()`. Treat
                    // this as `now` being the same as `start_host`.
                    //
                    // But if `now` is too much before `running.start_host`, then
                    // there is probably some serious OS timekeeping bug, or maybe
                    // the debugger broke in at just the wrong time.
                    let delta = start_time.os_time() - now_os;
                    if delta > Duration::from_secs(1) {
                        tracing::error!(
                            now = now_os.as_nanos(),
                            start_host = start_time.os_time().as_nanos(),
                            ?delta,
                            "time went backward"
                        );
                    }
                    start_time
                }
            }
        }
    }
}

impl VmTimeKeeper {
    /// Creates a new time keeper with the specified current guest time.
    pub fn new(driver: &impl SpawnDriver, uptime: VmTime) -> Self {
        let (new_send, new_recv) = mesh::mpsc_channel();
        let (req_send, req_recv) = mesh::channel();
        let time = TimeState::Stopped(uptime);
        let task = driver.spawn("vm-time-keeper", async move {
            let mut primary = PrimaryKeeper {
                req_recv,
                new_recv,
                keepers: Vec::new(),
                next_id: 0,
                time,
            };
            primary.run().await;
        });
        Self {
            time,
            req_send,
            builder: VmTimeSourceBuilder { new_send },
            _task: task,
        }
    }

    /// Saves the time state.
    pub fn save(&self) -> SavedState {
        SavedState {
            vmtime: self.time.stop_time().expect("should be stopped"),
        }
    }

    /// Restores the time state.
    pub async fn restore(&mut self, state: SavedState) {
        let SavedState { vmtime } = state;
        self.reset_to(vmtime).await
    }

    async fn reset_to(&mut self, vmtime: VmTime) {
        assert!(!self.time.is_started(), "should be stopped");
        self.time = TimeState::Stopped(vmtime);
        self.req_send
            .call(KeeperRequest::Reset, vmtime)
            .await
            .unwrap();
    }

    /// Reset the VM time to 0.
    pub async fn reset(&mut self) {
        self.reset_to(VmTime::from_100ns(0)).await
    }

    /// Starts the timer, so that the current time will increase.
    pub async fn start(&mut self) {
        let vmtime = self.time.stop_time().expect("should be stopped");
        let timestamp = Timestamp::new(vmtime, Instant::now());
        self.time = TimeState::Started(timestamp);
        self.req_send
            .call(KeeperRequest::Start, timestamp)
            .await
            .unwrap();
    }

    /// Stops the timer, so that the current time will stop increasing.
    pub async fn stop(&mut self) {
        assert!(self.time.is_started(), "should be running");
        let stop_time = self.req_send.call(KeeperRequest::Stop, ()).await.unwrap();
        self.time = TimeState::Stopped(stop_time);
    }

    /// Returns a time source builder, which can be used to spawn tasks that
    /// back [`VmTimeSource`] instances, all backed by this time keeper's clock.
    pub fn builder(&self) -> &VmTimeSourceBuilder {
        &self.builder
    }
}

/// A time source builder, used to spawn tasks that back [`VmTimeSource`]
/// instances.
///
/// Note that this can be sent across processes via `mesh`.
///
/// However, the time keeping infrastructure assumes that all time keeping tasks
/// share a single global monotonic OS clock. This means that if you send this
/// across a VM/kernel/network boundary, the resulting time sources will not be
/// in sync with each other.
#[derive(MeshPayload, Clone, Debug)]
pub struct VmTimeSourceBuilder {
    new_send: mesh::Sender<NewKeeperRequest>,
}

/// Error returned by [`VmTimeSourceBuilder::build`] when the time keeper has
/// been torn down.
#[derive(Debug, Error)]
#[error("the time keeper has been torn down")]
pub struct TimeKeeperIsGone;

impl VmTimeSourceBuilder {
    /// Builds and spawns a backing task for [`VmTimeSource`]s. All
    /// [`VmTimeSource`] instances cloned from the first one will share a
    /// backing task.
    pub async fn build(&self, driver: &impl SpawnDriver) -> Result<VmTimeSource, TimeKeeperIsGone> {
        let (send, recv) = mesh::channel();
        let time = self
            .new_send
            .call(NewKeeperRequest::New, send)
            .await
            .map_err(|_| TimeKeeperIsGone)?;

        let mut state = Arc::new(RwLock::new(TimerState::new(driver, VmTime::from_100ns(0))));
        // Synchronize the time.
        {
            let state = Arc::get_mut(&mut state).unwrap().get_mut();
            match time {
                TimeState::Stopped(vmtime) => state.reset(vmtime),
                TimeState::Started(timestamp) => state.start(timestamp),
            }
        }
        let mut keeper = SecondaryKeeper {
            state: state.clone(),
            recv,
        };
        driver
            .spawn("vm-time", async move { keeper.run().await })
            .detach();
        Ok(VmTimeSource {
            state,
            remote: self.clone(),
        })
    }
}

/// Task that stores the current time state and manages the list of secondary
/// keepers.
///
/// There is one of these per VM time clock (i.e. one per VM).
#[derive(Inspect)]
struct PrimaryKeeper {
    #[inspect(skip)]
    req_recv: mesh::Receiver<KeeperRequest>,
    #[inspect(skip)]
    new_recv: mesh::Receiver<NewKeeperRequest>,
    #[inspect(
        with = "|x| inspect::iter_by_key(x.iter().map(|(id, sender)| (id, inspect::send(sender, KeeperRequest::Inspect))))"
    )]
    keepers: Vec<(u64, mesh::Sender<KeeperRequest>)>,
    #[inspect(skip)]
    next_id: u64,
    time: TimeState,
}

#[derive(MeshPayload)]
enum KeeperRequest {
    Start(Rpc<Timestamp, ()>),
    Stop(Rpc<(), VmTime>),
    Reset(Rpc<VmTime, ()>),
    Inspect(inspect::Deferred),
}

#[derive(MeshPayload)]
enum NewKeeperRequest {
    New(Rpc<mesh::Sender<KeeperRequest>, TimeState>),
}

impl PrimaryKeeper {
    async fn run(&mut self) {
        enum Event {
            New(NewKeeperRequest),
            Request(KeeperRequest),
        }

        while let Some(event) = (
            (&mut self.new_recv).map(Event::New),
            (&mut self.req_recv).map(Event::Request),
        )
            .merge()
            .next()
            .await
        {
            // Garbage collect the existing keepers.
            self.keepers.retain(|(_, s)| !s.is_closed());
            match event {
                Event::New(req) => match req {
                    NewKeeperRequest::New(rpc) => rpc.handle_sync(|sender| {
                        self.keepers.push((self.next_id, sender));
                        self.next_id += 1;
                        self.time
                    }),
                },
                Event::Request(req) => {
                    match req {
                        KeeperRequest::Start(rpc) => {
                            rpc.handle(async |start_time| {
                                assert!(!self.time.is_started());
                                self.time = TimeState::Started(start_time);
                                join_all(self.keepers.iter().map(|(_, sender)| {
                                    sender.call(KeeperRequest::Start, start_time)
                                }))
                                .await;
                            })
                            .await
                        }
                        KeeperRequest::Stop(rpc) => {
                            rpc.handle(async |()| {
                                let results = join_all(
                                    self.keepers
                                        .iter()
                                        .map(|(_, sender)| sender.call(KeeperRequest::Stop, ())),
                                )
                                .await;

                                let start_time = self.time.start_time().expect("should be running");
                                let now = start_time
                                    .vmtime
                                    .wrapping_add(Instant::now() - start_time.os_time());

                                // Compute the stop time as the max of all stop
                                // times so that no keeper goes backwards next
                                // start.
                                let stop_time = results
                                    .into_iter()
                                    .filter_map(|r| r.ok())
                                    .fold(now, |a, b| a.max(b));

                                self.time = TimeState::Stopped(stop_time);

                                // Update all the keepers with the stop time so that
                                // it's consistent.
                                join_all(self.keepers.iter().map(|(_, sender)| {
                                    sender.call(KeeperRequest::Reset, stop_time)
                                }))
                                .await;

                                stop_time
                            })
                            .await
                        }
                        KeeperRequest::Reset(rpc) => {
                            rpc.handle(async |time| {
                                assert!(!self.time.is_started(), "should not be running");
                                self.time = TimeState::Stopped(time);
                                join_all(
                                    self.keepers
                                        .iter()
                                        .map(|(_, sender)| sender.call(KeeperRequest::Reset, time)),
                                )
                                .await;
                            })
                            .await
                        }
                        KeeperRequest::Inspect(deferred) => deferred.inspect(&self),
                    }
                }
            }
        }
    }
}

/// Task that provides access to the current VM time.
///
/// There can be multiple of these per VM, across multiple processes. They are
/// all backed by the same clock and report the same time.
#[derive(InspectMut)]
struct SecondaryKeeper {
    #[inspect(flatten)]
    state: Arc<RwLock<TimerState>>,
    #[inspect(skip)]
    recv: mesh::Receiver<KeeperRequest>,
}

impl SecondaryKeeper {
    async fn run(&mut self) {
        loop {
            let r = {
                let state = &self.state;
                (
                    self.recv.next(),
                    poll_fn(|cx| {
                        state.write().poll(cx);
                        Poll::Pending
                    }),
                )
                    .race()
                    .await
            };
            match r {
                Some(req) => match req {
                    KeeperRequest::Start(rpc) => rpc.handle_sync(|start_time| {
                        let mut state = self.state.write();
                        state.start(start_time);
                    }),
                    KeeperRequest::Reset(rpc) => rpc.handle_sync(|vmtime| {
                        let mut state = self.state.write();
                        state.reset(vmtime);
                    }),
                    KeeperRequest::Stop(rpc) => rpc.handle_sync(|()| {
                        let mut state = self.state.write();
                        state.stop(Instant::now())
                    }),
                    KeeperRequest::Inspect(deferred) => deferred.inspect(&mut *self),
                },
                None => break,
            }
        }
    }
}

/// A time source, used to instantiate [`VmTimeAccess`].
#[derive(Clone)]
pub struct VmTimeSource {
    state: Arc<RwLock<TimerState>>,
    remote: VmTimeSourceBuilder,
}

impl VmTimeSource {
    /// Gets a time accessor.
    ///
    /// `name` is used for diagnostics via `inspect`.
    pub fn access(&self, name: impl Into<Arc<str>>) -> VmTimeAccess {
        let name = name.into();
        VmTimeAccess {
            timeout: None,
            waiting: false,
            index: self
                .state
                .write()
                .waiters
                .insert(WaiterState::new(name.clone())),
            state: self.state.clone(),
            name,
        }
    }

    /// Gets the builder for creating additional time sources backing tasks
    /// whose times are in sync with this one.
    pub fn builder(&self) -> &VmTimeSourceBuilder {
        &self.remote
    }
}

/// An individual time accessor, used to query and wait for time.
#[derive(Inspect)]
pub struct VmTimeAccess {
    timeout: Option<VmTime>,
    waiting: bool,
    #[inspect(skip)]
    index: usize,
    #[inspect(skip)]
    state: Arc<RwLock<TimerState>>,
    name: Arc<str>,
}

impl Drop for VmTimeAccess {
    fn drop(&mut self) {
        self.state.write().waiters.remove(self.index);
    }
}

impl VmTimeAccess {
    /// Gets the current time.
    pub fn now(&self) -> VmTime {
        let now = Instant::now();
        self.state.read().now(now).vmtime
    }

    /// Returns the host time corresponding to a guest time.
    ///
    /// If the guest time is before the VM last resumed, then returns the time
    /// the VM last resumed.
    ///
    /// If the VM is not running, returns `None`.
    pub fn host_time(&self, time: VmTime) -> Option<Instant> {
        Some(self.state.read().timestamp(time)?.os_time())
    }

    /// Get the currently set timeout.
    pub fn get_timeout(&self) -> Option<VmTime> {
        self.timeout
    }

    /// Sets the timeout [`poll_timeout`](Self::poll_timeout) will return ready.
    pub fn set_timeout(&mut self, time: VmTime) {
        self.timeout = Some(time);
        if self.waiting {
            self.state.write().update_timeout(self.index, time);
        }
    }

    /// Sets the timeout for [`poll_timeout`](Self::poll_timeout) will return ready,
    /// but only if `time` is earlier than the current timeout.
    pub fn set_timeout_if_before(&mut self, time: VmTime) {
        if self.timeout.is_none_or(|timeout| time.is_before(timeout)) {
            self.set_timeout(time);
        }
    }

    /// Clears the current timeout for [`poll_timeout`](Self::poll_timeout).
    pub fn cancel_timeout(&mut self) {
        if self.waiting && self.timeout.is_some() {
            self.state.write().cancel_timeout(self.index);
        }
        self.timeout = None;
    }

    /// Polls the current time against the current timeout.
    ///
    /// Returns `Poll::Ready(self.now())` if the current timeout is before now.
    /// Returns `Poll::Pending` if there is no current timeout, or if the
    /// current timeout is after now.
    ///
    /// Although this takes `&self`, note that it only stores a single waker,
    /// meaning that if you poll this from multiple tasks concurrently, only one
    /// task will be woken when the time elapses. Create another instance of
    /// this type with [`VmTimeSource`] if you need to poll this from multiple
    /// tasks.
    pub fn poll_timeout(&mut self, cx: &mut Context<'_>) -> Poll<VmTime> {
        let now = Instant::now();
        match self
            .state
            .write()
            .poll_timeout(cx, self.index, now, self.timeout)
        {
            Poll::Ready(now) => {
                self.waiting = false;
                self.timeout = None;
                Poll::Ready(now.vmtime)
            }
            Poll::Pending => {
                self.waiting = true;
                Poll::Pending
            }
        }
    }
}

#[derive(Debug, Inspect)]
#[inspect(tag = "state")]
enum VmTimerPeriodicInner {
    Stopped,
    Running {
        last_timeout: VmTime,
        period: Duration,
    },
}

/// An abstraction over [`VmTimeAccess`] that streamlines the process of setting
/// up a periodic timer.
#[derive(Inspect)]
pub struct VmTimerPeriodic {
    vmtime: VmTimeAccess,
    inner: VmTimerPeriodicInner,
}

impl VmTimerPeriodic {
    /// Create a new periodic timer, backed by the given [`VmTimeAccess`].
    pub fn new(vmtime_access: VmTimeAccess) -> Self {
        Self {
            vmtime: vmtime_access,
            inner: VmTimerPeriodicInner::Stopped,
        }
    }

    /// Cancel the timer.
    ///
    /// If the timer isn't running, this method is a no-op.
    pub fn cancel(&mut self) {
        self.vmtime.cancel_timeout();
        self.inner = VmTimerPeriodicInner::Stopped;
    }

    /// Start the timer, configuring it to fire at the specified period.
    ///
    /// If the timer is currently running, the timer will be cancelled +
    /// restarted.
    pub fn start(&mut self, period: Duration) {
        self.cancel();

        let time = self.vmtime.now().wrapping_add(period);
        self.vmtime.set_timeout(time);
        self.inner = VmTimerPeriodicInner::Running {
            last_timeout: time,
            period,
        }
    }

    /// Check if the timer is currently running.
    pub fn is_running(&self) -> bool {
        matches!(self.inner, VmTimerPeriodicInner::Running { .. })
    }

    /// Polls the timer.
    ///
    /// Returns `Poll::Ready(now)` when the timer is past-due, returning
    /// `Poll::Pending` otherwise.
    pub fn poll_timeout(&mut self, cx: &mut Context<'_>) -> Poll<VmTime> {
        match self.inner {
            VmTimerPeriodicInner::Stopped => {
                assert_eq!(self.vmtime.get_timeout(), None);
                // Make sure the waker is still managed properly
                // This is guaranteed to return Pending according to its documentation thanks to the above assert.
                self.vmtime.poll_timeout(cx)
            }
            VmTimerPeriodicInner::Running {
                ref mut last_timeout,
                period,
            } => {
                let mut res = Poll::Pending;
                while let Poll::Ready(now) = self.vmtime.poll_timeout(cx) {
                    res = Poll::Ready(now);

                    let time = last_timeout.wrapping_add(period);
                    self.vmtime.set_timeout(time);
                    *last_timeout = time;
                }
                res
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VmTime;
    use super::VmTimeKeeper;
    use futures::FutureExt;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use std::future::poll_fn;
    use std::time::Duration;

    #[async_test]
    async fn test_vmtime(driver: DefaultDriver) {
        let mut keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut access = keeper
            .builder()
            .build(&driver)
            .await
            .unwrap()
            .access("test");
        keeper.start().await;

        // Test long timeout.
        access.set_timeout(access.now().wrapping_add(Duration::from_secs(1000)));
        let mut timer = PolledTimer::new(&driver);
        futures::select! {
            _ = timer.sleep(Duration::from_millis(50)).fuse() => {}
            _ = poll_fn(|cx| access.poll_timeout(cx)).fuse() => panic!("unexpected wait completion"),
        }

        // Test short timeout.
        let deadline = access.now().wrapping_add(Duration::from_millis(10));
        access.set_timeout(deadline);
        futures::select! {
            _ = timer.sleep(Duration::from_millis(1000)).fuse() => panic!("unexpected timeout"),
            now = poll_fn(|cx| access.poll_timeout(cx)).fuse() => {
                assert!(now.is_after(deadline));
            }
        }
        // Timeout should be cleared by the successful poll.
        assert!(
            poll_fn(|cx| access.poll_timeout(cx))
                .now_or_never()
                .is_none()
        );

        // Test changing timeout.
        let now = access.now();
        let deadline = now.wrapping_add(Duration::from_millis(2000));
        access.set_timeout(deadline);
        futures::select! {
            _ = timer.sleep(Duration::from_millis(30)).fuse() => {
                let deadline = now.wrapping_add(Duration::from_millis(50));
                access.set_timeout(deadline);
                futures::select! {
                    _ = timer.sleep(Duration::from_millis(1000)).fuse() => panic!("unexpected timeout"),
                    now = poll_fn(|cx| access.poll_timeout(cx)).fuse() => {
                        assert!(now.is_after(deadline));
                    }
                }
            }
            _ = poll_fn(|cx| access.poll_timeout(cx)).fuse() => panic!("unexpected wait completion"),
        }
        keeper.stop().await;
    }

    #[async_test]
    async fn test_multi_vmtime(driver: DefaultDriver) {
        let mut keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let src1 = keeper.builder().build(&driver).await.unwrap();
        keeper.start().await;
        let src2 = src1.builder().build(&driver).await.unwrap();
        let acc1 = src1.access("test");
        let acc2 = src2.access("test");
        {
            let t1 = acc1.now();
            let t2 = acc2.now();
            let t3 = acc1.now();
            assert!(!t2.is_before(t1), "{t1:?} {t2:?}");
            assert!(!t3.is_before(t2), "{t2:?} {t3:?}");
        }
        let now = acc1.now();
        keeper.stop().await;
        let t1 = acc1.now();
        let t2 = acc2.now();
        assert!(!t1.is_before(now));
        assert_eq!(t1, t2);
        let zero = VmTime::from_100ns(0);
        // Even on very fast machines, at least _some_ time will have advanced.
        assert_ne!(t1, zero);
        keeper.reset().await;
        assert_eq!(acc1.now(), zero);
        assert_eq!(acc2.now(), zero);
    }
}
