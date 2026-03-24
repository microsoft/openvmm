// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to support delivering notifications to the guest.

#![forbid(unsafe_code)]

use crate::local_only::LocalOnly;
use mesh::MeshPayload;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
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
    ///
    /// N.B. If the caller requires the interrupt to have an underlying event, it is recommended to
    ///      use [`Self::null_event`] instead.
    pub fn null() -> Self {
        // Create a dummy event.
        Self::from_event(Event::new())
    }

    /// An interrupt that does nothing but is guaranteed to have an underlying event.
    ///
    /// N.B. Currently this does the same thing as [`Self::null`], but it allows [`Self::null`] to
    ///      be optimized in the future, while callers that require an event can use this function.
    pub fn null_event() -> Self {
        Self::from_event(Event::new())
    }

    /// Creates an interrupt from an event.
    ///
    /// The event will be signaled when [`Self::deliver`] is called.
    pub fn from_event(event: Event) -> Self {
        Self {
            inner: InterruptInner::Event(Arc::new(event)),
        }
    }

    /// Creates an interrupt from a mesh cell containing an event.
    ///
    /// The current event will be signaled when [`Self::deliver`] is called. The event
    /// can be transparently changed without interaction from the caller.
    pub fn from_cell(cell: mesh::Cell<Event>) -> Self {
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
    pub fn event(&self) -> Option<&Event> {
        match &self.inner {
            InterruptInner::Event(event) => Some(event.as_ref()),
            _ => None,
        }
    }

    /// Returns an event that, when signaled, will deliver this interrupt.
    ///
    /// If the interrupt is already event-backed, returns a clone of the
    /// existing event and no proxy is needed. Otherwise, creates an
    /// [`EventProxy`] that spawns an async task to bridge a new event to
    /// [`Interrupt::deliver`]. The caller must keep the returned
    /// `Option<EventProxy>` alive for as long as the event is in use.
    pub fn event_or_proxy(
        &self,
        driver: &impl SpawnDriver,
    ) -> std::io::Result<(Event, Option<EventProxy>)> {
        if let Some(event) = self.event() {
            Ok((event.clone(), None))
        } else {
            let (proxy, event) = EventProxy::new(driver, self.clone())?;
            Ok((event, Some(proxy)))
        }
    }
}

#[derive(Clone, MeshPayload)]
enum InterruptInner {
    Event(Arc<Event>),
    Cell(Arc<mesh::Cell<Event>>),
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

/// An async task that bridges an [`Event`] to an [`Interrupt`].
///
/// When the interrupt is not directly backed by an OS event (e.g., it uses
/// a function callback for MSI-X), this wrapper creates a new event and
/// spawns a task that waits on it and calls [`Interrupt::deliver`]. When
/// the `EventProxy` is dropped, the task is cancelled.
pub struct EventProxy {
    _task: Task<()>,
}

impl EventProxy {
    /// Create a new proxy: returns the proxy (which owns the async task)
    /// and the [`Event`] that the caller should pass to the consumer.
    pub fn new(driver: &impl SpawnDriver, interrupt: Interrupt) -> std::io::Result<(Self, Event)> {
        let event = Event::new();
        let wait = PolledWait::new(driver, event.clone())?;
        let task = driver.spawn("interrupt-event-proxy", async move {
            Self::run(wait, interrupt).await;
        });
        Ok((Self { _task: task }, event))
    }

    async fn run(mut wait: PolledWait<Event>, interrupt: Interrupt) {
        loop {
            wait.wait().await.expect("wait should not fail");
            interrupt.deliver();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Interrupt;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[async_test]
    async fn test_event_or_proxy_event_backed(driver: DefaultDriver) {
        let orig_event = pal_event::Event::new();
        let interrupt = Interrupt::from_event(orig_event.clone());
        let (event, proxy) = interrupt.event_or_proxy(&driver).unwrap();
        // Event-backed interrupt should return the same event and no proxy.
        assert!(proxy.is_none());
        event.signal();
        assert!(orig_event.try_wait());
    }

    #[async_test]
    async fn test_event_or_proxy_fn_backed(driver: DefaultDriver) {
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        let interrupt = Interrupt::from_fn(move || {
            count2.fetch_add(1, Ordering::SeqCst);
        });
        let (event, proxy) = interrupt.event_or_proxy(&driver).unwrap();
        // Fn-backed interrupt requires a proxy.
        assert!(proxy.is_some());
        // Signal the proxy event and give the async task a moment to deliver.
        event.signal();
        // Poll until the proxy task delivers the interrupt.
        for _ in 0..100 {
            if count.load(Ordering::SeqCst) > 0 {
                break;
            }
            pal_async::timer::PolledTimer::new(&driver)
                .sleep(std::time::Duration::from_millis(10))
                .await;
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
