// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An executor backed by IO completion ports.

#![expect(clippy::undocumented_unsafe_blocks)]

use super::overlapped::IoOverlapped;
use super::overlapped::OverlappedIoDriver;
use super::socket::AfdHandle;
use super::socket::AfdSocketReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::io_pool::IoBackend;
use crate::io_pool::IoDriver;
use crate::io_pool::IoPool;
use crate::socket::PollSocketReady;
use crate::socket::SocketReadyDriver;
use crate::sys::overlapped::overlapped_io_done;
use crate::timer::Instant;
use crate::timer::PollTimer;
use crate::timer::TimerDriver;
use crate::timer::TimerQueue;
use crate::timer::TimerQueueId;
use crate::timer::TimerResult;
use crate::wait::PollWait;
use crate::wait::WaitDriver;
use crate::waker::WakerList;
use futures::FutureExt;
use futures::task::ArcWake;
use futures::task::waker_ref;
use once_cell::sync::OnceCell;
use pal::windows::IoCompletionPort;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use pal::windows::WaitPacket;
use pal::windows::afd;
use pal::windows::disassociate_completion_port;
use pal::windows::set_file_completion_notification_modes;
use parking_lot::Mutex;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::windows::prelude::*;
use std::pin::pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use winapi::um::winbase::FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;
use winapi::um::winbase::FILE_SKIP_SET_EVENT_ON_HANDLE;

/// A single-threaded task pool backed by an IO completion port.
pub type IocpPool = IoPool<IocpBackend>;

/// A driver to spawn tasks and IO objects on [`IocpPool`].
pub type IocpDriver = IoDriver<IocpBackend>;

#[derive(Debug)]
pub struct IocpBackend {
    port: IoCompletionPort,
    // TODO: switch to std::sync::OnceLock once `get_or_try_init` is stable
    afd_file: OnceCell<File>,
    state: Mutex<IocpState>,
}

impl Default for IocpBackend {
    fn default() -> Self {
        Self {
            port: IoCompletionPort::new(),
            afd_file: OnceCell::new(),
            state: Mutex::new(IocpState {
                state: PoolState::Running,
                timers: TimerQueue::default(),
            }),
        }
    }
}

#[derive(Debug)]
struct IocpState {
    state: PoolState,
    timers: TimerQueue,
}

#[derive(Debug)]
enum PoolState {
    Running,
    Woken,
    Sleeping(Option<Instant>),
}

impl PoolState {
    fn reset(&mut self) {
        match self {
            PoolState::Running => {}
            PoolState::Woken => {}
            PoolState::Sleeping(_) => unreachable!(),
        }
        *self = PoolState::Running;
    }

    /// Returns true if a wakeup must be posted.
    #[must_use]
    fn wake(&mut self) -> bool {
        let signal = match self {
            PoolState::Running => false,
            PoolState::Woken => false,
            PoolState::Sleeping(_) => true,
        };
        *self = PoolState::Woken;
        signal
    }

    fn can_sleep(&self) -> bool {
        match self {
            PoolState::Running => true,
            PoolState::Woken => false,
            PoolState::Sleeping(_) => unreachable!(),
        }
    }

    fn sleep(&mut self, deadline: Option<Instant>) {
        match self {
            PoolState::Running => {}
            PoolState::Woken => unreachable!(),
            PoolState::Sleeping(_) => unreachable!(),
        }
        *self = PoolState::Sleeping(deadline);
    }

    /// Returns true if a wakeup must be posted.
    #[must_use]
    fn wake_for_timer(&mut self, deadline: Instant) -> bool {
        match self {
            PoolState::Running => false,
            PoolState::Woken => false,
            &mut PoolState::Sleeping(Some(current_deadline)) if current_deadline <= deadline => {
                false
            }
            PoolState::Sleeping(_) => {
                *self = PoolState::Woken;
                true
            }
        }
    }
}

