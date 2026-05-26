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

/// Saved progress state for a single virtio queue.
///
/// For split queues: `avail_index` and `used_index` are plain ring indices.
/// For packed queues: bit 15 of each carries the wrap counter
/// (`index | (wrap_counter << 15)`), matching the vhost-user wire format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, mesh::payload::Protobuf)]
#[mesh(package = "virtio.queue")]
pub struct QueueState {
    #[mesh(1)]
    pub avail_index: u16,
    #[mesh(2)]
    pub used_index: u16,
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
    Split,
    Packed(PackedQueueCompletionContext),
}

pub struct QueueWork {
    context: QueueCompletionContext,
    descriptor_index: u16,
    pub payload: Vec<VirtioQueuePayload>,
}

impl QueueWork {
    pub fn descriptor_index(&self) -> u16 {
        self.descriptor_index
    }
}

#[derive(Debug, Inspect)]
#[inspect(tag = "type")]
enum QueueGetWorkInner {
    Split(#[inspect(flatten)] SplitQueueGetWork),
    Packed(#[inspect(flatten)] PackedQueueGetWork),
}

#[derive(Debug, Inspect)]
#[inspect(tag = "type")]
enum QueueCompleteWorkInner {
    Split(#[inspect(flatten)] SplitQueueCompleteWork),
    Packed(#[inspect(flatten)] PackedQueueCompleteWork),
}

#[derive(Debug, Copy, Clone, Default, inspect::Inspect)]
pub struct QueueParams {
    pub size: u16,
    pub enable: bool,
    #[inspect(hex)]
    pub desc_addr: u64,
    #[inspect(hex)]
    pub avail_addr: u64,
    #[inspect(hex)]
    pub used_addr: u64,
}

#[derive(Debug, Inspect)]
pub(crate) struct QueueCoreGetWork {
    queue_desc: GuestMemory,
    queue_size: u16,
    features: VirtioDeviceFeatures,
    mem: GuestMemory,
    #[inspect(flatten)]
    inner: QueueGetWorkInner,
    /// Whether kick notification is currently armed.
    armed: bool,
}

impl QueueCoreGetWork {
    pub fn avail_index(&self) -> u16 {
        match &self.inner {
            QueueGetWorkInner::Split(split) => split.last_avail_index(),
            QueueGetWorkInner::Packed(packed) => packed.avail_state(),
        }
    }

    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_state: Option<QueueState>,
    ) -> Result<Self, QueueError> {
        if params.size == 0 {
            return Err(QueueError::InvalidQueueSize(params.size));
        }
        let initial_avail = initial_state.map(|s| s.avail_index);
        // Split queues require power-of-2 sizes (virtio spec §2.7.1).
        // Packed queues do not (§2.8.10.1).
        if !features.ring_packed() && !params.size.is_power_of_two() {
            return Err(QueueError::InvalidQueueSize(params.size));
        }
        let queue_desc = mem
            .subrange(params.desc_addr, descriptor_offset(params.size), true)
            .map_err(QueueError::Memory)?;
        let inner = if features.ring_packed() {
            let (index, wrap) = match initial_avail {
                Some(v) => (v & 0x7FFF, (v >> 15) != 0),
                None => (0, true),
            };
            QueueGetWorkInner::Packed(PackedQueueGetWork::new(
                features,
                mem.clone(),
                params,
                index,
                wrap,
            )?)
        } else {
            let index = initial_avail.unwrap_or(0);
            QueueGetWorkInner::Split(SplitQueueGetWork::new(
                features,
                mem.clone(),
                params,
                index,
            )?)
        };
        Ok(Self {
            queue_desc,
            queue_size: params.size,
            features,
            mem,
            inner,
            armed: false,
        })
    }

    pub fn try_next_work(&mut self) -> Result<Option<QueueWork>, QueueError> {
        match self.try_peek_work() {
            Ok(Some(work)) => {
                self.advance(&work);
                Ok(Some(work))
            }
            r => r,
        }
    }

    /// Like [`try_next_work`](Self::try_next_work), but does not advance
    /// the available index. The caller must call [`advance`](Self::advance) to
    /// consume the peeked descriptor and move to the next one. Calling this
    /// again without advancing will return the same descriptor, but note that
    /// the guest may have modified the descriptor memory in the meantime.
    pub fn try_peek_work(&mut self) -> Result<Option<QueueWork>, QueueError> {
        let index = match &mut self.inner {
            QueueGetWorkInner::Split(split) => split.is_available()?,
            QueueGetWorkInner::Packed(packed) => packed.is_available()?,
        };
        let Some(index) = index else { return Ok(None) };
        self.suppress_if_armed();
        self.work_from_index(index).map(Some)
    }

    /// Arms kick notification so the guest will send a doorbell when new work
    /// is available. Returns `true` if armed successfully (caller should
    /// sleep), or `false` if new data arrived during arming (caller should
    /// retry by calling [`try_next_work`](Self::try_next_work) again).
    ///
    /// If already armed, this is a no-op and returns `true`.
    pub fn arm_for_kick(&mut self) -> bool {
        if self.armed {
            return true;
        }
        let r = match &mut self.inner {
            QueueGetWorkInner::Split(split) => split.arm_kick(),
            QueueGetWorkInner::Packed(packed) => packed.arm_kick(),
        };
        match r {
            Ok(true) => {
                self.armed = true;
                true
            }
            Ok(false) => false,
            Err(err) => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to arm kick"
                );
                // On error, behave as if armed to avoid a busy loop in callers
                // that treat `false` as "retry immediately".
                self.armed = true;
                true
            }
        }
    }

    /// If kicks are armed, suppress them. Called automatically when work is
    /// found so the guest doesn't send unnecessary doorbells while draining.
    fn suppress_if_armed(&mut self) {
        if self.armed {
            self.armed = false;
            let r = match &self.inner {
                QueueGetWorkInner::Split(split) => split.suppress_kicks(),
                QueueGetWorkInner::Packed(packed) => packed.suppress_kicks(),
            };

            if let Err(err) = r {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to suppress kicks"
                );
            }
        }
    }

