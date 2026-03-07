// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An executor backed by the Windows thread pool.

#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

use super::overlapped::IoOverlapped;
use super::overlapped::OverlappedIoDriver;
use super::overlapped::overlapped_io_done;
use super::socket::AfdHandle;
use super::socket::AfdSocketReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::socket::PollSocketReady;
use crate::socket::SocketReadyDriver;
use crate::task::Runnable;
use crate::task::Schedule;
use crate::task::Spawn;
use crate::task::TaskMetadata;
use crate::timer::Instant;
use crate::timer::PollTimer;
use crate::timer::TimerDriver;
use crate::wait::PollWait;
use crate::wait::WaitDriver;
use crate::waker::WakerList;
use loan_cell::LoanCell;
use once_cell::sync::OnceCell;
use pal::windows::SendSyncRawHandle;
use pal::windows::afd;
use pal::windows::disassociate_completion_port;
use pal::windows::set_file_completion_notification_modes;
use pal::windows::tp::TpIo;
use pal::windows::tp::TpTimer;
use pal::windows::tp::TpWait;
use pal::windows::tp::TpWork;
use parking_lot::Mutex;
use std::cell::Cell;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::prelude::RawHandle;
use std::os::windows::prelude::*;
use std::ptr::null_mut;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use winapi::um::winbase::FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;
use winapi::um::winbase::FILE_SKIP_SET_EVENT_ON_HANDLE;
use winapi::um::winnt::TP_CALLBACK_INSTANCE;
use winapi::um::winnt::TP_IO;
use winapi::um::winnt::TP_TIMER;
use winapi::um::winnt::TP_WAIT;
use winapi::um::winnt::TP_WORK;

/// A Windows thread pool.
#[derive(Debug, Clone)]
pub struct TpPool(());

impl TpPool {
    /// Returns the default system thread pool.
    pub fn system() -> Self {
        TpPool(())
    }
}

thread_local! {
    static DEFERRED_RUNNABLE: LoanCell<Cell<Option<Runnable>>> = const { LoanCell::new() };
}

fn wake_locally(f: impl FnOnce()) {
    DEFERRED_RUNNABLE.with(|slot| {
        let deferred = Cell::new(None);
        slot.lend(&deferred, f);
        if let Some(runnable) = deferred.into_inner() {
            runnable.schedule();
        }
    })
}

struct TpScheduler {
    work: TpWork,
    next: Arc<NextRunnable>,
    name: Arc<str>,
}

struct NextRunnable(Mutex<Option<Runnable>>);

impl Schedule for TpScheduler {
    fn schedule(&self, runnable: Runnable) {
        DEFERRED_RUNNABLE.with(|slot| {
            slot.borrow(|slot| {
                if let Some(slot) = slot {
                    let old_runnable = slot.replace(Some(runnable));
                    if let Some(runnable) = old_runnable {
                        runnable.schedule();
                    }
                } else {
                    {
                        let mut next = self.next.0.lock();
                        assert!(next.is_none());
                        *next = Some(runnable);
                    }
                    let _ = Arc::into_raw(self.next.clone());
                    self.work.submit();
                }
            })
        })
    }

    fn name(&self) -> Arc<str> {
        self.name.clone()
    }
}

impl Spawn for TpPool {
    fn scheduler(&self, _metadata: &TaskMetadata) -> Arc<dyn Schedule> {
        let next = Arc::new(NextRunnable(Default::default()));
        let work = unsafe {
            TpWork::new(Some(tp_work_callback), Arc::as_ptr(&next) as *mut _)
                .expect("oom allocating work")
        };
        Arc::new(TpScheduler {
            work,
            next,
            name: "tp".to_owned().into(),
        })
    }
}

unsafe extern "system" fn tp_work_callback(
    _: *mut TP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _: *mut TP_WORK,
) {
    // SAFETY: consume reference incremented in schedule().
    let next = unsafe { Arc::from_raw(context as *const NextRunnable) };
    let runnable = next.0.lock().take().unwrap();
    runnable.run();
}

