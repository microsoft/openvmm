// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements GSI routing management for KVM VMs.

use crate::KvmPartitionInner;
use pal_event::Event;
use parking_lot::Mutex;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

const NUM_GSIS: usize = 2048;

/// The GSI routing table configured for a VM.
#[derive(Debug)]
pub struct GsiRouting {
    states: Box<[GsiState; NUM_GSIS]>,
}

impl GsiRouting {
    /// Creates a new routing table.
    pub fn new() -> Self {
        Self {
            states: Box::new([GsiState::Unallocated; NUM_GSIS]),
        }
    }

    /// Claims a specific GSI.
    pub fn claim(&mut self, gsi: u32) {
        let gsi = gsi as usize;
        assert_eq!(self.states[gsi], GsiState::Unallocated);
        self.states[gsi] = GsiState::Disabled;
    }

    /// Allocates an unused GSI.
    pub fn alloc(&mut self) -> Option<u32> {
        let gsi = self.states.iter().position(|state| !state.is_allocated())?;
        self.states[gsi] = GsiState::Disabled;
        Some(gsi as u32)
    }

    /// Frees an allocated or claimed GSI.
    pub fn free(&mut self, gsi: u32) {
        let gsi = gsi as usize;
        assert_eq!(self.states[gsi], GsiState::Disabled);
        self.states[gsi] = GsiState::Unallocated;
    }

    /// Sets the routing entry for a GSI.
    pub fn set(&mut self, gsi: u32, entry: Option<kvm::RoutingEntry>) -> bool {
        let new_state = entry.map_or(GsiState::Disabled, GsiState::Enabled);
        let state = &mut self.states[gsi as usize];
        assert!(state.is_allocated());
        if *state != new_state {
            *state = new_state;
            true
        } else {
            false
        }
    }

    /// Updates the kernel's routing table with the contents of this table.
    pub fn update_routes(&mut self, kvm: &kvm::Partition) {
        let routing: Vec<_> = self
            .states
            .iter()
            .enumerate()
            .filter_map(|(gsi, state)| match state {
                GsiState::Unallocated | GsiState::Disabled => None,
                GsiState::Enabled(entry) => Some((gsi as u32, *entry)),
            })
            .collect();

        kvm.set_gsi_routes(&routing).expect("should not fail");
    }
}

impl KvmPartitionInner {
    /// Reserves a new route, optionally with an associated irqfd event.
    #[expect(dead_code)]
    pub(crate) fn new_route(self: &Arc<Self>, irqfd_event: Option<Event>) -> Option<GsiRoute> {
        let gsi = self.gsi_routing.lock().alloc()?;
        Some(GsiRoute {
            partition: Arc::downgrade(self),
            gsi,
            irqfd_event,
            enabled: false.into(),
            enable_mutex: Mutex::new(()),
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum GsiState {
    Unallocated,
    Disabled,
    Enabled(kvm::RoutingEntry),
}

impl GsiState {
    fn is_allocated(&self) -> bool {
        !matches!(self, GsiState::Unallocated)
    }
}

/// A GSI route.
#[derive(Debug)]
pub struct GsiRoute {
    partition: Weak<KvmPartitionInner>,
    gsi: u32,
    irqfd_event: Option<Event>,
    enabled: AtomicBool,
    enable_mutex: Mutex<()>, // used to serialize enable/disable calls
}

impl Drop for GsiRoute {
    fn drop(&mut self) {
        self.disable();
        self.set_entry(None);
        if let Some(partition) = self.partition.upgrade() {
            partition.gsi_routing.lock().free(self.gsi);
        }
    }
}

impl GsiRoute {
    fn set_entry(&self, new_entry: Option<kvm::RoutingEntry>) -> Option<Arc<KvmPartitionInner>> {
        let partition = self.partition.upgrade();
        if let Some(partition) = &partition {
            let mut routing = partition.gsi_routing.lock();
            if routing.set(self.gsi, new_entry) {
                routing.update_routes(&partition.kvm);
            }
        }
        partition
    }

    /// Enables the route and associated irqfd.
    #[expect(dead_code)]
    pub fn enable(&self, entry: kvm::RoutingEntry) {
        let partition = self.set_entry(Some(entry));
        let _lock = self.enable_mutex.lock();
        if !self.enabled.load(Ordering::Relaxed) {
            if let (Some(partition), Some(event)) = (&partition, &self.irqfd_event) {
                partition
                    .kvm
                    .irqfd(self.gsi, event.as_fd().as_raw_fd(), true)
                    .expect("should not fail");
            }
            self.enabled.store(true, Ordering::Relaxed);
        }
    }

    /// Disables the associated irqfd.
    ///
    /// This actually leaves the route configured, but it disables the irqfd and
    /// clears the `enabled` bool so that `signal` won't.
    pub fn disable(&self) {
        let _lock = self.enable_mutex.lock();
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(irqfd_event) = &self.irqfd_event {
                if let Some(partition) = self.partition.upgrade() {
                    partition
                        .kvm
                        .irqfd(self.gsi, irqfd_event.as_fd().as_raw_fd(), false)
                        .expect("should not fail");
                }
            }
            self.enabled.store(false, Ordering::Relaxed);
        }
    }

    /// Returns the configured irqfd event, if there is one.
    #[expect(dead_code)]
    pub fn irqfd_event(&self) -> Option<&Event> {
        self.irqfd_event.as_ref()
    }

    /// Signals the interrupt if it is enabled.
    #[allow(clippy::assertions_on_constants)]
    #[expect(dead_code)]
    pub fn signal(&self) {
        // Use a relaxed atomic read to avoid extra synchronization in this
        // path. It's up to callers to synchronize this with `enable`/`disable`
        // if strict ordering is necessary.
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(event) = &self.irqfd_event {
                event.signal();
            } else if let Some(partition) = self.partition.upgrade() {
                // TODO: `gsi` must include certain flags on aarch64 to indicate
                // the type of the interrupt: SPI or PPI handled by the in-kernel vGIC,
                // or the user mode GIC emulator (where have to specify the target VP, too).

                assert!(cfg!(guest_arch = "x86_64"));
                partition
                    .kvm
                    .irq_line(self.gsi, true)
                    .expect("interrupt delivery failure");
            }
        }
    }
}
