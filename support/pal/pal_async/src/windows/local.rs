// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A thread-local executor backed by WaitForMultipleObjects.

#![expect(clippy::undocumented_unsafe_blocks)]

use super::overlapped::IoOverlapped;
use super::overlapped::OverlappedIoDriver;
use super::overlapped::overlapped_io_done;
use super::socket::make_poll_handle_info;
use super::socket::parse_poll_handle_info;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::interest::PollInterestSet;
use crate::local::LocalDriver;
use crate::local::LocalInner;
use crate::socket::PollSocketReady;
use crate::socket::SocketReadyDriver;
use crate::sparsevec::SparseVec;
use crate::wait::PollWait;
use crate::wait::WaitDriver;
use crate::waker::WakerList;
use headervec::HeaderVec;
use once_cell::sync::OnceCell;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use pal::windows::afd;
use pal::windows::set_file_completion_notification_modes;
use pal_event::Event;
use std::fs::File;
use std::io;
use std::os::windows::prelude::*;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use winapi::shared::winerror::ERROR_OPERATION_ABORTED;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::ioapiset::CancelIoEx;
use winapi::um::ioapiset::GetOverlappedResult;
use winapi::um::processthreadsapi::GetCurrentProcess;
use winapi::um::processthreadsapi::GetCurrentThread;
use winapi::um::processthreadsapi::QueueUserAPC;
use winapi::um::synchapi::WaitForMultipleObjectsEx;
use winapi::um::synchapi::WaitForSingleObjectEx;
use winapi::um::winbase::INFINITE;
use winapi::um::winbase::WAIT_FAILED;

#[derive(Debug, Default)]
pub(crate) struct State {
    entries: SparseVec<HandleEntry>,
    sockets: SparseVec<SocketEntry>,
    files: SparseVec<SendSyncRawHandle>,
    ios: Vec<InFlightIo>,
}

#[derive(Debug)]
struct InFlightIo {
    file_id: FileId,
    overlapped: OverlappedPtr,
}

#[derive(Debug)]
struct OverlappedPtr(*const Overlapped);

// SAFETY: the overlapped structure can be accessed on any thread.
unsafe impl Send for OverlappedPtr {}

#[derive(Debug, Default)]
pub(crate) struct WaitState {
    poll_info: HeaderVec<afd::PollInfo, afd::PollHandleInfo, 32>,
    handles: Vec<SendSyncRawHandle>,
    indexes: Vec<usize>,
    afd_in_flight: bool,
    afd_overlapped: Overlapped,
    done_entry: Option<usize>,
}

#[derive(Debug)]
struct HandleEntry {
    handle: SendSyncRawHandle,
    signaled: bool,
    waker: Option<Waker>,
}

#[derive(Debug)]
struct SocketEntry {
    socket: RawSocket,
    interests: PollInterestSet,
}

#[derive(Debug, Copy, Clone)]
struct HandleId(usize);