#[derive(Debug)]
pub struct Wait {
    tp_wait: TpWait,
    handle: SendSyncRawHandle,
    inner: Arc<WaitInner>,
}

#[derive(Debug)]
struct WaitInner {
    state: Mutex<WaitState>,
}

#[derive(Debug)]
enum WaitState {
    NotWaiting,
    Signaled,
    Waiting(Waker),
}

impl Drop for Wait {
    fn drop(&mut self) {
        if self.tp_wait.cancel() {
            {
                let mut state = self.inner.state.lock();
                assert!(matches!(&*state, WaitState::Waiting(_)));
                *state = WaitState::NotWaiting;
            }
            // SAFETY: dropping the reference count from poll_wait.
            unsafe {
                drop(Arc::from_raw(Arc::as_ptr(&self.inner)));
            }
        }
    }
}

impl WaitDriver for TpPool {
    type Wait = Wait;

    fn new_wait(&self, handle: RawHandle) -> io::Result<Self::Wait> {
        let inner = Arc::new(WaitInner {
            state: Mutex::new(WaitState::NotWaiting),
        });
        let tp_wait =
            unsafe { TpWait::new(Some(tp_wait_complete), Arc::as_ptr(&inner) as *mut _)? };
        Ok(Wait {
            tp_wait,
            handle: SendSyncRawHandle(handle),
            inner,
        })
    }
}

unsafe extern "system" fn tp_wait_complete(
    _: *mut TP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _: *mut TP_WAIT,
    _: u32,
) {
    // SAFETY: Claiming the reference incremented in poll_wait.
    let inner = unsafe { Arc::from_raw(context as *const WaitInner) };
    let mut state = inner.state.lock();
    match std::mem::replace(&mut *state, WaitState::Signaled) {
        WaitState::NotWaiting | WaitState::Signaled => unreachable!(),
        WaitState::Waiting(waker) => {
            drop(state);
            wake_locally(|| waker.wake());
        }
    }
}

