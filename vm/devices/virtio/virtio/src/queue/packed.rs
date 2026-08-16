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
        // Ensure the descriptor update is visible before checking if the guest requires notification.
        atomic::fence(atomic::Ordering::SeqCst);
        let driver_event: PackedEventSuppression = self
            .driver_event
            .read_plain(0)
            .map_err(QueueError::Memory)?;
        // Both ends of the range this completion covers are needed to decide the
        // notification, so advance first and keep the start.
        let start = self.next_index;
        let start_wrap = self.wrapped_bit;
        // The un-wrapped end of the range, computed in u32 because the u16 sum has no
        // headroom: `start + descriptor_count` reaches `2 * queue_size - 1`, which is
        // exactly `u16::MAX` at the largest ring the spec allows.
        let end = u32::from(start) + u32::from(context.descriptor_count);
        // Wraps at most once (see `advance`); compare-and-subtract avoids a modulo. Both
        // arms are reduced into the ring, so they fit u16.
        self.next_index = if end >= u32::from(self.queue_size) {
            self.wrapped_bit = !self.wrapped_bit;
            (end - u32::from(self.queue_size)) as u16
        } else {
            end as u16
        };
        let send_signal = match driver_event.flags() {
            EventSuppressionFlags::Disabled => false,
            EventSuppressionFlags::DescriptorIndex if self.use_event_index => {
                armed_position_passed(
                    driver_event.offset(),
                    driver_event.wrap(),
                    start,
                    start_wrap,
                    end,
                    self.queue_size,
                )
            }
            _ => true,
        };
        Ok(send_signal)
    }
}

/// Whether a completion covering the descriptors `[start, end)` reaches or passes the ring
/// position the driver armed at.
///
/// The driver publishes that position in the Driver Event Suppression structure (VIRTIO 1.3
/// section 2.8.14, "Event Suppression Structure Format"). The spec's literal rule for it is a
/// position match: "Event will only trigger when this descriptor is made available/used
/// respectively" (section 2.8.10, "Driver and Device Event Suppression"). This test is a
/// deliberate superset of that wording, CONTAINMENT of the armed position in the completed
/// range, never equality with either endpoint. Equality is right only while every completion
/// advances by exactly one descriptor - which is why it survived so long: at
/// `descriptor_count == 1` the two agree on every input. A chained completion (virtio-net
/// receive, a virtio-blk header/data/status chain) steps OVER the armed position without
/// landing on it, and the driver is then never woken for a request the device has already
/// finished. It waits forever while the device reports itself idle.
///
/// The superset is grounded in the driver this device serves, not in spec prose. Linux makes the
/// gap the common case rather than a corner: `virtqueue_enable_cb_delayed_packed` arms at
/// `last_used + 3/4 of the in-flight count`, a position no single completion is obliged to land
/// on.
///
/// Positions are compared in a lap-extended space anchored at `start`: `end` is
/// `start + descriptor_count` and may run past `queue_size` when the completion wraps, and an
/// armed position whose wrap counter belongs to the next lap is lifted by one ring. The
/// equivalent modular arithmetic over `u16` is harder to read at the wrap boundary, which is
/// where this is easiest to get wrong.
fn armed_position_passed(
    event_offset: u16,
    event_wrap: bool,
    start: u16,
    start_wrap: bool,
    end: u32,
    queue_size: u16,
) -> bool {
    // `end` is deliberately NOT reduced modulo the ring: it is the un-wrapped end of the range,
    // so a completion that crosses the lap boundary stays comparable with a lifted armed
    // position. That is why it is a u32, along with the armed position below, which is lifted
    // by a ring and would overflow u16 near the top of a large one.
    let armed = u32::from(event_offset)
        + if event_wrap == start_wrap {
            0
        } else {
            u32::from(queue_size)
        };
    (u32::from(start)..end).contains(&armed)
}

#[cfg(test)]
mod tests {
    use super::armed_position_passed;

    /// A small ring, so a completion can be placed right at the lap boundary.
    const RING: u16 = 8;

    /// The case the integration test drives end to end, pinned here at the boundary itself:
    /// a chain completes 5 and 6 in one step, and a driver armed at 6 must be woken even
    /// though the used position never equals 6 - it goes from 5 straight to 7.
    #[test]
    fn a_chain_that_steps_over_the_armed_position_notifies() {
        assert!(armed_position_passed(6, true, 5, true, 7, RING));
        // The landing case still holds: armed exactly where the completion starts.
        assert!(armed_position_passed(5, true, 5, true, 6, RING));
    }

    /// A completion that crosses the ring boundary must still wake a driver armed just past
    /// it, which is only possible if the armed position is lifted by one ring: the driver
    /// publishes it with the NEXT lap's wrap counter, so raw offset 0 sits AHEAD of start 7,
    /// not six descriptors behind it. Getting this wrong is invisible until a queue happens
    /// to wrap mid-chain, and then it hangs the same way.
    #[test]
    fn a_completion_crossing_the_ring_boundary_notifies_for_the_next_lap() {
        // start 7 (wrap true), a 2-descriptor chain -> un-wrapped end 9, covering 7 and 8(=0').
        assert!(armed_position_passed(0, false, 7, true, 9, RING));
        // With the armed position one short of being covered, there is nothing to announce
        // yet - the boundary has to hold from both sides.
        assert!(!armed_position_passed(1, false, 7, true, 9, RING));
    }

    /// An armed position the device has ALREADY passed in this lap earns no notification:
    /// it was announced when it was covered, and re-announcing it would be a spurious
    /// interrupt on every later completion. This is the branch a naive "armed <= end" test
    /// would get wrong, and no chained test would catch it.
    #[test]
    fn an_armed_position_already_behind_the_device_does_not_notify() {
        assert!(!armed_position_passed(2, true, 5, true, 6, RING));
        // Nor when the completion is a long chain that ends well past it.
        assert!(!armed_position_passed(0, true, 4, true, 8, RING));
    }

    /// The boundary is half-open at BOTH ends, which is what keeps a wake from firing one
    /// completion early: the range covers the descriptors completed, not the position the
    /// device will write next.
    #[test]
    fn the_position_the_device_will_write_next_is_not_yet_covered() {
        assert!(!armed_position_passed(6, true, 5, true, 6, RING));
        assert!(!armed_position_passed(0, false, 7, true, 8, RING));
    }
}
