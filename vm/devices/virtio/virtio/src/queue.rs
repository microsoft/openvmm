// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core virtio queue implementation, without any notification mechanisms, async
//! support, or other transport-specific details.

mod packed;
mod split;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::queue as spec;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use packed::PackedQueueCompleteWork;
pub use packed::PackedQueueCompletionContext;
use packed::PackedQueueGetWork;
use spec::DescriptorFlags;
use spec::PackedDescriptor;
use spec::SplitDescriptor;
use split::SplitQueueCompleteWork;
pub use split::SplitQueueCompletionContext;
use split::SplitQueueGetWork;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub(crate) fn descriptor_offset(index: u16) -> u64 {
    index as u64 * size_of::<SplitDescriptor>() as u64
}

pub(crate) fn read_descriptor<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
    queue_desc: &GuestMemory,
    index: u16,
) -> Result<T, QueueError> {
    queue_desc
        .read_plain::<T>(descriptor_offset(index))
        .map_err(QueueError::Memory)
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("error accessing queue memory")]
    Memory(#[source] GuestMemoryError),
    #[error("an indirect descriptor had the indirect flag set")]
    DoubleIndirect,
    #[error("a descriptor chain is too long or has a cycle")]
    TooLong,
    #[error("Invalid queue size {0}. Must be a power of 2.")]
    InvalidQueueSize(u16),
}

pub struct QueueDescriptor {
    address: u64,
    length: u32,
    flags: DescriptorFlags,
    pub(crate) buffer_id: Option<u16>,
    next: Option<u16>,
}

pub enum QueueCompletionContext {
    Split(SplitQueueCompletionContext),
    Packed(PackedQueueCompletionContext),
}

pub struct QueueWork {
    context: QueueCompletionContext,
    pub payload: Vec<VirtioQueuePayload>,
}

impl QueueWork {
    pub fn descriptor_index(&self) -> u16 {
        match &self.context {
            QueueCompletionContext::Split(context) => context.descriptor_index,
            QueueCompletionContext::Packed(context) => context.descriptor_index,
        }
    }
}

#[derive(Debug, Inspect)]
#[inspect(tag = "type")]
enum QueueGetWorkInner {
    Split(#[inspect(flatten)] SplitQueueGetWork),
    Packed(#[inspect(flatten)] PackedQueueGetWork),
}

#[derive(Debug)]
enum QueueCompleteWorkInner {
    Split(SplitQueueCompleteWork),
    Packed(PackedQueueCompleteWork),
}

#[derive(Debug, Copy, Clone, Default)]
pub struct QueueParams {
    pub size: u16,
    pub enable: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

#[derive(Debug, Inspect)]
pub(crate) struct QueueCoreGetWork {
    queue_desc: GuestMemory,
    queue_size: u16,
    #[inspect(skip)]
    features: VirtioDeviceFeatures,
    mem: GuestMemory,
    #[inspect(flatten)]
    inner: QueueGetWorkInner,
}

impl QueueCoreGetWork {
    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
    ) -> Result<Self, QueueError> {
        // Queue size must be a power of 2
        if !params.size.is_power_of_two() {
            return Err(QueueError::InvalidQueueSize(params.size));
        }
        let queue_desc = mem
            .subrange(params.desc_addr, descriptor_offset(params.size), true)
            .map_err(QueueError::Memory)?;
        let inner = if features.bank1().ring_packed() {
            QueueGetWorkInner::Packed(PackedQueueGetWork::new(
                features.clone(),
                mem.clone(),
                params,
            )?)
        } else {
            QueueGetWorkInner::Split(SplitQueueGetWork::new(
                features.clone(),
                mem.clone(),
                params,
            )?)
        };
        Ok(Self {
            queue_desc,
            queue_size: params.size,
            features,
            mem,
            inner,
        })
    }

    pub fn try_next_work(&mut self) -> Result<Option<QueueWork>, QueueError> {
        let index = match &mut self.inner {
            QueueGetWorkInner::Split(split) => split.is_available()?,
            QueueGetWorkInner::Packed(packed) => packed.is_available()?,
        };
        let Some(index) = index else {
            return Ok(None);
        };
        if let QueueGetWorkInner::Split(split) = &mut self.inner {
            // Fetch descriptor index from given available index.
            let descriptor_index = split.get_available_descriptor_index(index)?;
            let payload = self
                .reader(descriptor_index)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(QueueWork {
                context: QueueCompletionContext::Split(SplitQueueCompletionContext {
                    descriptor_index,
                }),
                payload,
            }))
        } else {
            let (payload, last_primary_desc_index) = {
                let mut reader = self.reader(index);
                (
                    (&mut reader).collect::<Result<Vec<_>, _>>()?,
                    reader.last_primary_desc_index(),
                )
            };
            let last = self.descriptor(&self.queue_desc, last_primary_desc_index, None)?;
            let count = if last_primary_desc_index >= index {
                last_primary_desc_index - index + 1
            } else {
                // Wrapped around the end of the queue.
                self.queue_size - index + last_primary_desc_index + 1
            };
            // Packed descriptors can use additional ring-contiguous
            // descriptors to describe a buffer. Find the last descriptor in
            // the current chain and update the available index accordingly.
            // Indirect descriptors are ignored.
            let QueueGetWorkInner::Packed(packed) = &mut self.inner else {
                unreachable!();
            };
            let completion_context = packed.consume_next_available_descriptors(index, count, last);
            Ok(Some(QueueWork {
                context: QueueCompletionContext::Packed(completion_context),
                payload,
            }))
        }
    }

    fn reader(&mut self, descriptor_index: u16) -> DescriptorReader<'_> {
        DescriptorReader {
            chain: DescriptorChain::new(
                self,
                self.features.bank0().ring_indirect_desc(),
                descriptor_index,
            ),
        }
    }