const KEY_WAKEUP: usize = 0;
const KEY_WAIT_COMPLETE: usize = 1;
const KEY_AFD: usize = 2;
const KEY_FILE: usize = 3;

impl IoBackend for IocpBackend {
    fn name() -> &'static str {
        "iocp"
    }

    fn run<Fut: Future>(self: &Arc<Self>, fut: Fut) -> Fut::Output {
        let waker = waker_ref(self);
        let mut cx = Context::from_waker(&waker);
        let mut wakers = WakerList::default();
        let mut fut = pin!(fut);

        let mut state = self.state.lock();
        loop {
            state.state.reset();

            // Wake timers.
            state.timers.wake_expired(&mut wakers);
            // Drop the lock before calling wakers.
            drop(state);
            wakers.wake();

            match fut.poll_unpin(&mut cx) {
                Poll::Ready(r) => break r,
                Poll::Pending => {}
            }

            state = self.state.lock();

            if state.state.can_sleep() {
                let deadline = state.timers.next_deadline();
                state.state.sleep(deadline);
                drop(state);

                let timeout = deadline.map(|deadline| {
                    let now = Instant::now();
                    deadline.max(now) - now
                });

                // Get the next entries from the port.
                let mut entries = [Default::default(); 16];
                let n = self.port.get(&mut entries, timeout);

                // Block unnecessary wakeups.
                let _ = self.state.lock().state.wake();

                // Process the entries.
                for entry in &entries[..n] {
                    match entry.lpCompletionKey {
                        KEY_WAKEUP => {}
                        KEY_WAIT_COMPLETE => {
                            let wait =
                                unsafe { Arc::from_raw(entry.lpOverlapped.cast::<WaitOp>()) };
                            wait.wait_complete(&mut wakers);
                        }
                        KEY_AFD => unsafe {
                            AfdSocketReady::io_complete(
                                self.as_ref(),
                                entry.lpOverlapped,
                                &mut wakers,
                            );
                        },
                        KEY_FILE => unsafe {
                            overlapped_io_done(entry.lpOverlapped, &mut wakers);
                        },
                        key => panic!("unknown key {:#x}", key),
                    }
                }

                state = self.state.lock();
            }
        }
    }
}

impl IocpBackend {
    fn post_wake(&self) {
        self.port.post(0, KEY_WAKEUP, std::ptr::null_mut());
    }
}

impl ArcWake for IocpBackend {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let post = arc_self.state.lock().state.wake();
        if post {
            arc_self.post_wake();
        }
    }
}

impl WaitDriver for IocpDriver {
    type Wait = Wait;

    fn new_wait(&self, handle: RawHandle) -> io::Result<Self::Wait> {
        let op = Arc::new(WaitOp {
            inner: Mutex::new(WaitInner {
                state: WaitState::NotWaiting,
            }),
        });
        Ok(Wait {
            iocp: self.inner.clone(),
            packet: WaitPacket::new()?,
            handle: SendSyncRawHandle(handle),
            op,
        })
    }
}

impl WaitDriver for Arc<IocpDriver> {
    type Wait = Wait;

    fn new_wait(&self, handle: RawHandle) -> io::Result<Self::Wait> {
        self.as_ref().new_wait(handle)
    }
}

#[derive(Debug)]
pub struct Wait {
    iocp: Arc<IocpBackend>,
    packet: WaitPacket,
    handle: SendSyncRawHandle,
    op: Arc<WaitOp>,
}

#[derive(Debug)]
struct WaitOp {
    inner: Mutex<WaitInner>,
}

#[derive(Debug)]
struct WaitInner {
    state: WaitState,
}

#[derive(Debug)]
enum WaitState {
    NotWaiting,
    Signaled,
    Waiting(Waker),
}