impl PollWait for Wait {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.inner.state.lock();
        match &mut *state {
            WaitState::Signaled => {
                *state = WaitState::NotWaiting;
                Poll::Ready(Ok(()))
            }
            WaitState::NotWaiting => {
                // Increment the reference count.
                let _ = Arc::into_raw(self.inner.clone());
                *state = WaitState::Waiting(cx.waker().clone());
                drop(state);
                // SAFETY: handle is valid
                unsafe {
                    self.tp_wait.set(self.handle.0);
                }
                Poll::Pending
            }
            WaitState::Waiting(waker) => {
                if !waker.will_wake(cx.waker()) {
                    let _old = std::mem::replace(waker, cx.waker().clone());
                    drop(state);
                }
                Poll::Pending
            }
        }
    }

    fn poll_cancel_wait(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        let mut state = self.inner.state.lock();
        match &mut *state {
            WaitState::NotWaiting => Poll::Ready(false),
            WaitState::Signaled => {
                *state = WaitState::NotWaiting;
                Poll::Ready(true)
            }
            WaitState::Waiting(waker) => {
                if self.tp_wait.cancel() {
                    // SAFETY: dropping the reference count from poll_wait.
                    unsafe {
                        drop(Arc::from_raw(Arc::as_ptr(&self.inner)));
                    }
                    Poll::Ready(false)
                } else {
                    if !waker.will_wake(cx.waker()) {
                        let _old = std::mem::replace(waker, cx.waker().clone());
                        drop(state);
                    }
                    Poll::Pending
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct SocketReady {
    op: AfdSocketReady,
}

// TODO: switch to std::sync::OnceLock once `get_or_try_init` is stable
struct TpAfdHandle(OnceCell<(TpIo, File)>);

impl TpAfdHandle {
    fn system() -> &'static Self {
        static SYSTEM: TpAfdHandle = TpAfdHandle(OnceCell::new());
        &SYSTEM
    }

    fn init(&self) -> io::Result<()> {
        self.0.get_or_try_init(|| -> io::Result<_> {
            let file = afd::open_afd()?;
            let tp_io =
                unsafe { TpIo::new(file.as_raw_handle(), Some(tp_afd_io_complete), null_mut())? };
            // SAFETY: file is owned, and we are prepared to get only pending IO
            // completions via the completion port.
            unsafe {
                set_file_completion_notification_modes(
                    file.as_raw_handle(),
                    FILE_SKIP_SET_EVENT_ON_HANDLE | FILE_SKIP_COMPLETION_PORT_ON_SUCCESS,
                )?;
            }

            Ok((tp_io, file))
        })?;
        Ok(())
    }

    fn get(&self) -> &(TpIo, File) {
        self.0.get().expect("initialized in init()")
    }
}

impl AfdHandle for TpAfdHandle {
    fn handle(&self) -> RawHandle {
        self.get().1.as_raw_handle()
    }

    fn ref_io(&self) -> RawHandle {
        let (io, handle) = self.get();
        io.start_io();
        handle.as_raw_handle()
    }

    unsafe fn deref_io(&self) {
        // SAFETY: Caller ensured this is safe.
        unsafe {
            self.get().0.cancel_io();
        }
    }
}

unsafe extern "system" fn tp_afd_io_complete(
    _: *mut TP_CALLBACK_INSTANCE,
    _: *mut c_void,
    overlapped: *mut c_void,
    _: u32,
    _: usize,
    _: *mut TP_IO,
) {
    let mut wakers = WakerList::default();
    // SAFETY: the overlapped IO is complete (and will be considered so only once).
    unsafe { AfdSocketReady::io_complete(TpAfdHandle::system(), overlapped.cast(), &mut wakers) };
    wake_locally(|| wakers.wake());
}

impl SocketReadyDriver for TpPool {
    type SocketReady = SocketReady;

    fn new_socket_ready(&self, socket: RawSocket) -> io::Result<Self::SocketReady> {
        // Defer opening the afd file until it's needed since initializing it
        // has non-zero cost and winsock may not be available for this process.
        TpAfdHandle::system().init()?;
        Ok(SocketReady {
            op: AfdSocketReady::new(socket),
        })
    }
}

impl Drop for SocketReady {
    fn drop(&mut self) {
        self.op.teardown(TpAfdHandle::system());
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
            .poll_socket_ready(cx, TpAfdHandle::system(), slot, events)
    }

    fn clear_socket_ready(&mut self, slot: InterestSlot) {
        self.op.clear_socket_ready(slot)
    }
}

#[derive(Debug)]
pub struct Timer {
    tp_timer: TpTimer,
    deadline: Instant,
    inner: Arc<Mutex<TimerInner>>,
}

#[derive(Debug)]
struct TimerInner {
    waker: Option<Waker>,
}

impl TimerDriver for TpPool {
    type Timer = Timer;

    fn new_timer(&self) -> Self::Timer {
        let inner = Arc::new(Mutex::new(TimerInner { waker: None }));
        let tp_timer = unsafe {
            TpTimer::new(Some(tp_timer_callback), Arc::as_ptr(&inner) as *mut _).unwrap()
        };
        Timer {
            tp_timer,
            inner,
            deadline: Instant::from_nanos(0),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.tp_timer.cancel() {
            // SAFETY: taking reference added in poll_timer().
            unsafe {
                drop(Arc::from_raw(Arc::as_ptr(&self.inner)));
            }
        }
    }
}

unsafe extern "system" fn tp_timer_callback(
    _: *mut TP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _: *mut TP_TIMER,
) {
    let inner = unsafe { Arc::from_raw(context as *const Mutex<TimerInner>) };
    wake_locally(|| {
        let waker = inner.lock().waker.take();
        if let Some(waker) = waker {
            waker.wake();
        }
    });
}

impl Timer {
    fn set_tp_timer(&mut self, duration: Duration) {
        // Take a reference before setting the timer to ensure the
        // reference is not released in `tp_timer_callback` before it is
        // acquired.
        let inner_ref = self.inner.clone();
        if !self.tp_timer.set(duration) {
            // Donate the reference to be released in tp_timer_callback() or
            // in drop().
            let _ = Arc::into_raw(inner_ref);
        }
    }
}

impl PollTimer for Timer {
    fn poll_timer(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<Instant> {
        let now = Instant::now();

        let mut set_timer = false;
        if let Some(deadline) = deadline {
            set_timer = deadline < self.deadline;
            self.deadline = deadline;
        }

        if self.deadline <= now {
            Poll::Ready(now)
        } else {
            let _drop_waker_outside_lock;
            let mut inner = self.inner.lock();
            if let Some(old_waker) = &mut inner.waker {
                if !old_waker.will_wake(cx.waker()) {
                    _drop_waker_outside_lock = std::mem::replace(old_waker, cx.waker().clone());
                }
            } else {
                inner.waker = Some(cx.waker().clone());
                set_timer = true;
            }
            if set_timer {
                drop(inner);
                self.set_tp_timer(self.deadline - now);
            }
            Poll::Pending
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        if self.deadline > deadline && self.inner.lock().waker.is_some() {
            self.set_tp_timer(deadline - Instant::now());
        }
        self.deadline = deadline;
    }
}

#[derive(Debug)]
pub struct OverlappedIo {
    tp_io: TpIo,
    handle: SendSyncRawHandle,
}

impl OverlappedIoDriver for TpPool {
    type OverlappedIo = OverlappedIo;

    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo> {
        // SAFETY: caller ensures `handle` is uniquely owned, so associating a
        // completion port and changing the completion modes is safe.
        unsafe {
            let tp_io = TpIo::new(handle, Some(tp_io_callback), null_mut())?;
            set_file_completion_notification_modes(
                handle,
                FILE_SKIP_SET_EVENT_ON_HANDLE | FILE_SKIP_COMPLETION_PORT_ON_SUCCESS,
            )?;
            Ok(OverlappedIo {
                tp_io,
                handle: SendSyncRawHandle(handle),
            })
        }
    }
}

unsafe extern "system" fn tp_io_callback(
    _: *mut TP_CALLBACK_INSTANCE,
    _: *mut c_void,
    overlapped: *mut c_void,
    _: u32,
    _: usize,
    _: *mut TP_IO,
) {
    let mut wakers = WakerList::default();
    // SAFETY: the IO is done and will be considered so only once.
    unsafe { overlapped_io_done(overlapped.cast(), &mut wakers) };
    wake_locally(|| wakers.wake());
}

impl IoOverlapped for OverlappedIo {
    fn pre_io(&self) {
        self.tp_io.start_io()
    }

    unsafe fn post_io(&self, completed: bool, _overlapped: &pal::windows::Overlapped) {
        if completed {
            // SAFETY: the IO completion will not arrive to the thread pool.
            unsafe { self.tp_io.cancel_io() };
        }
    }

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
    use super::TpPool;
    use crate::executor_tests;
    use crate::task::Spawn;
    use futures::executor::block_on;

    #[test]
    fn waker_works() {
        block_on(TpPool::system().spawn("test", executor_tests::waker_tests()))
    }

    #[test]
    fn spawn_works() {
        executor_tests::spawn_tests(|| (TpPool::system(), || ()))
    }

    #[test]
    fn sleep_works() {
        block_on(TpPool::system().spawn("test", executor_tests::sleep_tests(TpPool::system())))
    }

    #[test]
    fn wait_works() {
        block_on(TpPool::system().spawn("test", executor_tests::wait_tests(TpPool::system())))
    }

    #[test]
    fn socket_works() {
        block_on(TpPool::system().spawn("test", executor_tests::socket_tests(TpPool::system())))
    }

    #[test]
    fn overlapped_file_works() {
        block_on(TpPool::system().spawn(
            "test",
            executor_tests::windows::overlapped_file_tests(TpPool::system()),
        ))
    }
}
