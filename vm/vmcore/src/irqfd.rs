// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for irqfd-based interrupt delivery.
//!
//! irqfd allows a hypervisor to directly inject an MSI into a guest when an
//! event is signaled, without involving userspace in the interrupt delivery
//! path. This is used for device passthrough (e.g., VFIO) where the physical
//! device signals an event and the hypervisor injects the corresponding MSI
//! into the guest VM.

use pal_event::Event;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::Weak;

/// A cached interrupt consumer notified when its translation becomes stale.
pub trait RetranslateInterrupts: Send + Sync {
    /// The source identity used for selective invalidation.
    fn device_id(&self) -> u32;

    /// Invalidates cached delivery state before returning.
    ///
    /// Kernel bindings must be revoked synchronously. Resolution and fault
    /// reporting can then be deferred until the next actual interrupt.
    fn retranslate(&self);
}

/// Weak registrations for interrupt translation invalidation.
#[derive(Default)]
pub struct RetranslateInterruptsList {
    routes: Mutex<Vec<Weak<dyn RetranslateInterrupts>>>,
}

impl RetranslateInterruptsList {
    /// Creates an empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a consumer.
    pub fn register(&self, route: &Arc<dyn RetranslateInterrupts>) {
        let mut routes = self.routes.lock();
        routes.retain(|route| route.strong_count() != 0);
        if !routes
            .iter()
            .any(|existing| existing.ptr_eq(&Arc::downgrade(route)))
        {
            routes.push(Arc::downgrade(route));
        }
    }

    /// Removes a consumer and expired registrations.
    pub fn unregister(&self, route: &Arc<dyn RetranslateInterrupts>) {
        self.routes.lock().retain(|weak| {
            weak.upgrade()
                .is_some_and(|existing| !Arc::ptr_eq(&existing, route))
        });
    }

    /// Invalidates one source, or all sources when `device_id` is `None`.
    ///
    /// Callbacks run without the list lock. Registration must precede lookup,
    /// and each consumer must serialize lookup/installation with its callback.
    pub fn invalidate(&self, device_id: Option<u32>) {
        let routes = {
            let mut routes = self.routes.lock();
            let mut live = Vec::new();
            routes.retain(|weak| match weak.upgrade() {
                Some(route) => {
                    live.push(route);
                    true
                }
                None => false,
            });
            live
        };
        for route in routes {
            if device_id.is_none() || device_id == Some(route.device_id()) {
                route.retranslate();
            }
        }
    }
}

/// Trait for partitions that support irqfd-based interrupt delivery.
///
/// An irqfd associates an event with a GSI (Global System Interrupt), and a
/// GSI routing table maps GSIs to MSI addresses and data values. When the
/// event is signaled, the kernel looks up the GSI routing and injects the
/// configured MSI into the guest without a usermode transition.
pub trait IrqFd: Send + Sync {
    /// Creates a new irqfd route bound to the caller-supplied `event`.
    ///
    /// Allocates a GSI and (for backends that arm eagerly) registers `event`
    /// with the hypervisor so that signaling it injects the configured MSI into
    /// the guest. The caller owns `event` and passes a clone here; the same
    /// event is returned by [`IrqFdRoute::event`] for VFIO or other interrupt
    /// sources.
    ///
    /// The caller owning the event (rather than the route minting it) is what
    /// lets a single fd be rebound across backends as the guest reprograms the
    /// MSI address.
    ///
    /// When the route is dropped, the irqfd is unregistered and the GSI is
    /// freed.
    fn new_irqfd_route(&self, event: Event) -> anyhow::Result<Box<dyn IrqFdRoute>>;
}

/// A handle to a registered irqfd route.
///
/// Each route represents a single GSI with an associated event. When the
/// event is signaled (e.g., by VFIO on a device interrupt), the kernel injects
/// the MSI configured via [`enable`](IrqFdRoute::enable) into the guest.
///
/// Dropping this handle unregisters the irqfd and frees the GSI.
pub trait IrqFdRoute: Send + Sync {
    /// Returns the event that triggers interrupt injection when signaled.
    ///
    /// Pass this to VFIO `map_msix` or any other interrupt source. On Linux,
    /// this is an eventfd created by the implementation. On WHP (future), this
    /// is the event handle returned by `WHvCreateTrigger`.
    fn event(&self) -> &Event;