    /// Advances the available index after a successful
    /// [`try_peek_work`](Self::try_peek_work) call.
    pub fn advance(&mut self, work: &QueueWork) {
        match &mut self.inner {
            QueueGetWorkInner::Split(split) => split.advance(),
            QueueGetWorkInner::Packed(packed) => {
                let QueueCompletionContext::Packed(ctx) = &work.context else {
                    unreachable!();
                };
                packed.advance(ctx.descriptor_count());
            }
        }
    }

    fn work_from_index(&mut self, index: u16) -> Result<QueueWork, QueueError> {
        if let QueueGetWorkInner::Split(split) = &mut self.inner {
            let descriptor_index = split.get_available_descriptor_index(index)?;
            let payload = self
                .reader(descriptor_index)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(QueueWork {
                descriptor_index,
                context: QueueCompletionContext::Split,
                payload,
            })
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
            let completion_context = PackedQueueCompletionContext::new(&last, count);
            Ok(QueueWork {
                context: QueueCompletionContext::Packed(completion_context),
                payload,
                descriptor_index: index,
            })
        }
    }

    fn reader(&mut self, descriptor_index: u16) -> DescriptorReader<'_> {
        DescriptorReader {
            chain: DescriptorChain::new(self, self.features.ring_indirect_desc(), descriptor_index),
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
                    next: if let Some(active_indirect_len) = active_indirect_len {
                        // Packed descriptors consume all of the indirect
                        // descriptors based on the buffer length, regardless
                        // of the NEXT flag.
                        let next = index.wrapping_add(1);
                        if next < active_indirect_len {
                            Some(next)
                        } else {
                            None
                        }
                    } else if descriptor.flags().next() {
                        // Packed ring descriptors are sequential and wrap
                        // at queue_size.
                        let next = index.wrapping_add(1);
                        if next >= self.queue_size {
                            Some(0)
                        } else {
                            Some(next)
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

#[derive(Debug, Inspect)]
pub struct QueueCoreCompleteWork {
    #[inspect(flatten)]
    inner: QueueCompleteWorkInner,
}

impl QueueCoreCompleteWork {
    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_state: Option<QueueState>,
    ) -> Result<Self, QueueError> {
        let initial_used = initial_state.map(|s| s.used_index);
        let inner = if features.ring_packed() {
            let (index, wrap) = match initial_used {
                Some(v) => (v & 0x7FFF, (v >> 15) != 0),
                None => (0, true),
            };
            QueueCompleteWorkInner::Packed(PackedQueueCompleteWork::new(
                features,
                mem.clone(),
                params,
                index,
                wrap,
            )?)
        } else {
            let index = initial_used.unwrap_or(0);
            QueueCompleteWorkInner::Split(SplitQueueCompleteWork::new(
                features,
                mem.clone(),
                params,
                index,
            )?)
        };
        Ok(Self { inner })
    }

    pub fn used_index(&self) -> u16 {
        match &self.inner {
            QueueCompleteWorkInner::Split(split) => split.last_used_index(),
            QueueCompleteWorkInner::Packed(packed) => packed.used_state(),
        }
    }

    pub fn complete_descriptor(
        &mut self,
        work: &QueueWork,
        bytes_written: u32,
    ) -> Result<bool, QueueError> {
        match &mut self.inner {
            QueueCompleteWorkInner::Split(split) => {
                split.complete_descriptor(work.descriptor_index, bytes_written)
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
    initial_state: Option<QueueState>,
) -> Result<(QueueCoreGetWork, QueueCoreCompleteWork), QueueError> {
    let get_work = QueueCoreGetWork::new(features, mem.clone(), params, initial_state)?;
    let complete_work = QueueCoreCompleteWork::new(features, mem.clone(), params, initial_state)?;
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
