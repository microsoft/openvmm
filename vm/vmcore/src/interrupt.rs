// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to support delivering notifications to the guest.

#![forbid(unsafe_code)]

use crate::local_only::LocalOnly;
use mesh::MeshPayload;
use std::fmt::Debug;
use std::sync::Arc;

/// An object representing an interrupt-like signal to notify the guest of
/// device activity.
///
/// This is generally an edge-triggered interrupt, but it could also be a synic
/// event or similar notification.
///
/// The interrupt can be backed by a [`pal_event::Event`], a
/// [`mesh::Cell<pal_event::Event>`], or a function. In the former two cases, the
/// `Interrupt` can be sent across a mesh channel to remote processes.
#[derive(Clone, Debug, MeshPayload)]
pub struct Interrupt {
    inner: InterruptInner,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::null()
    }
}

impl Interrupt {
    /// An interrupt that does nothing.
    pub fn null() -> Self {
        // Create a dummy event.
        Self::from_event(pal_event::Event::new())
    }

    /// Creates an interrupt from an event.
    ///
    /// The event will be signaled when [`Self::deliver`] is called.
    pub fn from_event(event: pal_event::Event) -> Self {
        Self {
            inner: InterruptInner::Event(Arc::new(event)),
        }
    }

    /// Creates an interrupt from a mesh cell containing an event.
    ///
    /// The current event will be signaled when [`Self::deliver`] is called. The event
    /// can be transparently changed without interaction from the caller.
    pub fn from_cell(cell: mesh::Cell<pal_event::Event>) -> Self {
        Self {
            inner: InterruptInner::Cell(Arc::new(cell)),
        }
    }

    /// Creates an interrupt from a function.
    ///
    /// The function will be called when [`Self::deliver`] is called. This type of
    /// interrupt cannot be sent to a remote process.
    pub fn from_fn<F>(f: F) -> Self
    where
        F: 'static + Send + Sync + Fn(),
    {
        Self {
            inner: InterruptInner::Fn(LocalOnly(Arc::new(f))),
        }
    }

    /// Delivers the interrupt.
    pub fn deliver(&self) {
        match &self.inner {
            InterruptInner::Event(event) => event.signal(),
            InterruptInner::Cell(cell) => cell.with(|event| event.signal()),
            InterruptInner::Fn(LocalOnly(f)) => f(),
        }
    }

    /// Gets a reference to the backing event, if there is one.
    pub fn event(&self) -> Option<&pal_event::Event> {
        match &self.inner {
            InterruptInner::Event(event) => Some(event.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone, MeshPayload)]
enum InterruptInner {
    Event(Arc<pal_event::Event>),
    Cell(Arc<mesh::Cell<pal_event::Event>>),
    Fn(LocalOnly<Arc<dyn Send + Sync + Fn()>>),
}

impl Debug for InterruptInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterruptInner::Event(_) => f.pad("Event"),
            InterruptInner::Cell(_) => f.pad("Cell"),
            InterruptInner::Fn(_) => f.pad("Fn"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Interrupt;
    use pal_async::async_test;

    #[test]
    fn test_interrupt_event() {
        let event = pal_event::Event::new();
        let interrupt = Interrupt::from_event(event.clone());
        interrupt.deliver();
        assert!(event.try_wait());
    }

    #[async_test]
    async fn test_interrupt_cell() {
        let mut event = pal_event::Event::new();
        let (mut updater, cell) = mesh::cell(event.clone());
        let interrupt = Interrupt::from_cell(cell);
        interrupt.deliver();
        assert!(event.try_wait());
        event = pal_event::Event::new();
        interrupt.deliver();
        assert!(!event.try_wait());
        updater.set(event.clone()).await;
        interrupt.deliver();
        assert!(event.try_wait());
    }
}