impl PollWait for Wait {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.op.inner.lock();
        match &mut inner.state {
            WaitState::Signaled => {
                inner.state = WaitState::NotWaiting;
                Poll::Ready(Ok(()))
            }
            WaitState::NotWaiting => {
                // SAFETY: handle is valid
                unsafe {
                    self.packet.associate(
                        &self.iocp.port,
                        self.handle.0,
                        KEY_WAIT_COMPLETE,
                        Arc::into_raw(self.op.clone()) as usize,
                        0,
                        0,
                    );
                }
                inner.state = WaitState::Waiting(cx.waker().clone());
                Poll::Pending
            }
            WaitState::Waiting(waker) => {
                if !waker.will_wake(cx.waker()) {
                    let _old = std::mem::replace(waker, cx.waker().clone());
                    drop(inner);
                }
                Poll::Pending
            }
        }
    }

    fn poll_cancel_wait(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        let mut inner = self.op.inner.lock();
        match &mut inner.state {
            WaitState::Waiting(waker) => {
                if self.packet.cancel(false) {
                    // SAFETY: releasing the reference acquired in poll_wait().
                    drop(unsafe { Arc::from_raw(Arc::as_ptr(&self.op)) });
                    Poll::Ready(false)
                } else {
                    if !waker.will_wake(cx.waker()) {
                        let _old = std::mem::replace(waker, cx.waker().clone());
                        drop(inner);
                    }
                    Poll::Pending
                }
            }
            WaitState::NotWaiting => Poll::Ready(false),
            WaitState::Signaled => {
                inner.state = WaitState::NotWaiting;
                Poll::Ready(true)
            }
        }
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let inner = self.op.inner.lock();
        match inner.state {
            WaitState::Waiting(_) => {
                // Cancel the wait, removing it from the completion queue if
                // it's already there. Note that this may discard the result of
                // a wait.
                if self.packet.cancel(true) {
                    // SAFETY: releasing the reference acquired in poll_wait().
                    drop(unsafe { Arc::from_raw(Arc::as_ptr(&self.op)) });
                }
            }
            WaitState::NotWaiting | WaitState::Signaled => {}
        }
    }
}

impl WaitOp {
    fn wait_complete(self: Arc<Self>, wakers: &mut WakerList) {
        let mut inner = self.inner.lock();
        match std::mem::replace(&mut inner.state, WaitState::Signaled) {
            WaitState::NotWaiting | WaitState::Signaled => unreachable!(),
            WaitState::Waiting(waker) => {
                drop(inner);
                wakers.push(waker);
            }
        }
    }
}

#[derive(Debug)]
pub struct SocketReady {
    iocp: Arc<IocpBackend>,
    op: AfdSocketReady,
}

impl AfdHandle for IocpBackend {
    fn handle(&self) -> RawHandle {
        self.afd_file
            .get()
            .expect("initialized in new_socket_ready")
            .as_raw_handle()
    }

    fn ref_io(&self) -> RawHandle {
        self.handle()
    }

    unsafe fn deref_io(&self) {}
}

impl SocketReadyDriver for IocpDriver {
    type SocketReady = SocketReady;

    fn new_socket_ready(&self, socket: RawSocket) -> io::Result<Self::SocketReady> {
        // Defer opening the afd file until it's needed since initializing it
        // has non-zero cost and winsock may not be available for this process.
        self.inner.afd_file.get_or_try_init(|| -> io::Result<_> {
            let file = afd::open_afd()?;
            // SAFETY: handle is valid
            unsafe {
                self.inner.port.associate(file.as_raw_handle(), KEY_AFD)?;
            }
            // SAFETY: file is owned, and we are prepared to get only pending IO
            // completions via the completion port.
            unsafe {
                set_file_completion_notification_modes(
                    file.as_raw_handle(),
                    FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
                )?;
            }
            Ok(file)
        })?;

        Ok(SocketReady {
            iocp: self.inner.clone(),
            op: AfdSocketReady::new(socket),
        })
    }
}

impl SocketReadyDriver for Arc<IocpDriver> {
    type SocketReady = SocketReady;

    fn new_socket_ready(&self, socket: RawSocket) -> io::Result<Self::SocketReady> {
        self.as_ref().new_socket_ready(socket)
    }
}