    fn descriptor(
        &self,
        desc_queue: &GuestMemory,
        index: u16,
        active_indirect_len: Option<u16>,
    ) -> Result<QueueDescriptor, QueueError> {
        let descriptor = match self.inner {
            QueueGetWorkInner::Split(_) => {
                let descriptor: SplitDescriptor = read_descriptor(desc_queue, index)?;
                QueueDescriptor {
                    address: descriptor.address.get(),
                    length: descriptor.length.get(),
                    flags: descriptor.flags(),
                    buffer_id: None,
                    next: if descriptor.flags().next() {
                        Some(descriptor.next.get())
                    } else {
                        None
                    },
                }
            }
            QueueGetWorkInner::Packed(_) => {
                let descriptor: PackedDescriptor = read_descriptor(desc_queue, index)?;
                QueueDescriptor {
                    address: descriptor.address.get(),
                    length: descriptor.length.get(),
                    flags: descriptor.flags(),
                    buffer_id: Some(descriptor.buffer_id.get()),
                    next: if descriptor.flags().next() {
                        Some(index.wrapping_add(1))
                    } else if let Some(active_indirect_len) = active_indirect_len {
                        // Packed descriptors consume all of the indirect
                        // descriptors, even when the next flag is not set.
                        let next = index.wrapping_add(1);
                        if next < active_indirect_len {
                            Some(next)
                        } else {
                            None
                        }
                    } else {
                        None
                    },
                }
            }
        };
        Ok(descriptor)
    }

    fn size(&self) -> u16 {
        self.queue_size
    }
}

#[derive(Debug)]
pub struct QueueCoreCompleteWork {
    inner: QueueCompleteWorkInner,
}

impl QueueCoreCompleteWork {
    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
    ) -> Result<Self, QueueError> {
        let inner = if features.bank1().ring_packed() {
            QueueCompleteWorkInner::Packed(PackedQueueCompleteWork::new(
                features.clone(),
                mem.clone(),
                params,
            )?)
        } else {
            QueueCompleteWorkInner::Split(SplitQueueCompleteWork::new(
                features.clone(),
                mem.clone(),
                params,
            )?)
        };
        Ok(Self { inner })
    }

    pub fn complete_descriptor(
        &mut self,
        work: &QueueWork,
        bytes_written: u32,
    ) -> Result<bool, QueueError> {
        match &mut self.inner {
            QueueCompleteWorkInner::Split(split) => {
                let QueueCompletionContext::Split(context) = &work.context else {
                    panic!("mismatched queue completion context for split queue");
                };
                split.complete_descriptor(context, bytes_written)
            }
            QueueCompleteWorkInner::Packed(packed) => {
                let QueueCompletionContext::Packed(context) = &work.context else {
                    panic!("mismatched queue completion context for packed queue");
                };
                packed.complete_descriptor(context, bytes_written)
            }
        }
    }
}

