// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio queue implementation, without any notification mechanisms, async
//! support, or other transport-specific details.

use crate::spec::VirtioDeviceFeatures;
use crate::spec::queue as spec;
use crate::spec::u16_le;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use std::sync::atomic;
use thiserror::Error;

#[derive(Debug, Clone)]
pub(crate) struct QueueCore {
    queue_size: u16,
    queue_desc: GuestMemory,
    queue_avail: GuestMemory,
    queue_used: GuestMemory,
    use_ring_event_index: bool,
    mem: GuestMemory,
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("error accessing queue memory")]
    Memory(#[source] GuestMemoryError),
    #[error("an indirect descriptor had the indirect flag set")]
    DoubleIndirect,
    #[error("a descriptor chain is too long or has a cycle")]
    TooLong,
}

#[derive(Debug, Copy, Clone, Default)]
pub struct QueueParams {
    pub size: u16,
    pub enable: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

impl QueueCore {
    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
    ) -> Result<Self, QueueError> {
        let queue_avail = mem
            .subrange(
                params.avail_addr,
                spec::AVAIL_OFFSET_RING
                    + spec::AVAIL_ELEMENT_SIZE * params.size as u64
                    + size_of::<u16>() as u64,
                true,
            )
            .map_err(QueueError::Memory)?;

        let queue_used = mem
            .subrange(
                params.used_addr,
                spec::USED_OFFSET_RING
                    + spec::USED_ELEMENT_SIZE * params.size as u64
                    + size_of::<u16>() as u64,
                true,
            )
            .map_err(QueueError::Memory)?;

        let queue_desc = mem
            .subrange(
                params.desc_addr,
                size_of::<spec::Descriptor>() as u64 * params.size as u64,
                true,
            )
            .map_err(QueueError::Memory)?;

        Ok(Self {
            queue_size: params.size,
            queue_desc,
            queue_avail,
            queue_used,
            use_ring_event_index: features.bank0().ring_event_idx(),
            mem,
        })
    }

    fn set_used_flags(&self, flags: spec::UsedFlags) -> Result<(), QueueError> {
        self.queue_used
            .write_plain::<u16_le>(0, &u16::from(flags).into())
            .map_err(QueueError::Memory)
    }

    fn get_available_index(&self) -> Result<u16, QueueError> {
        Ok(self
            .queue_avail
            .read_plain::<u16_le>(spec::AVAIL_OFFSET_IDX)
            .map_err(QueueError::Memory)?
            .get())
    }

    fn is_available(&self, queue_last_avail_index: u16) -> Result<bool, QueueError> {
        let mut avail_index = self.get_available_index()?;
        if avail_index == queue_last_avail_index {
            if self.use_ring_event_index {
                self.set_available_event(avail_index)?;
            } else {
                self.set_used_flags(spec::UsedFlags::new())?;
            }
            // Ensure the available event/used flags are visible before checking
            // the available index again.
            atomic::fence(atomic::Ordering::SeqCst);
            avail_index = self.get_available_index()?;
            if avail_index == queue_last_avail_index {
                return Ok(false);
            }
        }
        if self.use_ring_event_index {
            self.set_available_event(avail_index.wrapping_sub(1))?;
        } else {
            self.set_used_flags(spec::UsedFlags::new().with_no_notify(true))?;
        }
        // Ensure available index read is ordered before subsequent descriptor
        // reads.
        atomic::fence(atomic::Ordering::Acquire);
        Ok(true)
    }

    pub fn descriptor_index(&self, avail_index: u16) -> Result<Option<u16>, QueueError> {
        if self.is_available(avail_index)? {
            Ok(Some(self.get_available_descriptor_index(avail_index)?))
        } else {
            Ok(None)
        }
    }

