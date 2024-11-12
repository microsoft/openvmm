// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types related to event interests.

use crate::waker::WakerList;
use std::fmt;
use std::fmt::Debug;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// A set of readiness events for polled IO.
#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub struct PollEvents(u32);

impl Debug for PollEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_set();
        if self.has_in() {
            list.entry(&format_args!("IN"));
        }
        if self.has_out() {
            list.entry(&format_args!("OUT"));
        }
        if self.has_err() {
            list.entry(&format_args!("ERR"));
        }
        if self.has_hup() {
            list.entry(&format_args!("HUP"));
        }
        if self.has_pri() {
            list.entry(&format_args!("PRI"));
        }
        #[cfg(any(windows, target_os = "linux"))]
        if self.has_rdhup() {
            list.entry(&format_args!("RDHUP"));
        }
        list.finish()
    }
}

impl PollEvents {
    /// The empty set.
    pub const EMPTY: Self = Self(0);
    /// The full set.
    pub const FULL: Self = Self(0x3f);
    /// Read readiness, corresponding to `POLLIN`.
    pub const IN: Self = Self(0x1);
    /// Write readiness, corresponding to `POLLOUT`.
    pub const OUT: Self = Self(0x2);
    /// An error condition, corresponding to `POLLERR`.
    pub const ERR: Self = Self(0x4);
    /// Hangup, corresponding to `POLLHUP`. The behavior of this depends on the object.
    pub const HUP: Self = Self(0x8);
    /// Priority data readiness, corresponding to `POLLPRI`.
    pub const PRI: Self = Self(0x10);
    /// Read hangup, corresponding to `POLLRDHUP`.
    pub const RDHUP: Self = Self(0x20);

    /// Returns whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether the set includes `IN`.
    pub fn has_in(self) -> bool {
        self.0 & Self::IN.0 != 0
    }

    /// Returns whether the set includes `OUT`.
    pub fn has_out(self) -> bool {
        self.0 & Self::OUT.0 != 0
    }

    /// Returns whether the set includes `ERR`.
    pub fn has_err(self) -> bool {
        self.0 & Self::ERR.0 != 0
    }

    /// Returns whether the set includes `HUP`.
    pub fn has_hup(self) -> bool {
        self.0 & Self::HUP.0 != 0
    }

    /// Returns whether the set includes `PRI`.
    pub fn has_pri(self) -> bool {
        self.0 & Self::PRI.0 != 0
    }

    /// Returns whether the set includes `RDHUP`.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn has_rdhup(self) -> bool {
        self.0 & Self::RDHUP.0 != 0
    }
}

impl std::ops::Not for PollEvents {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0) & Self::FULL
    }
}

impl std::ops::BitOr for PollEvents {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PollEvents {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for PollEvents {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl std::ops::BitAndAssign for PollEvents {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl PollEvents {
    /// Converts to the platform-specific poll events.
    #[cfg(unix)]
    pub fn to_poll_events(self) -> i16 {
        let mut poll_events = 0;
        if self.has_in() {
            poll_events |= libc::POLLIN;
        }
        if self.has_out() {
            poll_events |= libc::POLLOUT;
        }
        if self.has_pri() {
            poll_events |= libc::POLLPRI;
        }
        if self.has_hup() {
            poll_events |= libc::POLLHUP;
        }
        if self.has_err() {
            poll_events |= libc::POLLERR;
        }
        #[cfg(target_os = "linux")]
        if self.has_rdhup() {
            poll_events |= libc::POLLRDHUP;
        }
        poll_events
    }

    /// Converts from the platform-specific poll events.
    #[cfg(unix)]
    pub fn from_poll_events(poll_events: i16) -> Self {
        let mut events = PollEvents::EMPTY;
        if poll_events & libc::POLLHUP != 0 {
            events |= PollEvents::HUP;
        }
        if poll_events & libc::POLLERR != 0 {
            events |= PollEvents::ERR;
        }
        if poll_events & libc::POLLIN != 0 {
            events |= PollEvents::IN;
        }
        if poll_events & libc::POLLOUT != 0 {
            events |= PollEvents::OUT;
        }
        if poll_events & libc::POLLPRI != 0 {
            events |= PollEvents::PRI;
        }
        #[cfg(target_os = "linux")]
        if poll_events & libc::POLLRDHUP != 0 {
            events |= PollEvents::RDHUP;
        }
        events
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn from_epoll_events(epoll_events: u32) -> Self {
        let mut events = PollEvents::EMPTY;
        if epoll_events & libc::EPOLLIN as u32 != 0 {
            events |= PollEvents::IN;
        }
        if epoll_events & libc::EPOLLOUT as u32 != 0 {
            events |= PollEvents::OUT;
        }
        if epoll_events & libc::EPOLLERR as u32 != 0 {
            events |= PollEvents::ERR;
        }
        if epoll_events & libc::EPOLLHUP as u32 != 0 {
            events |= PollEvents::HUP;
        }
        if epoll_events & libc::EPOLLPRI as u32 != 0 {
            events |= PollEvents::PRI;
        }
        if epoll_events & libc::EPOLLRDHUP as u32 != 0 {
            events |= PollEvents::RDHUP;
        }
        events
    }
}

#[derive(Debug, Default)]
struct PollInterest {
    events: PollEvents,
    observed_revents: PollEvents,
    delivered_revents: PollEvents,
    waker: Option<Waker>,
}

/// The interest slot.
///
/// Sockets, fds, and waits can have multiple concurrent outstanding polls,
/// allowing a read and write operation to be polled concurrently, for example.
/// This enum is used to distinguish between the multiple polling operations.
///
/// Although they are called `Read` and `Write`, this is just a convention, and
/// the objects can be polled using any set of events.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InterestSlot {
    /// Read interest.
    Read = 0,
    /// Write interest.
    Write = 1,
}

/// The maximum number of interest slots.
pub const SLOT_COUNT: usize = 2;

/// A set of poll interests for a single object.
#[derive(Debug, Default)]
pub(crate) struct PollInterestSet([PollInterest; SLOT_COUNT]);

impl PollInterestSet {
    pub fn poll_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        let interest = &mut self.0[slot as usize];
        // POLLHUP and POLLERR are always included.
        let events = events | PollEvents::HUP | PollEvents::ERR;
        let revents = (interest.observed_revents | interest.delivered_revents) & events;
        if !revents.is_empty() {
            interest.events = PollEvents::EMPTY;
            interest.observed_revents |= revents;
            interest.delivered_revents &= !revents;
            Poll::Ready(revents)
        } else {
            interest.events = events;
            if !interest
                .waker
                .as_ref()
                .map_or(false, |w| w.will_wake(cx.waker()))
            {
                interest.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        }
    }

    #[cfg(windows)]
    pub fn clear_all(&mut self) {
        *self = Default::default();
    }

    pub fn clear_ready(&mut self, slot: InterestSlot) {
        let interest = &mut self.0[slot as usize];
        interest.events = PollEvents::EMPTY;
        interest.observed_revents = PollEvents::EMPTY;
    }

    pub fn events_to_poll(&self) -> PollEvents {
        self.0.iter().fold(PollEvents::EMPTY, |e, p| {
            e | if ((p.observed_revents | p.delivered_revents) & p.events).is_empty() {
                p.events
            } else {
                PollEvents::EMPTY
            }
        })
    }

    pub fn wake_ready(&mut self, revents: PollEvents, wakers: &mut WakerList) {
        wakers.extend(self.0.iter_mut().filter_map(|p| {
            p.delivered_revents |= revents;
            if !(p.events & revents).is_empty() {
                p.waker.take()
            } else {
                None
            }
        }))
    }
}
