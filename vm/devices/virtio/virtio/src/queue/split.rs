// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio split queue implementation.

use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::queue as spec;
use crate::spec::u16_le;
use guestmem::GuestMemory;
use inspect::Inspect;
use std::sync::atomic;

#[derive(Debug, Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub(crate) struct SplitQueueGetWork {
    queue_avail: GuestMemory,
    queue_used: GuestMemory,
    queue_size: u16,
    last_avail_index: u16,
    /// Cached guest avail_index from the last read, to avoid re-reading for
    /// each descriptor when draining a batch.
    cached_avail_index: u16,
    use_ring_event_index: bool,
}

impl SplitQueueGetWork {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field("available_index", self.get_available_index().ok());
    }

    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_avail_index: u16,
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
        Ok(Self {
            queue_avail,
            queue_used,
            queue_size: params.size,
            last_avail_index: initial_avail_index,
            cached_avail_index: initial_avail_index,
            use_ring_event_index: features.ring_event_idx(),
        })
    }

    pub fn last_avail_index(&self) -> u16 {
        self.last_avail_index
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

    /// Checks whether a descriptor is available, returning its wrapped index.
    /// Does not advance `last_avail_index`; call [`advance`](Self::advance)
    /// to consume the descriptor.
    ///
    /// This is a lightweight check that does not arm kick notification. When
    /// `None` is returned, the caller must call [`arm_kick`](Self::arm_kick)
    /// before sleeping to ensure the guest will send a kick when new work
    /// arrives.
    ///
    /// Reads the guest's avail_index only when the locally cached value has
    /// been exhausted, allowing multiple descriptors to be drained per read.
    pub fn is_available(&mut self) -> Result<Option<u16>, QueueError> {
        if self.cached_avail_index == self.last_avail_index {
            // Re-read the guest's avail_index to see if new work arrived.
            self.cached_avail_index = self.get_available_index()?;
            if self.cached_avail_index == self.last_avail_index {
                return Ok(None);
            }
            // Ensure available index read is ordered before subsequent
            // descriptor reads. Only needed when we actually read from guest
            // memory; the cached path reuses a previously-fenced value.
            atomic::fence(atomic::Ordering::Acquire);
        }
        Ok(Some(self.last_avail_index & (self.queue_size - 1)))
    }

    /// Arms kick notification so the guest will send a doorbell when new work
    /// is available. Returns `true` if armed successfully (caller should
    /// sleep), or `false` if new data arrived during arming (caller should
    /// retry).
    pub fn arm_kick(&mut self) -> Result<bool, QueueError> {
        if self.use_ring_event_index {
            self.set_available_event(self.last_avail_index)?;
        } else {
            self.set_used_flags(spec::UsedFlags::new())?;
        }
        // Ensure the available event/used flags are visible before checking
        // the available index again.
        atomic::fence(atomic::Ordering::SeqCst);
        if self.is_available()?.is_some() {
            // New work arrived during arming — suppress kicks again.
            self.suppress_kicks()?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Suppress kick notifications from the guest. Call this after finding
    /// work to avoid unnecessary kicks while processing.
    ///
    /// With `EVENT_IDX`, the `avail_event` value set during arming is
    /// inherently index-based and will go stale as we drain descriptors.
    /// There's no good way to express "don't kick" — any fixed index will
    /// eventually be hit on wrap. So we skip the write; the worst case is
    /// one spurious kick per u16 wrap (every 65536 descriptors).
    pub fn suppress_kicks(&self) -> Result<(), QueueError> {
        if !self.use_ring_event_index {
            self.set_used_flags(spec::UsedFlags::new().with_no_notify(true))?;
        }
        Ok(())
    }

    /// Advances `last_avail_index` by one, consuming the descriptor returned
    /// by [`is_available`](Self::is_available).
    pub fn advance(&mut self) {
        self.last_avail_index = self.last_avail_index.wrapping_add(1);
    }

    pub fn get_available_descriptor_index(&self, wrapped_index: u16) -> Result<u16, QueueError> {
        Ok(self
            .queue_avail
            .read_plain::<u16_le>(
                spec::AVAIL_OFFSET_RING + spec::AVAIL_ELEMENT_SIZE * wrapped_index as u64,
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
}

#[derive(Debug, Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub(crate) struct SplitQueueCompleteWork {
    #[inspect(skip)]
    queue_avail: GuestMemory,
    #[inspect(skip)]
    queue_used: GuestMemory,
    queue_size: u16,
    last_used_index: u16,
    use_ring_event_index: bool,
}

impl SplitQueueCompleteWork {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        if self.use_ring_event_index {
            resp.field("used_event", self.get_used_event().ok());
        } else {
            resp.field("available_flags", self.get_available_flags().ok());
        }
    }

    pub fn new(
        features: VirtioDeviceFeatures,
        mem: GuestMemory,
        params: QueueParams,
        initial_used_index: u16,
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
        Ok(Self {
            queue_avail,
            queue_used,
            queue_size: params.size,
            last_used_index: initial_used_index,
            use_ring_event_index: features.ring_event_idx(),
        })
    }

    pub fn last_used_index(&self) -> u16 {
        self.last_used_index
    }

    pub fn complete_descriptor(
        &mut self,
        descriptor_index: u16,
        bytes_written: u32,
    ) -> Result<bool, QueueError> {
        self.set_used_descriptor(self.last_used_index, descriptor_index, bytes_written)?;
        let last_used_index = self.last_used_index;
        self.last_used_index = self.last_used_index.wrapping_add(1);

        // Ensure used element writes are ordered before used index write.
        atomic::fence(atomic::Ordering::Release);
        self.set_used_index(self.last_used_index)?;

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
        let wrapped_index = (queue_last_used_index & (self.queue_size - 1)) as u64;
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
