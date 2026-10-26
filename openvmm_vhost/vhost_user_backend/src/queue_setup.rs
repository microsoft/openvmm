// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-queue readiness state machine for vhost-user.
//!
//! Each virtio queue accumulates setup messages (SET_VRING_NUM, SET_VRING_ADDR,
//! SET_VRING_KICK, etc.) before it can be started. This module tracks that
//! state and produces `QueueResources` + `QueueState` when all required fields
//! are present.

use guestmem::GuestMemory;
use pal_event::Event;
use virtio::QueueResources;
use virtio::queue::QueueParams;
use virtio::queue::QueueState;
use vmcore::interrupt::Interrupt;

/// Accumulated vring setup state for a single queue.
pub struct QueueSetup {
    /// Queue size from SET_VRING_NUM.
    size: Option<u16>,
    /// Ring addresses from SET_VRING_ADDR (as GPAs).
    addrs: Option<QueueAddrs>,
    /// Kick eventfd from SET_VRING_KICK.
    kick: Option<Event>,
    /// Call interrupt from SET_VRING_CALL.
    call: Option<Interrupt>,
    /// Raw value from SET_VRING_BASE.
    ///
    /// For split ring: low 16 bits = avail index.
    /// For packed ring: bits 0-15 = avail state (index + wrap),
    ///                  bits 16-31 = used state (index + wrap).
    base: u32,
    /// Whether the device is currently processing this queue. Devices without
    /// enable control are inactive while disabled, even with a kick fd.
    active: bool,
    pub enabled: bool,
    /// Progress retained across queue stop or reconfiguration.
    saved_state: Option<QueueState>,
}

struct QueueAddrs {
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
}

impl Default for QueueSetup {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueSetup {
    /// Create a new queue setup in the initial (inactive) state.
    pub fn new() -> Self {
        Self {
            size: None,
            addrs: None,
            kick: None,
            call: None,
            base: 0,
            active: false,
            enabled: false,
            saved_state: None,
        }
    }

    /// Store the queue size (from SET_VRING_NUM).
    pub fn set_num(&mut self, size: u16) {
        self.size = Some(size);
    }

    /// Store the ring addresses as GPAs (from SET_VRING_ADDR, after VA→GPA translation).
    pub fn set_addr(&mut self, desc: u64, avail: u64, used: u64) {
        self.addrs = Some(QueueAddrs {
            desc_addr: desc,
            avail_addr: avail,
            used_addr: used,
        });
    }

    /// Store the kick eventfd (from SET_VRING_KICK).
    pub fn set_kick(&mut self, event: Event) {
        self.kick = Some(event);
    }

    /// Installing the kick fd starts a configured ring; the
    /// eventfd need not have been signaled.
    pub fn ready_to_start(&self, process_disabled: bool) -> bool {
        !self.active
            && (self.enabled || process_disabled)
            && self.size.is_some()
            && self.addrs.is_some()
            && self.kick.is_some()
    }

    /// Store the call interrupt (from SET_VRING_CALL).
    pub fn set_call(&mut self, interrupt: Interrupt) {
        self.call = Some(interrupt);
    }

    /// Store the raw SET_VRING_BASE value.
    ///
    /// For split ring, only the low 16 bits matter (avail index).
    /// For packed ring, bits 0-15 = avail state, bits 16-31 = used state.
    pub fn set_base(&mut self, base: u32) {
        self.base = base;
        self.saved_state = None;
    }

    pub fn base(&self) -> u32 {
        self.base
    }

    pub fn stop(&mut self) {
        self.set_inactive();
        self.kick = None;
        self.call = None;
    }

    /// Whether this queue is currently active in the device.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Mark this queue as active.
    pub fn set_active(&mut self) {
        self.active = true;
    }

    /// Mark this queue as inactive.
    pub fn set_inactive(&mut self) {
        self.active = false;
    }

    /// Mark this queue as inactive and save its state for later
    /// retrieval by GET_VRING_BASE.
    pub fn set_inactive_with_state(&mut self, state: QueueState) {
        self.active = false;
        self.saved_state = Some(state);
        self.base = state.avail_index as u32 | ((state.used_index as u32) << 16);
    }

    /// Return the saved queue state without consuming it.
    pub fn saved_state(&self) -> Option<QueueState> {
        self.saved_state
    }