#[derive(Debug, Copy, Clone)]
struct SocketId(usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct FileId(usize);

// TODO: switch to std::sync::OnceLock once `get_or_try_init` is stable
static AFD_FILE: OnceCell<File> = OnceCell::new();

impl State {
    fn add_handle(&mut self, handle: RawHandle) -> HandleId {
        HandleId(self.entries.add(HandleEntry {
            handle: SendSyncRawHandle(handle),
            signaled: false,
            waker: None,
        }))
    }

    fn remove_handle(&mut self, id: HandleId) {
        self.entries.remove(id.0);
    }

    fn poll_handle(&mut self, cx: &mut Context<'_>, id: HandleId) -> Poll<()> {
        let entry = &mut self.entries[id.0];
        if std::mem::take(&mut entry.signaled) {
            Poll::Ready(())
        } else {
            if !entry
                .waker
                .as_ref()
                .is_some_and(|old| old.will_wake(cx.waker()))
            {
                entry.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        }
    }

    fn clear_handle_signal(&mut self, id: HandleId) -> bool {
        let entry = &mut self.entries[id.0];
        std::mem::take(&mut entry.signaled)
    }

    fn add_socket(&mut self, socket: RawSocket) -> SocketId {
        SocketId(self.sockets.add(SocketEntry {
            socket,
            interests: Default::default(),
        }))
    }

    fn remove_socket(&mut self, id: SocketId) {
        self.sockets.remove(id.0);
    }

    fn clear_socket_ready(&mut self, id: SocketId, slot: InterestSlot) {
        let entry = &mut self.sockets[id.0];
        entry.interests.clear_ready(slot)
    }

    fn poll_socket(
        &mut self,
        cx: &mut Context<'_>,
        id: SocketId,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        let entry = &mut self.sockets[id.0];
        entry.interests.poll_ready(cx, slot, events)
    }

    fn add_file(&mut self, handle: RawHandle) -> FileId {
        FileId(self.files.add(SendSyncRawHandle(handle)))
    }

    fn remove_file(&mut self, id: FileId) {
        self.files.remove(id.0);
    }

    pub fn pre_wait(&mut self, wait_state: &mut WaitState, wait_cancel: &WaitCancel) {
        wait_state.handles.clear();
        wait_state.indexes.clear();
        for (index, entry) in self.entries.iter() {
            if entry.waker.is_some() {
                wait_state.handles.push(entry.handle);
                wait_state.indexes.push(index);
            }
        }

        // Add any files that may have pending IOs. The wait will complete when
        // an associated IO completes.
        for (_, handle) in self.files.iter() {
            wait_state.handles.push(*handle);
        }

        let poll_info = &mut wait_state.poll_info;
        poll_info.head = Default::default();
        poll_info.clear_tail();
        poll_info.extend(self.sockets.iter().filter_map(|(_, entry)| {
            let events = entry.interests.events_to_poll();
            if !events.is_empty() {
                Some(make_poll_handle_info(entry.socket as RawHandle, events))
            } else {
                None
            }
        }));

        wait_state.afd_in_flight = false;
        if !poll_info.tail.is_empty() {
            thread_local! {
                static AFD_EVENT: Event = Event::new();
            }

            let afd_handle = AFD_FILE
                .get()
                .expect("should have been created in new_socket_ready")
                .as_raw_handle();
            let afd_event = AFD_EVENT.with(|e| e.as_handle().as_raw_handle());
            wait_state.afd_overlapped.set_event(afd_event);
            poll_info.head.number_of_handles = poll_info.tail.len().try_into().unwrap();
            poll_info.head.timeout = i64::MAX;
            let len = poll_info.total_byte_len();
            unsafe {
                if !afd::poll(
                    afd_handle,
                    poll_info.as_mut_ptr(),
                    len,
                    wait_state.afd_overlapped.as_ptr(),
                ) {
                    wait_state.afd_in_flight = true;
                    wait_state.handles.push(SendSyncRawHandle(afd_event));
                }
            }
        }

        // Store the thread handle in the waker before marking the state as
        // sleeping. The thread handle will be passed to QueueUserAPC to
        // wake up this thread if the waker's `wake` method is called.
        wait_cancel.thread.get_or_init(|| {
            unsafe { BorrowedHandle::borrow_raw(GetCurrentThread()) }
                .try_clone_to_owned()
                .unwrap()
        });
    }

    pub fn post_wait(&mut self, wait_state: &mut WaitState, wakers: &mut WakerList) {
        for info in &wait_state.poll_info.tail {
            let (_, entry) = self
                .sockets
                .iter_mut()
                .find(|(_, e)| e.socket == info.handle.0 as RawSocket)
                .unwrap();

            let revents = parse_poll_handle_info(info);
            if !revents.is_empty() {
                entry.interests.wake_ready(revents, wakers);
            }
        }

        if let Some(done_entry) = wait_state.done_entry {
            let entry = &mut self.entries[done_entry];
            entry.signaled = true;
            if let Some(waker) = entry.waker.take() {
                wakers.push(waker);
            }
        }

        // Poll for completed IOs. Do so regardless of which handle was
        // signaled, since in some cases the file handle signal can be lost
        // (e.g. if a new IO is issued while another one is pending).
        self.ios.retain(|io| {
            // SAFETY: overlapped is guaranteed to be a valid &Overlapped until
            // overlapped_io_done is called on it.
            let overlapped = unsafe { &*io.overlapped.0 };
            if overlapped.io_status().is_some() {
                unsafe { overlapped_io_done(overlapped.as_ptr(), wakers) };
                false
            } else {
                true
            }
        });
    }
}

impl WaitState {
    pub fn wait(&mut self, _wait_cancel: &WaitCancel, timeout: Option<Duration>) {
        let mut n = !0;
        if self.afd_in_flight || self.poll_info.head.number_of_handles == 0 {
            let timeout =
                timeout.map_or(INFINITE, |t| t.as_millis().min(INFINITE as u128 - 1) as u32);

            unsafe {
                // If there are no handles, which can happen if we are just
                // waiting on a timeout or an alert, then wait on a pseudo
                // handle that will never be ready.
                if self.handles.is_empty() {
                    n = WaitForSingleObjectEx(GetCurrentProcess(), timeout, true.into());
                } else {
                    n = WaitForMultipleObjectsEx(
                        self.handles.len() as u32,
                        self.handles.as_ptr().cast(),
                        false.into(),
                        timeout,
                        true.into(),
                    );
                }
            }
            assert!(
                n != WAIT_FAILED,
                "wait failed: {}",
                io::Error::last_os_error(),
            );
        }

        if self.afd_in_flight {
            let afd_handle = AFD_FILE.get().unwrap().as_raw_handle();
            let io_complete = n as usize == self.handles.len() - 1;
            if !io_complete {
                unsafe {
                    CancelIoEx(afd_handle, self.afd_overlapped.as_ptr());
                }
            }
            let mut returned = 0;
            unsafe {
                if GetOverlappedResult(
                    afd_handle,
                    self.afd_overlapped.as_ptr(),
                    &mut returned,
                    (!io_complete).into(),
                ) == 0
                {
                    assert_eq!(GetLastError(), ERROR_OPERATION_ABORTED);
                    self.poll_info.head.number_of_handles = 0;
                }
            }
        }

        self.poll_info
            .truncate_tail(self.poll_info.head.number_of_handles as usize);

        self.done_entry = self.indexes.get(n as usize).copied();
    }
}

#[derive(Debug, Default)]
pub(crate) struct WaitCancel {
    thread: OnceCell<OwnedHandle>,
}

impl WaitCancel {
    pub fn cancel_wait(&self) {
        extern "system" fn no_op(_: usize) {}

        // Queue an APC to abort the call to WaitForMultipleObjectsEx. The
        // APC routine will run and do nothing, and the wait will return
        // with an error code.
        assert!(
            unsafe {
                QueueUserAPC(
                    Some(no_op),
                    self.thread
                        .get()
                        .expect("thread should be set before setting APC_WAKER_SLEEPING")
                        .as_raw_handle(),
                    0,
                )
            } != 0
        );
    }
}

impl WaitDriver for LocalDriver {
    type Wait = Wait;

    fn new_wait(&self, handle: RawHandle) -> io::Result<Self::Wait> {
        let id = self.inner.lock_sys_state().add_handle(handle);
        Ok(Wait {
            inner: self.inner.clone(),
            id,
        })
    }
}

#[derive(Debug)]
pub struct Wait {
    inner: Arc<LocalInner>,
    id: HandleId,
}

impl Drop for Wait {
    fn drop(&mut self) {
        self.inner.lock_sys_state().remove_handle(self.id);
    }
}

impl PollWait for Wait {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(self.inner.lock_sys_state().poll_handle(cx, self.id));
        Poll::Ready(Ok(()))
    }

    fn poll_cancel_wait(&mut self, _cx: &mut Context<'_>) -> Poll<bool> {
        Poll::Ready(self.inner.lock_sys_state().clear_handle_signal(self.id))
    }
}

impl SocketReadyDriver for LocalDriver {
    type SocketReady = SocketReady;

    fn new_socket_ready(&self, socket: RawSocket) -> io::Result<Self::SocketReady> {
        // Ensure the AFD file has been opened.
        AFD_FILE.get_or_try_init(afd::open_afd)?;

        let id = self.inner.lock_sys_state().add_socket(socket);
        Ok(SocketReady {
            inner: self.inner.clone(),
            id,
        })
    }
}

#[derive(Debug)]
pub struct SocketReady {
    inner: Arc<LocalInner>,
    id: SocketId,
}

impl Drop for SocketReady {
    fn drop(&mut self) {
        self.inner.lock_sys_state().remove_socket(self.id);
    }
}

impl PollSocketReady for SocketReady {
    fn poll_socket_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        self.inner
            .lock_sys_state()
            .poll_socket(cx, self.id, slot, events)
    }

    fn clear_socket_ready(&mut self, slot: InterestSlot) {
        self.inner
            .lock_sys_state()
            .clear_socket_ready(self.id, slot)
    }
}

