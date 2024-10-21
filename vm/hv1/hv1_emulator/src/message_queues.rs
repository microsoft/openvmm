// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Emulation of an unbounded synic message queue.
//!
//! This is separate from the synic emulator because it can be used with the
//! real hypervisor as well.
//!
//! FUTURE: This should be replaced with a bounded queue, with some kind of back
//! pressure mechanism.

use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::NUM_SINTS;
use inspect::Inspect;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use virt::x86::vp::SynicMessageQueues;

/// A set of synic message queues, one per sint.
#[derive(Inspect, Debug)]
pub struct MessageQueues {
    #[inspect(
        flatten,
        with = "|x| inspect::adhoc(|req| inspect::iter_by_index(x.lock().iter().map(|q| q.len())).inspect(req))"
    )]
    queues: Mutex<[VecDeque<HvMessage>; NUM_SINTS]>,
    #[inspect(skip)]
    pending: AtomicU16,
}

impl MessageQueues {
    /// Returns a new empty instance.
    pub fn new() -> Self {
        Self {
            queues: Default::default(),
            pending: Default::default(),
        }
    }

    /// Saves the queue state.
    pub fn save(&self) -> SynicMessageQueues {
        let queues = self
            .queues
            .lock()
            .iter()
            .map(|queue| queue.iter().copied().map(HvMessage::into_bytes).collect())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        SynicMessageQueues { queues }
    }

    /// Restores the queue state.
    pub fn restore(&self, value: &SynicMessageQueues) {
        let queues = &mut self.queues.lock();
        for (dest, src) in queues.iter_mut().zip(&value.queues) {
            dest.truncate(0);
            dest.extend(src.iter().copied().map(HvMessage::from_bytes));
        }

        let pending = queues
            .iter()
            .enumerate()
            .fold(0, |p, (i, q)| p | ((!q.is_empty() as u16) << i));

        self.pending.store(pending, Ordering::Relaxed);
    }

    /// Enqueues a message to be posted to the guest.
    pub fn enqueue_message(&self, sint: u8, message: &HvMessage) -> bool {
        let mut queues = self.queues.lock();
        queues[sint as usize].push_back(*message);
        let mask = 1 << sint;
        self.pending.fetch_or(mask, Ordering::Relaxed) & mask == 0
    }

    /// Returns the bitmap of the sints that have pending messages.
    pub fn pending_sints(&self) -> u16 {
        self.pending.load(Ordering::Relaxed)
    }

    /// Posts any pending messages, using `post_message`.
    ///
    /// If `post_message` returns `Err(HvError::ObjectInUse)`, then the message
    /// is retained in the queue. Otherwise, it is removed.
    ///
    /// Returns the sints that are still pending.
    pub fn post_pending_messages(
        &self,
        sints: u16,
        mut post_message: impl FnMut(u8, &HvMessage) -> Result<(), HvError>,
    ) -> u16 {
        for (sint_index, queue) in self.queues.lock().iter_mut().enumerate() {
            let sint = sint_index as u8;
            let mask = 1 << sint;
            if sints & mask == 0 {
                continue;
            }

            self.pending.fetch_and(!mask, Ordering::Relaxed);
            while let Some(message) = queue.front() {
                match post_message(sint, message) {
                    Ok(()) => {
                        tracing::debug!(sint, "posted sint message");
                    }
                    Err(HvError::ObjectInUse) => {
                        tracing::debug!(sint, "message slot in use");
                        self.pending.fetch_or(mask, Ordering::Relaxed);
                        break;
                    }
                    Err(err) => {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            sint,
                            "dropping sint message"
                        );
                    }
                }
                queue.pop_front();
            }
        }
        self.pending.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::MessageQueues;
    use hvdef::HvError;
    use hvdef::HvMessage;
    use hvdef::HvMessageType;

    #[test]
    fn test_message_queues() {
        let queues = MessageQueues::new();

        let message = HvMessage::new(HvMessageType(0), 0, &[]);

        queues.enqueue_message(0, &message);
        queues.enqueue_message(2, &message);
        queues.enqueue_message(4, &message);
        queues.enqueue_message(4, &message);
        assert_eq!(queues.pending_sints(), 0b10101);

        let mut sints = 0;
        queues.post_pending_messages(!1, |sint, _message| {
            if sints & (1 << sint) == 0 {
                sints |= 1 << sint;
                Ok(())
            } else {
                Err(HvError::ObjectInUse)
            }
        });

        assert_eq!(queues.pending_sints(), 0b10001);
        assert_eq!(sints, 0b10100);
    }
}
