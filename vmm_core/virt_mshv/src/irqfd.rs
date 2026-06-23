// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! irqfd support for the mshv hypervisor backend.
//!
//! This module implements [`IrqFd`] and [`IrqFdRoute`] for mshv, allowing
//! eventfds to be registered with the mshv kernel module for direct MSI
//! injection into the guest without a userspace transition.

// UNSAFETY: Calling mshv ioctls for irqfd and MSI routing.
#![expect(unsafe_code)]

use crate::MshvPartitionInner;
use anyhow::Context;
use headervec::HeaderVec;
use mshv_bindings::MSHV_IRQFD_BIT_DEASSIGN;
use mshv_bindings::mshv_user_irq_entry;
use mshv_bindings::mshv_user_irqfd;
use pal_event::Event;
use parking_lot::Mutex;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use virt::irqfd::IrqFd;
use virt::irqfd::IrqFdRoute;

pub(crate) const NUM_GSIS: usize = 2048;

/// MSI routing state for a single GSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GsiState {
    /// GSI slot is not allocated.
    Unallocated,
    /// GSI is allocated but has no active routing.
    Disabled,
    /// GSI is allocated with an active MSI route.
    Enabled(MsiRoute),
}

/// An MSI routing entry (address + data) for a GSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MsiRoute {
    address_lo: u32,
    address_hi: u32,
    data: u32,
}

impl MshvPartitionInner {
    /// Allocates an unused GSI.
    fn alloc_gsi(&self) -> Option<u32> {
        let mut states = self.gsi_states.lock();
        let gsi = states
            .iter()
            .position(|state| matches!(state, GsiState::Unallocated))?;
        states[gsi] = GsiState::Disabled;
        Some(gsi as u32)
    }

    /// Frees an allocated GSI.
    fn free_gsi(&self, gsi: u32) {
        self.gsi_states.lock()[gsi as usize] = GsiState::Unallocated;
    }

    /// Sets the MSI routing for a GSI and pushes the full routing table to the
    /// kernel. Rolls back the in-memory state on ioctl failure.
    fn set_gsi_route(&self, gsi: u32, route: Option<MsiRoute>) -> anyhow::Result<()> {
        let mut states = self.gsi_states.lock();
        let state = &mut states[gsi as usize];
        anyhow::ensure!(
            !matches!(state, GsiState::Unallocated),
            "cannot set route for unallocated GSI {gsi}"
        );
        let new_state = match route {
            Some(r) => GsiState::Enabled(r),
            None => GsiState::Disabled,
        };
        if *state == new_state {
            return Ok(());
        }
        let old_state = *state;
        *state = new_state;

        if let Err(e) = Self::push_routing_table(&self.vmfd, &states) {
            // Roll back to keep in-memory state consistent with the kernel.
            states[gsi as usize] = old_state;
            return Err(e);
        }
        Ok(())
    }

    /// Rebuilds and pushes the full routing table to the kernel.
    fn push_routing_table(
        vmfd: &mshv_ioctls::VmFd,
        states: &[GsiState; NUM_GSIS],
    ) -> anyhow::Result<()> {
        let entries: Vec<mshv_user_irq_entry> = states
            .iter()
            .enumerate()
            .filter_map(|(gsi, state)| match state {
                GsiState::Enabled(route) => Some(mshv_user_irq_entry {
                    gsi: gsi as u32,
                    address_lo: route.address_lo,
                    address_hi: route.address_hi,
                    data: route.data,
                }),
                _ => None,
            })
            .collect();

        set_msi_routing_ioctl(vmfd, &entries).context("failed to set MSI routing")
    }

    /// Registers an eventfd as an irqfd for the given GSI.
    ///
    /// # Safety
    /// The caller must ensure that `event` outlives the irqfd registration
    /// (i.e., until `unregister_irqfd` is called). The kernel holds a
    /// reference to the underlying eventfd file descriptor.
    unsafe fn register_irqfd(&self, event: &Event, gsi: u32) -> anyhow::Result<()> {
        let irqfd_arg = mshv_user_irqfd {
            fd: event.as_fd().as_raw_fd(),
            resamplefd: 0,
            gsi,
            flags: 0,
        };
        // SAFETY: `self.vmfd` is valid because it is owned by
        // `MshvPartitionInner` which outlives this call. The `irqfd_arg`
        // struct is properly initialized on the stack. The caller guarantees
        // `event` will outlive the registration.
        let ret = unsafe {
            libc::ioctl(
                self.vmfd.as_raw_fd(),
                mshv_ioctls::MSHV_IRQFD() as _,
                std::ptr::from_ref(&irqfd_arg),
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error()).context("MSHV_IRQFD register failed");
        }
        Ok(())
    }

    /// Unregisters an eventfd from an irqfd for the given GSI.
    ///
    /// # Safety
    /// Must be called with the same `event` that was passed to
    /// `register_irqfd`. After this call returns successfully, the kernel
    /// no longer holds a reference to the eventfd.
    unsafe fn unregister_irqfd(&self, event: &Event, gsi: u32) -> anyhow::Result<()> {
        let irqfd_arg = mshv_user_irqfd {
            fd: event.as_fd().as_raw_fd(),
            resamplefd: 0,
            gsi,
            flags: 1 << MSHV_IRQFD_BIT_DEASSIGN,
        };
        // SAFETY: `self.vmfd` is valid because it is owned by
        // `MshvPartitionInner` which outlives this call. The caller guarantees
        // this is the same event passed to `register_irqfd`.
        let ret = unsafe {
            libc::ioctl(
                self.vmfd.as_raw_fd(),
                mshv_ioctls::MSHV_IRQFD() as _,
                std::ptr::from_ref(&irqfd_arg),
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error()).context("MSHV_IRQFD unregister failed");
        }
        Ok(())
    }
}