impl Drop for SocketReady {
    fn drop(&mut self) {
        self.op.teardown(self.iocp.as_ref());
    }
}

impl PollSocketReady for SocketReady {
    fn poll_socket_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        self.op
            .poll_socket_ready(cx, self.iocp.as_ref(), slot, events)
    }

    fn clear_socket_ready(&mut self, slot: InterestSlot) {
        self.op.clear_socket_ready(slot)
    }
}

#[derive(Debug)]
pub struct Timer {
    iocp: Arc<IocpBackend>,
    id: TimerQueueId,
}

impl TimerDriver for IocpDriver {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        let id = self.inner.state.lock().timers.add();
        Timer {
            iocp: self.inner.clone(),
            id,
        }
    }
}

impl TimerDriver for Arc<IocpDriver> {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        self.as_ref().new_timer()
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _waker = self.iocp.state.lock().timers.remove(self.id);
    }
}

impl PollTimer for Timer {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        let mut state = self.iocp.state.lock();
        if let Some(deadline) = deadline {
            state.timers.set_deadline(self.id, deadline);
        }
        match state.timers.poll_deadline(cx, self.id) {
            TimerResult::TimedOut(now) => Poll::Ready(now),
            TimerResult::Pending(deadline) => {
                if state.state.wake_for_timer(deadline) {
                    // No need to hold the lock across posting to the completion port.
                    drop(state);
                    self.iocp.post_wake();
                }
                Poll::Pending
            }
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        let mut state = self.iocp.state.lock();
        if state.timers.set_deadline(self.id, deadline) && state.state.wake_for_timer(deadline) {
            // No need to hold the lock across posting to the completion port.
            drop(state);
            self.iocp.post_wake();
        }
    }
}

#[derive(Debug)]
pub struct OverlappedIo {
    _iocp: Arc<IocpBackend>,
    handle: SendSyncRawHandle,
}

impl OverlappedIoDriver for IocpDriver {
    type OverlappedIo = OverlappedIo;

    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo> {
        // SAFETY: handle is valid
        unsafe {
            self.inner.port.associate(handle, KEY_FILE)?;
        }

        // SAFETY: the caller guarantees handle is owned, and we are prepared to
        // get only pending IO completions via the completion port.
        unsafe {
            set_file_completion_notification_modes(
                handle,
                FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
            )?;
        }

        Ok(OverlappedIo {
            _iocp: self.inner.clone(),
            handle: SendSyncRawHandle(handle),
        })
    }
}

impl OverlappedIoDriver for Arc<IocpDriver> {
    type OverlappedIo = OverlappedIo;

    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo> {
        // SAFETY: handle is valid
        unsafe { self.as_ref().new_overlapped_file(handle) }
    }
}

impl IoOverlapped for OverlappedIo {
    fn pre_io(&self) {}

    unsafe fn post_io(&self, _completed: bool, _overlapped: &Overlapped) {}

    unsafe fn disassociate(&mut self) {
        // SAFETY: the caller guarantees the handle is still owned and that no
        // further IO will be issued.
        unsafe {
            disassociate_completion_port(self.handle.0)
                .expect("caller bug: no pending IO should be in flight");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IocpPool;
    use crate::executor_tests;

    #[test]
    fn waker_works() {
        IocpPool::run_with(|_| executor_tests::waker_tests())
    }

    #[test]
    fn spawn_works() {
        executor_tests::spawn_tests(|| {
            let pool = IocpPool::new();
            (pool.driver(), move || pool.run())
        })
    }

    #[test]
    fn sleep_works() {
        IocpPool::run_with(executor_tests::sleep_tests)
    }

    #[test]
    fn wait_works() {
        IocpPool::run_with(executor_tests::wait_tests)
    }

    #[test]
    fn socket_works() {
        IocpPool::run_with(executor_tests::socket_tests)
    }

    #[test]
    fn overlapped_file_works() {
        IocpPool::run_with(executor_tests::windows::overlapped_file_tests)
    }
}
