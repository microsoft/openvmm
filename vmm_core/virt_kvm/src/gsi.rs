// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements GSI routing management for KVM VMs.

use crate::KvmPartitionInner;
use anyhow::Context;
use pal_event::Event;
use parking_lot::Mutex;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use virt::irqfd::IrqFdRoute;

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
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
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
    fn new_route(self: &Arc<Self>, irqfd_event: Option<Event>) -> Option<GsiRoute> {
        let gsi = self.gsi_routing.lock().alloc()?;
        Some(GsiRoute {
            partition: Arc::downgrade(self),
            inner: GsiRouteInner {
                gsi,
                irqfd_event,
                enabled: false.into(),
                enable_mutex: Mutex::new(()),
            },
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
struct GsiRoute {
    partition: Weak<KvmPartitionInner>,
    inner: GsiRouteInner,
}

struct GsiRouteInner {
    gsi: u32,
    irqfd_event: Option<Event>,
    enabled: AtomicBool,
    enable_mutex: Mutex<()>, // serializes route updates and enable/disable calls
}

impl Drop for GsiRoute {
    fn drop(&mut self) {
        if let Some(partition) = self.partition.upgrade() {
            self.inner.disable(&partition);
            self.inner.set_entry(&partition, None);
            partition.gsi_routing.lock().free(self.inner.gsi);
        }
    }
}

impl GsiRouteInner {
    fn set_entry(&self, partition: &KvmPartitionInner, new_entry: Option<kvm::RoutingEntry>) {
        let mut routing = partition.gsi_routing.lock();
        if routing.set(self.gsi, new_entry) {
            routing.update_routes(&partition.kvm);
        }
    }

    /// Enables the route and associated irqfd.
    pub fn enable(&self, partition: &KvmPartitionInner, entry: kvm::RoutingEntry) {
        let _lock = self.enable_mutex.lock();
        self.set_entry(partition, Some(entry));
        if !self.enabled.load(Ordering::Relaxed) {
            if let Some(event) = &self.irqfd_event {
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
    /// clears the `enabled` flag.
    pub fn disable(&self, partition: &KvmPartitionInner) {
        let _lock = self.enable_mutex.lock();
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(irqfd_event) = &self.irqfd_event {
                partition
                    .kvm
                    .irqfd(self.gsi, irqfd_event.as_fd().as_raw_fd(), false)
                    .expect("should not fail");
            }
            self.enabled.store(false, Ordering::Relaxed);
        }
    }
}

pub(crate) struct KvmIrqFdState {
    pub(crate) partition: Arc<KvmPartitionInner>,
}

impl KvmIrqFdState {
    pub fn new(partition: Arc<KvmPartitionInner>) -> Self {
        Self { partition }
    }

    pub fn new_irqfd_route<T: MsiRouteBuilder>(
        &self,
        builder: T,
    ) -> anyhow::Result<KvmIrqFdRoute<T>> {
        let event = Event::new();
        let route = self
            .partition
            .new_route(Some(event.clone()))
            .context("no free GSIs available for irqfd")?;
        Ok(KvmIrqFdRoute {
            builder,
            route,
            event,
        })
    }
}

/// A registered irqfd route backed by a KVM [`GsiRoute`].
///
/// Cleanup (disable irqfd, clear route, free GSI) is handled by
/// [`GsiRoute::drop`].
pub(crate) struct KvmIrqFdRoute<T> {
    builder: T,
    route: GsiRoute,
    event: Event,
}

pub(crate) trait MsiRouteBuilder: Send + Sync {
    fn routing_entry(
        &self,
        partition: &KvmPartitionInner,
        address: u64,
        data: u32,
    ) -> Option<kvm::RoutingEntry>;
}

impl<T: MsiRouteBuilder> IrqFdRoute for KvmIrqFdRoute<T> {
    fn event(&self) -> &Event {
        &self.event
    }

    fn enable(&self, address: u64, data: u32) {
        if let Some(partition) = self.route.partition.upgrade() {
            if let Some(entry) = self.builder.routing_entry(&partition, address, data) {
                self.route.inner.enable(&partition, entry);
            } else {
                tracelimit::warn_ratelimited!(
                    address,
                    data,
                    "failed to build irqfd interrupt route"
                );
                self.route.inner.disable(&partition);
                self.route.inner.set_entry(&partition, None);
            }
        }
    }

    fn disable(&self) {
        if let Some(partition) = self.route.partition.upgrade() {
            self.route.inner.disable(&partition);
        }
    }
}