/// irqfd routing interface for an mshv partition.
///
/// Wraps `Arc<MshvPartitionInner>` to implement the [`IrqFd`] trait.
/// Routes created via [`IrqFd::new_irqfd_route`] hold their own
/// `Arc<MshvPartitionInner>` reference for GSI management.
pub(crate) struct MshvIrqFd {
    partition: Arc<MshvPartitionInner>,
}

impl MshvIrqFd {
    pub fn new(partition: Arc<MshvPartitionInner>) -> Self {
        Self { partition }
    }
}

impl IrqFd for MshvIrqFd {
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
        let gsi = self
            .partition
            .alloc_gsi()
            .context("no free GSIs available for irqfd")?;

        let event = Event::new();
        // SAFETY: `event` is moved into `MshvIrqFdRoute` below, which keeps
        // it alive until `Drop` calls `unregister_irqfd`.
        if let Err(e) = unsafe { self.partition.register_irqfd(&event, gsi) } {
            self.partition.free_gsi(gsi);
            return Err(e);
        }

        Ok(Box::new(MshvIrqFdRoute {
            partition: self.partition.clone(),
            gsi,
            event,
            armed: Mutex::new(true),
        }))
    }
}

/// A registered irqfd route for a single GSI.
///
/// When dropped, unregisters the irqfd and frees the GSI.
struct MshvIrqFdRoute {
    partition: Arc<MshvPartitionInner>,
    gsi: u32,
    event: Event,
    /// Whether the irqfd is currently armed (registered with the kernel).
    /// Serializes route updates and arm/disarm ioctls to prevent races.
    armed: Mutex<bool>,
}

impl MshvIrqFdRoute {
    fn disarm(&self) {
        let mut armed = self.armed.lock();
        if *armed {
            // SAFETY: `self.event` is the same event passed to `register_irqfd`.
            if let Err(e) = unsafe { self.partition.unregister_irqfd(&self.event, self.gsi) } {
                tracelimit::warn_ratelimited!(error = ?e, gsi = self.gsi, "failed to unregister irqfd");
                return;
            }
            *armed = false;
        }
    }
}

impl IrqFdRoute for MshvIrqFdRoute {
    fn event(&self) -> &Event {
        &self.event
    }

    fn enable(&self, address: u64, data: u32, _devid: Option<u32>) {
        let mut armed = self.armed.lock();
        let route = MsiRoute {
            address_lo: address as u32,
            address_hi: (address >> 32) as u32,
            data,
        };
        if let Err(e) = self.partition.set_gsi_route(self.gsi, Some(route)) {
            tracelimit::warn_ratelimited!(error = ?e, gsi = self.gsi, "failed to set GSI route");
            return;
        }
        if !*armed {
            // SAFETY: `self.event` is owned by this struct and will outlive
            // the registration (unregistered in `disarm` or `Drop`).
            if let Err(e) = unsafe { self.partition.register_irqfd(&self.event, self.gsi) } {
                tracelimit::warn_ratelimited!(error = ?e, gsi = self.gsi, "failed to register irqfd");
                return;
            }
            *armed = true;
        }
    }

    fn disable(&self) {
        // Just disarm the irqfd. The routing entry is inert without an
        // armed eventfd, so there's no need to remove it and trigger an
        // expensive MSHV_SET_MSI_ROUTING ioctl. On re-enable, if the
        // address/data haven't changed, set_gsi_route will be a no-op.
        self.disarm();
    }
}

impl Drop for MshvIrqFdRoute {
    fn drop(&mut self) {
        self.disarm();

        self.partition
            .set_gsi_route(self.gsi, None)
            .expect("failed to clear GSI route on drop");

        self.partition.free_gsi(self.gsi);
    }
}

/// Header for the MSI routing ioctl buffer, matching the layout of
/// `mshv_user_irq_table` but implementing `Copy` (unlike the bindgen type
/// which contains an `__IncompleteArrayField`).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct MsiRoutingHeader {
    nr: u32,
    rsvd: u32,
}

/// Pushes the full MSI routing table to the mshv kernel module.
///
/// This constructs the variable-length `mshv_user_irq_table` struct and calls
/// the `MSHV_SET_MSI_ROUTING` ioctl.
fn set_msi_routing_ioctl(
    vmfd: &mshv_ioctls::VmFd,
    entries: &[mshv_user_irq_entry],
) -> anyhow::Result<()> {
    let mut buf = HeaderVec::<MsiRoutingHeader, mshv_user_irq_entry, 0>::new(MsiRoutingHeader {
        nr: entries.len() as u32,
        rsvd: 0,
    });
    buf.extend_tail_from_slice(entries);

    // SAFETY: `vmfd` is valid (owned by `MshvPartitionInner`). `buf.as_ptr()`
    // points to a properly aligned buffer matching the layout of
    // `mshv_user_irq_table`: a header with `nr` and `rsvd` fields followed
    // by `nr` contiguous `mshv_user_irq_entry` values.
    let ret = unsafe {
        libc::ioctl(
            vmfd.as_raw_fd(),
            mshv_ioctls::MSHV_SET_MSI_ROUTING() as _,
            buf.as_ptr(),
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("MSHV_SET_MSI_ROUTING ioctl failed");
    }

    Ok(())
}
