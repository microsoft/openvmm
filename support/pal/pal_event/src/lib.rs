// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An abstraction over platform-specific event primitives:
//!
//! Windows: [event objects](https://learn.microsoft.com/en-us/windows/win32/sync/event-objects)
//! Linux: [eventfd](https://man7.org/linux/man-pages/man2/eventfd.2.html)
//! Other Unix: [fifo](https://man.openbsd.org/mkfifo.2)

#![warn(missing_docs)]
// UNSAFETY: FFI into platform-specific APIs.
#![expect(unsafe_code)]

mod unix;
mod windows;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

/// A platform-specific synchronization event.
#[derive(Debug)]
pub struct Event(sys::Inner);

impl Event {
    /// Creates a new event.
    ///
    /// Panics if the event cannot be created. This should only be due to low
    /// resources.
    pub fn new() -> Self {
        match Self::new_inner() {
            Ok(event) => event,
            Err(err) => panic!("failed to create event: {}", err),
        }
    }

    /// Signals the event.
    pub fn signal(&self) {
        self.signal_inner();
    }

    /// Waits for the event to be signaled and consumes the signal.
    pub fn wait(&self) {
        self.wait_inner();
    }

    /// Tries to consume the event signal.
    ///
    /// Returns `false` if the event is not currently signaled.
    pub fn try_wait(&self) -> bool {
        self.try_wait_inner()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_event() {
        let event = crate::Event::new();
        event.signal();
        event.wait();
    }
}
