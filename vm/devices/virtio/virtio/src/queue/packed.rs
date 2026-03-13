// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio packed queue implementation.

use crate::queue::QueueDescriptor;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::descriptor_offset;
use crate::queue::read_descriptor;
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
    pub descriptor_index: u16,
    buffer_id: u16,
    descriptor_count: u16,
}

#[derive(Debug, Inspect)]
pub(crate) struct PackedQueueGetWork {
    #[inspect(skip)]
    queue_desc: GuestMemory,
    #[inspect(skip)]
    device_event: GuestMemory,
    queue_size: u16,
    next_avail_index: u16,
    wrapped_bit: bool,
}

impl PackedQueueGetWork {
    pub fn new(
        _features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
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
            next_avail_index: 0,
            wrapped_bit: true,
        })
    }

    pub fn is_available(&self) -> Result<Option<u16>, QueueError> {
        loop {
            let disable_event =
                PackedEventSuppression::new().with_flags(EventSuppressionFlags::Disabled);
            self.device_event
                .write_plain(0, &disable_event)
                .map_err(QueueError::Memory)?;
            atomic::fence(atomic::Ordering::Acquire);
            let descriptor: PackedDescriptor =
                read_descriptor(&self.queue_desc, self.next_avail_index)?;
            let flags = descriptor.flags();
            if flags.available() == self.wrapped_bit && flags.used() != self.wrapped_bit {
                return Ok(Some(self.next_avail_index));
            }
            let enable_event =
                PackedEventSuppression::new().with_flags(EventSuppressionFlags::Enabled);
            self.device_event
                .write_plain(0, &enable_event)
                .map_err(QueueError::Memory)?;
            atomic::fence(atomic::Ordering::SeqCst);
            let descriptor: PackedDescriptor =
                read_descriptor(&self.queue_desc, self.next_avail_index)?;
            let flags = descriptor.flags();
            if flags.available() != self.wrapped_bit || flags.used() == self.wrapped_bit {
                return Ok(None);
            }
        }
    }

    pub fn consume_next_available_descriptors(
        &mut self,
        wrapped_index: u16,
        count: u16,
        last_descriptor: QueueDescriptor,
    ) -> PackedQueueCompletionContext {
        let completion_context = PackedQueueCompletionContext {
            descriptor_index: wrapped_index,
            buffer_id: last_descriptor
                .buffer_id
                .expect("packed descriptors have buffer id"),
            descriptor_count: count,
        };

        let next_avail_index = (wrapped_index + count) % self.queue_size;
        if next_avail_index < self.next_avail_index {
            self.wrapped_bit = !self.wrapped_bit;
        }
        self.next_avail_index = next_avail_index;
        completion_context
    }
}

#[derive(Debug)]
pub(crate) struct PackedQueueCompleteWork {
    queue_desc: GuestMemory,
    driver_event: GuestMemory,
    queue_size: u16,
    next_index: u16,
    wrapped_bit: bool,
    use_event_index: bool,
}

impl PackedQueueCompleteWork {
    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
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
            next_index: 0,
            wrapped_bit: true,
            use_event_index: features.bank0().ring_event_idx(),
        })
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
        self.queue_desc
            .write_plain(descriptor_offset(self.next_index), &descriptor)
            .map_err(QueueError::Memory)?;
        // Ensure the descriptor update is visible before checking if the guest requires notification.
        atomic::fence(atomic::Ordering::SeqCst);
        let driver_event: PackedEventSuppression = self
            .driver_event
            .read_plain(0)
            .map_err(QueueError::Memory)?;
        let send_signal = match driver_event.flags() {
            EventSuppressionFlags::Disabled => false,
            EventSuppressionFlags::DescriptorIndex if self.use_event_index => {
                driver_event.offset() == self.next_index && driver_event.wrap() == self.wrapped_bit
            }
            _ => true,
        };
        let next_index = (self.next_index + context.descriptor_count) % self.queue_size;
        if next_index < self.next_index {
            self.wrapped_bit = !self.wrapped_bit;
        }
        self.next_index = next_index;
        Ok(send_signal)
    }
}
