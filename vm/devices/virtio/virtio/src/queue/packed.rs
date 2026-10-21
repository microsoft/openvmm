// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio packed queue implementation.

use crate::queue::QueueDescriptor;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::descriptor_offset;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::queue as spec;
use crate::spec::queue::DescriptorFlags;
use guestmem::GuestMemory;
use inspect::Inspect;
use spec::EventSuppressionFlags;
use spec::PackedDescriptor;
use spec::PackedEventSuppression;
use std::sync::atomic;

pub struct PackedQueueCompletionContext {
    buffer_id: u16,
    descriptor_count: u16,
}

impl PackedQueueCompletionContext {
    pub(super) fn new(last_descriptor: &QueueDescriptor, descriptor_count: u16) -> Self {
        Self {
            buffer_id: last_descriptor
                .buffer_id
                .expect("packed descriptors have buffer id"),
            descriptor_count,
        }
    }

    pub(super) fn descriptor_count(&self) -> u16 {
        self.descriptor_count
    }
}

#[derive(Debug, Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub(crate) struct PackedQueueGetWork {
    #[inspect(skip)]
    queue_desc: GuestMemory,
    #[inspect(skip)]
    device_event: GuestMemory,
    queue_size: u16,
    next_avail_index: u16,
    wrapped_bit: bool,
    next_is_available: bool,
}

impl PackedQueueGetWork {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        if let Ok(event) = self.device_event.read_plain::<PackedEventSuppression>(0) {
            resp.field("device_event_flags", event.flags());
            resp.field("device_event_offset", event.offset());
            resp.field("device_event_wrap", event.wrap());
        }
    }

    pub fn new(
        _features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_index: u16,
        initial_wrap: bool,
    ) -> Result<Self, QueueError> {
        let queue_desc = mem
            .subrange(params.desc_addr, descriptor_offset(params.size), true)
            .map_err(QueueError::Memory)?;
        let device_event = mem
            .subrange(
                params.used_addr,
                size_of::<PackedEventSuppression>() as u64,
                true,
            )
            .map_err(QueueError::Memory)?;
        Ok(Self {
            queue_desc,
            device_event,
            queue_size: params.size,
            next_avail_index: initial_index,
            wrapped_bit: initial_wrap,
            next_is_available: false,
        })
    }

    /// Return the packed avail state: `index | (wrap_counter << 15)`.
    pub fn avail_state(&self) -> u16 {
        self.next_avail_index | (u16::from(self.wrapped_bit) << 15)
    }

    /// Checks whether a descriptor is available, returning its index.
    ///
    /// This is a lightweight check that does not arm kick notification. When
    /// `None` is returned, the caller must call [`arm_kick`](Self::arm_kick)
    /// before sleeping to ensure the guest will send a kick when new work
    /// arrives.
    pub fn is_available(&mut self) -> Result<Option<u16>, QueueError> {
        if !self.next_is_available {
            let flags: DescriptorFlags = self
                .queue_desc
                .read_plain(
                    descriptor_offset(self.next_avail_index)
                        + std::mem::offset_of!(PackedDescriptor, flags_raw) as u64,
                )
                .map_err(QueueError::Memory)?;
            if flags.available() != self.wrapped_bit || flags.used() == self.wrapped_bit {
                return Ok(None);
            }
            // Ensure subsequent descriptor-field reads cannot be reordered
            // before the flags read on weakly ordered architectures.
            atomic::fence(atomic::Ordering::Acquire);
            self.next_is_available = true;
        }
        Ok(Some(self.next_avail_index))
    }

    /// Arms kick notification so the guest will send a doorbell when new work
    /// is available. Returns `true` if armed successfully (caller should
    /// sleep), or `false` if new data arrived during arming (caller should
    /// retry).
    pub fn arm_kick(&mut self) -> Result<bool, QueueError> {
        let enable_event = PackedEventSuppression::new().with_flags(EventSuppressionFlags::Enabled);
        self.device_event
            .write_plain(0, &enable_event)
            .map_err(QueueError::Memory)?;
        // Ensure the event enable is visible before checking the descriptor.
        atomic::fence(atomic::Ordering::SeqCst);
        if self.is_available()?.is_some() {
            // New data arrived during arming — suppress kicks and report.
            self.suppress_kicks()?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Suppress kick notifications from the guest. Call this after finding
    /// work to avoid unnecessary kicks while processing.
    pub fn suppress_kicks(&self) -> Result<(), QueueError> {
        let disable_event =
            PackedEventSuppression::new().with_flags(EventSuppressionFlags::Disabled);
        self.device_event
            .write_plain(0, &disable_event)
            .map_err(QueueError::Memory)?;
        Ok(())
    }

    /// Advances `next_avail_index` by `count` descriptors.
    pub fn advance(&mut self, count: u16) {
        // A chain is never longer than the ring, so the cursor wraps at most
        // once; compare-and-subtract avoids a modulo.
        let raw = self.next_avail_index + count;
        self.next_avail_index = if raw >= self.queue_size {
            self.wrapped_bit = !self.wrapped_bit;
            raw - self.queue_size
        } else {
            raw
        };
        self.next_is_available = false;
    }
}

#[derive(Debug, Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub(crate) struct PackedQueueCompleteWork {
    #[inspect(skip)]
    queue_desc: GuestMemory,
    #[inspect(skip)]
    driver_event: GuestMemory,
    queue_size: u16,
    next_index: u16,
    wrapped_bit: bool,
    use_event_index: bool,
}

