// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::io;
use vmcore::interrupt::Interrupt;

/// Represents an object that may or may not be backed by an OS event, and can also be signaled
/// manually.
pub trait OsEventBacked {
    /// Gets the OS event associated with this object, if any.
    fn os_event(&self) -> Option<&Event>;

    /// Signals the object manually.
    fn signal(&self);
}

impl OsEventBacked for Interrupt {
    fn os_event(&self) -> Option<&Event> {
        self.event()
    }

    fn signal(&self) {
        self.deliver();
    }
}

/// A wrapper around an object that does not natively have an OS event, but needs to be signaled
/// using one.
pub struct WrappedEvent {
    _task: Task<()>,
}

impl WrappedEvent {
    /// Creates an OS event, and a task that will signal the original object when the event is triggered.
    fn new(
        driver: &impl SpawnDriver,
        original: impl OsEventBacked + Send + 'static,
    ) -> io::Result<(Self, Event)> {
        let event = Event::new();
        let wait = PolledWait::new(driver, event.clone())?;
        let task = driver.spawn("vmbus-event-wrapper", async move {
            Self::run(wait, original).await;
        });
        Ok((Self { _task: task }, event))
    }

    async fn run(mut event: PolledWait<Event>, original: impl OsEventBacked) {
        loop {
            event.wait().await.expect("wait should not fail");
            original.signal();
        }
    }
}

/// Represents an object that either has an OS event or is wrapped using one.
pub enum MaybeWrappedEvent<T> {
    Original(T),
    Wrapped { event: Event, wrapper: WrappedEvent },
}

impl<T: OsEventBacked + Send + 'static> MaybeWrappedEvent<T> {
    /// Creates a new `MaybeWrappedEvent`. If the original object has an OS event, it is used
    /// directly. Otherwise, a new OS event is created that can be used to signal the original
    /// object.
    pub fn new(driver: &impl SpawnDriver, original: T) -> io::Result<Self> {
        if original.os_event().is_some() {
            Ok(Self::Original(original))
        } else {
            let (wrapper, event) = WrappedEvent::new(driver, original)?;
            Ok(Self::Wrapped { event, wrapper })
        }
    }

    /// Gets the OS event associated with this object. This can be either the original event or the
    /// wrapped event.
    pub fn event(&self) -> &Event {
        match self {
            Self::Original(original) => original.os_event().expect("event should be present"),
            Self::Wrapped { event, .. } => event,
        }
    }

    /// Extracts the `WrappedEvent`, if one was created.
    pub fn into_wrapped(self) -> Option<WrappedEvent> {
        match self {
            Self::Original(_) => None,
            Self::Wrapped { wrapper, .. } => Some(wrapper),
        }
    }
}
