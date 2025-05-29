// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A thread-local executor based on poll(2).

use super::wait::FdWait;
use crate::fd::FdReadyDriver;
use crate::fd::PollFdReady;
use crate::interest::InterestSlot;
use crate::interest::PollEvents;
use crate::interest::PollInterestSet;
use crate::local::LocalDriver;
use crate::local::LocalInner;
use crate::sparsevec::SparseVec;
use crate::wait::WaitDriver;
use crate::waker::WakerList;
use pal::unix::SyscallResult;
use pal::unix::while_eintr;
use pal_event::Event;
use std::io;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct WaitState {
    pollfds: Vec<libc::pollfd>,
}

#[derive(Debug, Default)]
pub(crate) struct WaitCancel {
    event: OnceLock<Event>,
}

impl WaitCancel {
    pub fn cancel_wait(&self) {
        self.event.get().unwrap().signal();
    }
}

#[derive(Debug, Default)]
pub(crate) struct State {
    entries: SparseVec<FdEntry>,
}

#[derive(Debug)]
struct FdEntry {
    fd: RawFd,
    interests: PollInterestSet,
}

impl State {
    fn add_fd(&mut self, fd: RawFd) -> usize {
        self.entries.add(FdEntry {
            fd,
            interests: Default::default(),
        })
    }

    fn remove_fd(&mut self, index: usize) {
        self.entries.remove(index);
    }

    fn poll_fd(
        &mut self,
        cx: &mut Context<'_>,
        index: usize,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        let entry = &mut self.entries[index];
        entry.interests.poll_ready(cx, slot, events)
    }

    fn clear_fd_ready(&mut self, index: usize, slot: InterestSlot) {
        let entry = &mut self.entries[index];
        entry.interests.clear_ready(slot)
    }

    pub fn pre_wait(&mut self, wait_state: &mut WaitState, wait_cancel: &WaitCancel) {
        wait_state.pollfds.clear();
        wait_state
            .pollfds
            .extend(self.entries.iter().map(|(_, entry)| {
                let events = entry.interests.events_to_poll();
                if !events.is_empty() {
                    libc::pollfd {
                        fd: entry.fd,
                        events: events.to_poll_events(),
                        revents: 0,
                    }
                } else {
                    libc::pollfd {
                        fd: -1,
                        events: 0,
                        revents: 0,
                    }
                }
            }));

        let event = wait_cancel.event.get_or_init(Event::new);
        wait_state.pollfds.push(libc::pollfd {
            fd: event.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }

    pub fn post_wait(&mut self, wait_state: &mut WaitState, wakers: &mut WakerList) {
        for ((_, entry), pollfd) in self.entries.iter_mut().zip(wait_state.pollfds.iter_mut()) {
            let revents = PollEvents::from_poll_events(pollfd.revents);
            if !revents.is_empty() {
                entry.interests.wake_ready(revents, wakers);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn poll(pollfds: &mut [libc::pollfd], timeout: Option<&Duration>) -> i32 {
    let timeout = timeout.map(|timeout| libc::timespec {
        tv_sec: timeout.as_secs().try_into().unwrap(),
        tv_nsec: timeout.subsec_nanos().into(),
    });

    // SAFETY: calling as documented.
    unsafe {
        libc::ppoll(
            pollfds.as_mut_ptr(),
            pollfds.len().try_into().unwrap(),
            timeout.as_ref().map_or(std::ptr::null(), |t| t),
            std::ptr::null(),
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn poll(pollfds: &mut [libc::pollfd], timeout: Option<&Duration>) -> i32 {
    // SAFETY: calling as documented.
    unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len().try_into().unwrap(),
            timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32),
        )
    }
}

impl WaitState {
    pub fn wait(&mut self, wait_cancel: &WaitCancel, timeout: Option<Duration>) {
        while_eintr(|| poll(&mut self.pollfds, timeout.as_ref()).syscall_result())
            .expect("ppoll unexpectedly failed");
        if self.pollfds.last().unwrap().revents != 0 {
            // Consume the wake event.
            assert!(wait_cancel.event.get().unwrap().try_wait());
        }
    }
}

impl FdReadyDriver for LocalDriver {
    type FdReady = FdReady;

    fn new_fd_ready(&self, socket: RawFd) -> io::Result<Self::FdReady> {
        let index = self.inner.lock_sys_state().add_fd(socket);
        Ok(FdReady {
            inner: self.inner.clone(),
            index,
        })
    }
}

#[derive(Debug)]
pub struct FdReady {
    inner: Arc<LocalInner>,
    index: usize,
}

impl Drop for FdReady {
    fn drop(&mut self) {
        self.inner.lock_sys_state().remove_fd(self.index);
    }
}

impl PollFdReady for FdReady {
    fn poll_fd_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        self.inner
            .lock_sys_state()
            .poll_fd(cx, self.index, slot, events)
    }

    fn clear_fd_ready(&mut self, slot: InterestSlot) {
        self.inner.lock_sys_state().clear_fd_ready(self.index, slot)
    }
}

impl WaitDriver for LocalDriver {
    type Wait = FdWait<FdReady>;

    fn new_wait(&self, fd: RawFd, read_size: usize) -> io::Result<Self::Wait> {
        Ok(FdWait::new(fd, self.new_fd_ready(fd)?, read_size))
    }
}