pub(crate) fn new_queue(
    features: VirtioDeviceFeatures,
    mem: GuestMemory,
    params: QueueParams,
) -> Result<(QueueCoreGetWork, QueueCoreCompleteWork), QueueError> {
    let get_work = QueueCoreGetWork::new(features.clone(), mem.clone(), params)?;
    let complete_work = QueueCoreCompleteWork::new(features.clone(), mem.clone(), params)?;
    Ok((get_work, complete_work))
}

pub struct DescriptorReader<'a> {
    chain: DescriptorChain<'a>,
}

impl DescriptorReader<'_> {
    pub fn last_primary_desc_index(&self) -> u16 {
        self.chain.last_primary_desc_index()
    }
}

pub struct VirtioQueuePayload {
    pub writeable: bool,
    pub address: u64,
    pub length: u32,
}

impl Iterator for DescriptorReader<'_> {
    type Item = Result<VirtioQueuePayload, QueueError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.chain.next().map(|descriptor| {
            descriptor.map(|descriptor| VirtioQueuePayload {
                writeable: descriptor.flags.write(),
                address: descriptor.address,
                length: descriptor.length,
            })
        })
    }
}

pub struct DescriptorChain<'a> {
    queue: &'a QueueCoreGetWork,
    /// Maximum chain length — always the original ring queue size (spec §2.7.5.3.1).
    queue_size: u16,
    indirect_support: bool,
    indirect_queue: Option<GuestMemory>,
    /// Entry count of the active indirect table, if any.
    indirect_table_len: Option<u16>,
    descriptor_index: Option<u16>,
    last_primary_desc_index: u16,
    num_read: u16,
}

impl<'a> DescriptorChain<'a> {
    fn new(queue: &'a QueueCoreGetWork, indirect_support: bool, descriptor_index: u16) -> Self {
        Self {
            queue,
            queue_size: queue.size(),
            indirect_support,
            indirect_queue: None,
            indirect_table_len: None,
            descriptor_index: Some(descriptor_index),
            last_primary_desc_index: descriptor_index,
            num_read: 0,
        }
    }

    fn next_descriptor(&mut self) -> Result<Option<QueueDescriptor>, QueueError> {
        let Some(descriptor_index) = self.descriptor_index else {
            return Ok(None);
        };
        let descriptor = self.queue.descriptor(
            self.indirect_queue
                .as_ref()
                .unwrap_or(&self.queue.queue_desc),
            descriptor_index,
            self.indirect_table_len,
        )?;
        let descriptor = if !self.indirect_support || !descriptor.flags.indirect() {
            if self.indirect_queue.is_none() {
                self.last_primary_desc_index = descriptor_index;
            }
            descriptor
        } else {
            if self.indirect_queue.is_some() {
                return Err(QueueError::DoubleIndirect);
            }
            let indirect_queue = self.indirect_queue.insert(
                self.queue
                    .mem
                    .subrange(descriptor.address, descriptor.length as u64, true)
                    .map_err(QueueError::Memory)?,
            );
            self.descriptor_index = Some(0);
            let indirect_len = (descriptor.length / size_of::<SplitDescriptor>() as u32) as u16;
            self.indirect_table_len = Some(indirect_len);
            self.queue
                .descriptor(indirect_queue, 0, Some(indirect_len))?
        };

        self.num_read += 1;
        self.descriptor_index = descriptor.next;
        // A descriptor chain must not exceed the queue size (virtio spec
        // §2.7.5.3.1). Reject chains that hit this limit—this also catches
        // cycles in the descriptor ring.
        if self.descriptor_index.is_some() && self.num_read == self.queue_size {
            return Err(QueueError::TooLong);
        }
        Ok(Some(descriptor))
    }

    pub fn last_primary_desc_index(&self) -> u16 {
        self.last_primary_desc_index
    }
}

impl Iterator for DescriptorChain<'_> {
    type Item = Result<QueueDescriptor, QueueError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_descriptor().transpose()
    }
}