#[derive(Debug)]
pub struct OverlappedIo {
    inner: Arc<LocalInner>,
    id: FileId,
}

impl OverlappedIoDriver for LocalDriver {
    type OverlappedIo = OverlappedIo;

    unsafe fn new_overlapped_file(&self, handle: RawHandle) -> io::Result<Self::OverlappedIo> {
        // We need the file to be signaled on completion.
        //
        // SAFETY: the caller guarantees that handle is exclusively owned.
        unsafe {
            set_file_completion_notification_modes(handle, 0)?;
        }
        let id = self.inner.lock_sys_state().add_file(handle);
        Ok(OverlappedIo {
            inner: self.inner.clone(),
            id,
        })
    }
}

impl Drop for OverlappedIo {
    fn drop(&mut self) {
        self.inner.lock_sys_state().remove_file(self.id);
    }
}

impl IoOverlapped for OverlappedIo {
    fn pre_io(&self) {}

    unsafe fn post_io(&self, completed: bool, overlapped: &Overlapped) {
        // Always take the lock to force the thread out of its wait. This is
        // necessary because issuing the IO may have reset the file's internal
        // IO completion event before the waiting thread could consume it.
        //
        // This is only a problem if IO is being issued from a different thread.
        // For better performance when multiple threads are involved, switch to
        // a different driver.
        let mut inner = self.inner.lock_sys_state();
        if !completed {
            inner.ios.push(InFlightIo {
                file_id: self.id,
                overlapped: OverlappedPtr(overlapped),
            });
        }
    }

    unsafe fn disassociate(&mut self) {
        if self
            .inner
            .lock_sys_state()
            .ios
            .iter()
            .any(|io| io.file_id == self.id)
        {
            panic!("caller bug: no pending IO should be in flight");
        }
    }
}