impl PackedQueueCompleteWork {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        if let Ok(event) = self.driver_event.read_plain::<PackedEventSuppression>(0) {
            resp.field("driver_event_flags", event.flags());
            resp.field("driver_event_offset", event.offset());
            resp.field("driver_event_wrap", event.wrap());
        }
    }

    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_index: u16,
        initial_wrap: bool,
    ) -> Result<Self, QueueError> {
        let queue_desc = mem
            .subrange(params.desc_addr, descriptor_offset(params.size), true)
            .map_err(QueueError::Memory)?;
        let driver_event = mem
            .subrange(
                params.avail_addr,
                size_of::<PackedEventSuppression>() as u64,
                true,
            )
            .map_err(QueueError::Memory)?;
        Ok(Self {
            queue_desc,
            driver_event,
            queue_size: params.size,
            next_index: initial_index,
            wrapped_bit: initial_wrap,
            use_event_index: features.ring_event_idx(),
        })
    }

    /// Return the packed used state: `index | (wrap_counter << 15)`.
    pub fn used_state(&self) -> u16 {
        self.next_index | (u16::from(self.wrapped_bit) << 15)
    }

    pub fn complete_descriptor(
        &mut self,
        context: &PackedQueueCompletionContext,
        bytes_written: u32,
    ) -> Result<bool, QueueError> {
        let descriptor = PackedDescriptor::new()
            .with_buffer_id(context.buffer_id)
            .with_length(bytes_written)
            .with_flags(
                DescriptorFlags::new()
                    .with_available(self.wrapped_bit)
                    .with_used(self.wrapped_bit),
            );
        // Ensure any prior writes to guest buffers (e.g. device data) are
        // visible before the used descriptor becomes visible to the guest.
        atomic::fence(atomic::Ordering::Release);
        self.queue_desc
            .write_plain(descriptor_offset(self.next_index), &descriptor)
            .map_err(QueueError::Memory)?;
        let completed_index = self.next_index;
        let completed_wrap = self.wrapped_bit;
        // A visible used marker must remain consumed even if notification
        // metadata cannot be read.
        let raw = self.next_index + context.descriptor_count;
        self.next_index = if raw >= self.queue_size {
            self.wrapped_bit = !self.wrapped_bit;
            raw - self.queue_size
        } else {
            raw
        };
        // Ensure the descriptor update is visible before checking if the guest requires notification.
        atomic::fence(atomic::Ordering::SeqCst);
        let driver_event: PackedEventSuppression = self
            .driver_event
            .read_plain(0)
            .map_err(QueueError::Memory)?;
        let send_signal = match driver_event.flags() {
            EventSuppressionFlags::Disabled => false,
            EventSuppressionFlags::DescriptorIndex if self.use_event_index => {
                driver_event.offset() == completed_index && driver_event.wrap() == completed_wrap
            }
            _ => true,
        };
        Ok(send_signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn event_index_notifies_for_completed_slot_not_next() {
        for (index, wrap) in [(5, true), (31, true), (5, false), (31, false)] {
            let next_index = (index + 1) % 32;
            let next_wrap = wrap ^ (next_index == 0);
            for (event_index, event_wrap, expected_notify) in [
                (index, wrap, true),
                (next_index, next_wrap, false),
                (index, !wrap, false),
            ] {
                let driver_event = GuestMemory::allocate(guestmem::PAGE_SIZE);
                driver_event
                    .write_plain(
                        0,
                        &PackedEventSuppression::new()
                            .with_flags(EventSuppressionFlags::DescriptorIndex)
                            .with_offset(event_index)
                            .with_wrap(event_wrap),
                    )
                    .unwrap();
                let mut queue = PackedQueueCompleteWork {
                    queue_desc: GuestMemory::allocate(guestmem::PAGE_SIZE),
                    driver_event,
                    queue_size: 32,
                    next_index: index,
                    wrapped_bit: wrap,
                    use_event_index: true,
                };
                let context = PackedQueueCompletionContext {
                    buffer_id: 7,
                    descriptor_count: 1,
                };
                assert_eq!(
                    queue.complete_descriptor(&context, 512).unwrap(),
                    expected_notify,
                    "completed ({index}, {wrap}), event ({event_index}, {event_wrap})"
                );
                assert_eq!(
                    queue.used_state(),
                    next_index | (u16::from(next_wrap) << 15)
                );
            }
        }
    }

    #[test]
    fn published_completion_advances_even_when_notification_read_fails() {
        for (index, expected) in [(0, 0x8001), (31, 0)] {
            let descriptors = GuestMemory::allocate(guestmem::PAGE_SIZE);
            let mut queue = PackedQueueCompleteWork {
                queue_desc: descriptors.clone(),
                driver_event: GuestMemory::allocate(0),
                queue_size: 32,
                next_index: index,
                wrapped_bit: true,
                use_event_index: true,
            };
            let context = PackedQueueCompletionContext {
                buffer_id: 7,
                descriptor_count: 1,
            };
            assert!(queue.complete_descriptor(&context, 512).is_err());
            assert_eq!(queue.used_state(), expected);
            let published = descriptors
                .read_plain::<PackedDescriptor>(descriptor_offset(index))
                .unwrap();
            assert_eq!(published.buffer_id, 7);
            assert_eq!(published.length, 512);
            assert!(published.flags().used());
        }
    }
}