    pub fn reader(&mut self, descriptor_index: u16) -> DescriptorReader<'_> {
        DescriptorReader {
            queue: self,
            indirect_queue: None,
            descriptor_index: Some(descriptor_index),
            num_read: 0,
        }
    }

    fn get_available_descriptor_index(&self, avail_index: u16) -> Result<u16, QueueError> {
        let wrapped_index = (avail_index % self.queue_size) as u64;
        Ok(self
            .queue_avail
            .read_plain::<u16_le>(
                spec::AVAIL_OFFSET_RING + spec::AVAIL_ELEMENT_SIZE * wrapped_index,
            )
            .map_err(QueueError::Memory)?
            .get())
    }

    fn set_available_event(&self, index: u16) -> Result<(), QueueError> {
        let addr = spec::USED_OFFSET_RING + spec::USED_ELEMENT_SIZE * (self.queue_size as u64);
        self.queue_used
            .write_plain::<u16_le>(addr, &index.into())
            .map_err(QueueError::Memory)
    }

    fn read_descriptor(
        &self,
        descriptor_queue: &GuestMemory,
        index: u16,
    ) -> Result<spec::Descriptor, QueueError> {
        descriptor_queue
            .read_plain(index as u64 * size_of::<spec::Descriptor>() as u64)
            .map_err(QueueError::Memory)
    }

    pub fn complete_descriptor(
        &mut self,
        queue_last_used_index: &mut u16,
        descriptor_index: u16,
        bytes_written: u32,
    ) -> Result<bool, QueueError> {
        self.set_used_descriptor(*queue_last_used_index, descriptor_index, bytes_written)?;
        let last_used_index = *queue_last_used_index;
        *queue_last_used_index = queue_last_used_index.wrapping_add(1);

        // Ensure used element writes are ordered before used index write.
        atomic::fence(atomic::Ordering::Release);
        self.set_used_index(*queue_last_used_index)?;

        // Ensure the used index write is visible before reading the field that
        // determines whether to signal.
        atomic::fence(atomic::Ordering::SeqCst);
        let send_signal = if self.use_ring_event_index {
            last_used_index == self.get_used_event()?
        } else {
            !self.get_available_flags()?.no_interrupt()
        };

        Ok(send_signal)
    }

    fn get_available_flags(&self) -> Result<spec::AvailableFlags, QueueError> {
        Ok(self
            .queue_avail
            .read_plain::<u16_le>(spec::AVAIL_OFFSET_FLAGS)
            .map_err(QueueError::Memory)?
            .get()
            .into())
    }

    fn get_used_event(&self) -> Result<u16, QueueError> {
        let addr = spec::AVAIL_OFFSET_RING + spec::AVAIL_ELEMENT_SIZE * self.queue_size as u64;
        Ok(self
            .queue_avail
            .read_plain::<u16_le>(addr)
            .map_err(QueueError::Memory)?
            .get())
    }

    fn set_used_descriptor(
        &self,
        queue_last_used_index: u16,
        descriptor_index: u16,
        bytes_written: u32,
    ) -> Result<(), QueueError> {
        let wrapped_index = (queue_last_used_index % self.queue_size) as u64;
        let addr = spec::USED_OFFSET_RING + spec::USED_ELEMENT_SIZE * wrapped_index;
        self.queue_used
            .write_plain(
                addr,
                &spec::UsedElement {
                    id: (descriptor_index as u32).into(),
                    len: bytes_written.into(),
                },
            )
            .map_err(QueueError::Memory)
    }

    fn set_used_index(&self, index: u16) -> Result<(), QueueError> {
        self.queue_used
            .write_plain::<u16_le>(spec::USED_OFFSET_IDX, &index.into())
            .map_err(QueueError::Memory)
    }
}

pub struct DescriptorReader<'a> {
    queue: &'a mut QueueCore,
    indirect_queue: Option<GuestMemory>,
    descriptor_index: Option<u16>,
    num_read: u8,
}

pub struct VirtioQueuePayload {
    pub writeable: bool,
    pub address: u64,
    pub length: u32,
}

impl DescriptorReader<'_> {
    fn next_descriptor(&mut self) -> Result<Option<VirtioQueuePayload>, QueueError> {
        let Some(descriptor_index) = self.descriptor_index else {
            return Ok(None);
        };
        let descriptor = self.queue.read_descriptor(
            self.indirect_queue
                .as_ref()
                .unwrap_or(&self.queue.queue_desc),
            descriptor_index,
        )?;
        let descriptor = if !descriptor.flags().indirect() {
            descriptor
        } else {
            if self.indirect_queue.is_some() {
                return Err(QueueError::DoubleIndirect);
            }
            // TODO: should we really create a subrange for this, or is it
            // rare enough for the HCS case that we can just read it
            // directly?
            let indirect_queue = self.indirect_queue.insert(
                self.queue
                    .mem
                    .subrange(
                        descriptor.address.get(),
                        descriptor.length.get() as u64,
                        true,
                    )
                    .map_err(QueueError::Memory)?,
            );
            self.descriptor_index = Some(0);
            self.queue.read_descriptor(indirect_queue, 0)?
        };

        self.num_read += 1;
        if descriptor.flags().next() {
            let next = descriptor.next.get();
            // Limit the descriptor chain length to avoid running out of memory
            // this may be due to a cycle in the descriptor chain.
            if self.num_read == 128 {
                return Err(QueueError::TooLong);
            }
            self.descriptor_index = Some(next);
        } else {
            self.descriptor_index = None;
        }

        Ok(Some(VirtioQueuePayload {
            writeable: descriptor.flags().write(),
            address: descriptor.address.get(),
            length: descriptor.length.get(),
        }))
    }
}

impl Iterator for DescriptorReader<'_> {
    type Item = Result<VirtioQueuePayload, QueueError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_descriptor().transpose()
    }
}
