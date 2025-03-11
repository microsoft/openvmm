// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::io;
use vmcore::interrupt::Interrupt;

pub trait OsEventBacked {
    fn os_event(&self) -> Option<&Event>;

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

pub struct WrappedEvent {
    _task: Task<()>,
}

impl WrappedEvent {
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

pub enum MaybeWrappedEvent<T> {
    Original(T),
    Wrapped { event: Event, wrapper: WrappedEvent },
}

impl<T: OsEventBacked + Send + 'static> MaybeWrappedEvent<T> {
    pub fn new(driver: &impl SpawnDriver, original: T) -> io::Result<Self> {
        if original.os_event().is_some() {
            Ok(Self::Original(original))
        } else {
            let (wrapper, event) = WrappedEvent::new(driver, original)?;
            Ok(Self::Wrapped { event, wrapper })
        }
    }

    pub fn event(&self) -> &Event {
        match self {
            Self::Original(original) => original.os_event().expect("event should be present"),
            Self::Wrapped { event, .. } => event,
        }
    }

    pub fn into_wrapped(self) -> Option<WrappedEvent> {
        match self {
            Self::Original(_) => None,
            Self::Wrapped { wrapper, .. } => Some(wrapper),
        }
    }
}