    /// Try to build `QueueResources` for activation.
    ///
    /// Returns `Some((resources, raw_base))` if size, addrs, and kick are all set.
    /// If call was not set, uses `Interrupt::null()`.
    ///
    /// The caller must split `raw_base` into `QueueState` based on the
    /// negotiated features (split vs packed ring).
    ///
    /// Kick and call are cloned (not consumed), so the queue can be
    /// reactivated later with new `GuestMemory` (e.g., after SET_MEM_TABLE).
    pub fn try_activate(&mut self, guest_memory: GuestMemory) -> Option<(QueueResources, u32)> {
        let size = self.size?;
        let addrs = self.addrs.as_ref()?;
        let kick = self.kick.as_ref()?.clone();

        let notify = self.call.clone().unwrap_or_else(Interrupt::null);

        let resources = QueueResources {
            params: QueueParams {
                size,
                enable: true,
                desc_addr: addrs.desc_addr,
                avail_addr: addrs.avail_addr,
                used_addr: addrs.used_addr,
            },
            notify,
            event: kick,
            guest_memory,
        };

        Some((resources, self.base))
    }

    /// Reset all state for this queue.
    pub fn reset(&mut self) {
        self.size = None;
        self.addrs = None;
        self.kick = None;
        self.call = None;
        self.base = 0;
        self.active = false;
        self.enabled = false;
        self.saved_state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn test_mem() -> GuestMemory {
        GuestMemory::allocate(4096)
    }

    #[test]
    fn incomplete_queue_returns_none() {
        let mut q = QueueSetup::new();
        assert!(q.try_activate(test_mem()).is_none());

        q.set_num(256);
        assert!(q.try_activate(test_mem()).is_none());

        q.set_addr(0x1000, 0x2000, 0x3000);
        // Still missing kick.
        assert!(q.try_activate(test_mem()).is_none());
    }

    #[test]
    fn kick_fd_starts_ring_independently_of_enable_and_signal() {
        let mut q = QueueSetup::new();
        q.set_num(16);
        q.set_addr(0, 0x1000, 0x2000);
        q.enabled = true;
        assert!(!q.ready_to_start(true));
        let kick = Event::new();
        q.set_kick(kick.clone());
        assert!(q.ready_to_start(false));
        q.enabled = false;
        assert!(q.ready_to_start(true));
        assert!(!q.ready_to_start(false));
        assert!(!kick.try_wait());
        q.set_active();
        assert!(!q.ready_to_start(true));
        q.stop();
        q.enabled = true;
        assert!(!q.ready_to_start(true));
        q.set_kick(Event::new());
        assert!(q.ready_to_start(false));
    }

    #[test]
    fn complete_queue_activates() {
        let mut q = QueueSetup::new();
        q.set_num(256);
        q.set_addr(0x1000, 0x2000, 0x3000);
        q.set_kick(Event::new());
        q.set_call(Interrupt::from_event(Event::new()));
        q.set_base(42);

        let (resources, raw_base) = q.try_activate(test_mem()).unwrap();
        assert_eq!(resources.params.size, 256);
        assert_eq!(resources.params.desc_addr, 0x1000);
        assert_eq!(resources.params.avail_addr, 0x2000);
        assert_eq!(resources.params.used_addr, 0x3000);
        assert!(resources.params.enable);
        assert_eq!(raw_base, 42);
    }

    #[test]
    fn activate_without_call_uses_null_interrupt() {
        let mut q = QueueSetup::new();
        q.set_num(128);
        q.set_addr(0x1000, 0x2000, 0x3000);
        q.set_kick(Event::new());
        // No set_call.

        let (resources, _raw_base) = q.try_activate(test_mem()).unwrap();
        assert_eq!(resources.params.size, 128);
    }

    #[test]
    fn reset_clears_state() {
        let mut q = QueueSetup::new();
        q.set_num(256);
        q.set_addr(0x1000, 0x2000, 0x3000);
        q.set_kick(Event::new());
        q.set_base(10);
        q.set_active();
        assert!(q.is_active());

        q.reset();
        assert!(!q.is_active());
        assert!(q.try_activate(test_mem()).is_none());
    }

    #[test]
    fn try_activate_retains_kick_and_call() {
        let mut q = QueueSetup::new();
        q.set_num(64);
        q.set_addr(0x1000, 0x2000, 0x3000);
        q.set_kick(Event::new());
        q.set_call(Interrupt::from_event(Event::new()));

        // First activation succeeds.
        assert!(q.try_activate(test_mem()).is_some());
        // Second activation also succeeds — kick/call are cloned, not consumed.
        assert!(q.try_activate(test_mem()).is_some());
    }
}