    /// Sets the MSI routing for this irqfd's GSI.
    ///
    /// `address` and `data` are the MSI address and data values that the
    /// hypervisor will use when injecting the interrupt into the guest.
    /// `devid` is an optional device identity used by backends that need a
    /// device ID for MSI routing (e.g., GICv3 ITS).
    ///
    /// Returns `true` if the route was bound in the kernel (the address
    /// decodes to a kernel-serviceable sink), or `false` if the route was
    /// left disabled because the address does not decode to a kernel sink
    /// (e.g. it targets an emulated controller). On `false`, the caller
    /// should deliver the interrupt in usermode instead — the fd is left
    /// unconsumed by the kernel.
    fn enable(&self, address: u64, data: u32, devid: Option<u32>) -> bool;

    /// Disables the MSI routing for this irqfd's GSI.
    ///
    /// Disarms the irqfd so that signaling the event no longer injects an
    /// interrupt. Interrupts that arrive while disabled remain pending on
    /// the event and will be delivered when [`enable`](IrqFdRoute::enable)
    /// is called, or can be drained by waiting on the event directly.
    fn disable(&self);
}

/// A live kernel-mediated MSI binding: a route that the hypervisor injects when
/// a caller-owned fd is signaled, decoupled from the concrete irqfd backend.
///
/// This is the interface an MSI sink's `bind_msi` (on `chipset_device`'s
/// `SignalMsi`) hands back. Dropping it releases the kernel routing resources
/// (disarms the irqfd, frees the GSI). [`IrqFdBinding`] adapts any
/// [`IrqFdRoute`] to this interface.
pub trait KernelMsiBinding: Send + Sync {
    /// Re-points this binding at `(address, data, devid)` in place — reusing the
    /// existing fd/GSI without rebinding the fd — iff the new address still
    /// routes to this binding's sink.
    ///
    /// Returns `true` if the routing entry was updated in place, or `false` if
    /// the address no longer decodes to this sink; on `false` the caller must
    /// drop this binding and rebind against the sink the address now resolves
    /// to. Keeping the fd assigned across an in-place update avoids the churn
    /// (and the racy deassign/reassign window) of a full rebind, which is the
    /// common case when a guest reprograms only the MSI vector/data.
    fn update(&self, devid: Option<u32>, address: u64, data: u32) -> bool;
}

/// Adapts an [`IrqFdRoute`] to the [`KernelMsiBinding`] interface.
///
/// The wrapped route already owns its GSI and the caller-supplied fd; an
/// in-place [`update`](KernelMsiBinding::update) is just another
/// [`enable`](IrqFdRoute::enable) (which re-points the routing entry without
/// touching the fd assignment), and dropping the route frees its kernel
/// resources.
pub struct IrqFdBinding(pub Box<dyn IrqFdRoute>);

impl KernelMsiBinding for IrqFdBinding {
    fn update(&self, devid: Option<u32>, address: u64, data: u32) -> bool {
        self.0.enable(address, data, devid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    struct ReentrantRoute {
        list: Arc<RetranslateInterruptsList>,
        retranslates: AtomicU32,
    }

    impl RetranslateInterrupts for ReentrantRoute {
        fn device_id(&self) -> u32 {
            assert!(self.list.routes.try_lock().is_some());
            0x10007
        }

        fn retranslate(&self) {
            self.retranslates.fetch_add(1, Ordering::SeqCst);
            assert!(self.list.routes.try_lock().is_some());
        }
    }

    #[test]
    fn invalidation_is_reentrant_and_preserves_full_identity() {
        let list = Arc::new(RetranslateInterruptsList::new());
        let route = Arc::new(ReentrantRoute {
            list: list.clone(),
            retranslates: AtomicU32::new(0),
        });
        let route_dyn = route.clone() as Arc<dyn RetranslateInterrupts>;
        list.register(&route_dyn);
        list.register(&route_dyn);
        list.invalidate(Some(7));
        assert_eq!(route.retranslates.load(Ordering::SeqCst), 0);
        list.invalidate(Some(0x10007));
        assert_eq!(route.retranslates.load(Ordering::SeqCst), 1);
        list.unregister(&route_dyn);
        list.invalidate(None);
        assert_eq!(route.retranslates.load(Ordering::SeqCst), 1);
    }
}
