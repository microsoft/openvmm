// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-to-host notification infrastructure, abstracting over [platform
//! events](pal_event::Event), and [task events](SlimEvent).

#![forbid(unsafe_code)]

use crate::interrupt::Interrupt;
use crate::local_only::LocalOnly;
use crate::slim_event::SlimEvent;
use mesh::MeshPayload;
use pal_async::driver::Driver;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use std::future::Future;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

/// An object that can be signaled when the guest needs attention.
#[derive(Debug, Clone, MeshPayload)]
pub struct Notify(Inner);

impl Notify {
    /// Creates an object from an event.
    pub fn from_event(event: Event) -> Self {
        Self(Inner::Event(event))
    }

    /// Creates an object from a slim event.
    pub fn from_slim_event(event: Arc<SlimEvent>) -> Self {
        Self(Inner::SlimEvent(LocalOnly(event)))
    }

    /// Returns a pollable version, using `driver` to poll an underlying event
    /// if there is one.
    pub fn pollable(self, driver: &(impl Driver + ?Sized)) -> std::io::Result<PolledNotify> {
        Ok(PolledNotify(match self.0 {
            Inner::Event(e) => PolledInner::Event(Mutex::new(PolledWait::new(driver, e)?)),
            Inner::SlimEvent(LocalOnly(e)) => PolledInner::SlimEvent(e),
        }))
    }

    /// Gets the underlying OS event, if there is one.
    pub fn event(&self) -> Option<&Event> {
        match &self.0 {
            Inner::Event(e) => Some(e),
            Inner::SlimEvent(_) => None,
        }
    }

    /// Gets an interrupt object that can be used to signal the underlying
    /// event.
    pub fn interrupt(self) -> Interrupt {
        match self.0 {
            Inner::Event(e) => Interrupt::from_event(e),
            Inner::SlimEvent(LocalOnly(e)) => Interrupt::from_fn(move || e.signal()),
        }
    }
}

/// A [`Notify`] object that is ready to be polled.
pub struct PolledNotify(PolledInner);

#[derive(Debug, MeshPayload)]
enum Inner {
    Event(Event),
    SlimEvent(LocalOnly<Arc<SlimEvent>>),
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        match self {
            Self::Event(event) => Self::Event(event.clone()),
            Self::SlimEvent(event) => Self::SlimEvent(event.clone()),
        }
    }
}

enum PolledInner {
    Event(Mutex<PolledWait<Event>>),
    SlimEvent(Arc<SlimEvent>),
}

impl PolledNotify {
    /// Polls for the notify object to be signaled.
    pub fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<()> {
        match &self.0 {
            PolledInner::Event(e) => e
                .lock()
                .poll_wait(cx)
                .map(|r| r.expect("waits on Event cannot fail")),
            PolledInner::SlimEvent(e) => e.poll_wait(cx),
        }
    }

    /// Waits for the notify object to be signaled.
    pub fn wait(&mut self) -> impl '_ + Unpin + Future<Output = ()> {
        poll_fn(move |cx| match &mut self.0 {
            PolledInner::Event(e) => e
                .get_mut()
                .poll_wait(cx)
                .map(|r| r.expect("waits on Event cannot fail")),
            PolledInner::SlimEvent(e) => e.poll_wait(cx),
        })
    }

    /// Returns the inner notify object.
    pub fn into_inner(self) -> Notify {
        Notify(match self.0 {
            PolledInner::Event(e) => Inner::Event(e.into_inner().into_inner()),
            PolledInner::SlimEvent(e) => Inner::SlimEvent(LocalOnly(e)),
        })
    }
}
