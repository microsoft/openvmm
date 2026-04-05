// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! irqfd support for the mshv hypervisor backend.
//!
//! This module implements [`IrqFd`] and [`IrqFdRoute`] for mshv, allowing
//! eventfds to be registered with the mshv kernel module for direct MSI
//! injection into the guest without a userspace transition.

// UNSAFETY: Calling mshv ioctls for irqfd and MSI routing.
#![expect(unsafe_code)]

use anyhow::Context;
use mshv_bindings::MSHV_IRQFD_BIT_DEASSIGN;
use mshv_bindings::mshv_user_irq_entry;
use mshv_bindings::mshv_user_irq_table;
use mshv_bindings::mshv_user_irqfd;
use mshv_ioctls::VmFd;
use pal_event::Event;
use parking_lot::Mutex;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use virt::irqfd::IrqFd;
use virt::irqfd::IrqFdRoute;

const NUM_GSIS: usize = 2048;

/// MSI routing state for a single GSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GsiState {
    /// GSI slot is not allocated.
    Unallocated,
    /// GSI is allocated but has no active routing.
    Disabled,
    /// GSI is allocated with an active MSI route.
    Enabled(MsiRoute),
}

/// An MSI routing entry (address + data) for a GSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MsiRoute {
    address_lo: u32,
    address_hi: u32,
    data: u32,
}

/// Shared GSI routing and irqfd state for an mshv partition.
///
/// Manages GSI allocation and the MSI routing table. All updates to the routing
/// table are pushed to the kernel atomically via `MSHV_SET_MSI_ROUTING`.
#[derive(Debug)]
struct SharedGsiState {
    gsi_states: Mutex<Box<[GsiState; NUM_GSIS]>>,
    vmfd: Arc<VmFd>,
}

impl SharedGsiState {
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
    /// kernel.
    fn set_gsi_route(&self, gsi: u32, route: Option<MsiRoute>) -> anyhow::Result<()> {
        let mut states = self.gsi_states.lock();
        let state = &mut states[gsi as usize];
        assert!(
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
        *state = new_state;

        Self::push_routing_table(&self.vmfd, &states)
    }

    /// Rebuilds and pushes the full routing table to the kernel.
    fn push_routing_table(vmfd: &VmFd, states: &[GsiState; NUM_GSIS]) -> anyhow::Result<()> {
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
    fn register_irqfd(&self, event: &Event, gsi: u32) -> anyhow::Result<()> {
        let irqfd_arg = mshv_user_irqfd {
            fd: event.as_fd().as_raw_fd(),
            resamplefd: 0,
            gsi,
            flags: 0,
        };
        // SAFETY: Calling the MSHV_IRQFD ioctl with a valid fd and properly
        // initialized mshv_user_irqfd struct.
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
    fn unregister_irqfd(&self, event: &Event, gsi: u32) -> anyhow::Result<()> {
        let irqfd_arg = mshv_user_irqfd {
            fd: event.as_fd().as_raw_fd(),
            resamplefd: 0,
            gsi,
            flags: 1 << MSHV_IRQFD_BIT_DEASSIGN,
        };
        // SAFETY: Calling the MSHV_IRQFD ioctl with a valid fd and properly
        // initialized mshv_user_irqfd struct with DEASSIGN flag.
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
/// This wraps shared state containing the GSI routing table and VM file
/// descriptor. Routes created via [`IrqFd::new_irqfd_route`] hold their own
/// reference to the shared state.
#[derive(Debug)]
pub struct MshvIrqFdState {
    shared: Arc<SharedGsiState>,
}

impl MshvIrqFdState {
    /// Creates a new irqfd state for the given VM.
    pub fn new(vmfd: Arc<VmFd>) -> Self {
        Self {
            shared: Arc::new(SharedGsiState {
                gsi_states: Mutex::new(Box::new([GsiState::Unallocated; NUM_GSIS])),
                vmfd,
            }),
        }
    }
}

impl IrqFd for MshvIrqFdState {
    fn new_irqfd_route(&self, event: &Event) -> anyhow::Result<Box<dyn IrqFdRoute>> {
        let gsi = self
            .shared
            .alloc_gsi()
            .context("no free GSIs available for irqfd")?;

        if let Err(e) = self.shared.register_irqfd(event, gsi) {
            self.shared.free_gsi(gsi);
            return Err(e);
        }

        Ok(Box::new(MshvIrqFdRoute {
            shared: self.shared.clone(),
            gsi,
            event: event.clone(),
        }))
    }
}

/// A registered irqfd route for a single GSI.
///
/// When dropped, unregisters the irqfd and frees the GSI.
struct MshvIrqFdRoute {
    shared: Arc<SharedGsiState>,
    gsi: u32,
    event: Event,
}

impl IrqFdRoute for MshvIrqFdRoute {
    fn set_msi(&self, address: u64, data: u32) -> anyhow::Result<()> {
        self.shared.set_gsi_route(
            self.gsi,
            Some(MsiRoute {
                address_lo: address as u32,
                address_hi: (address >> 32) as u32,
                data,
            }),
        )
    }

    fn clear_msi(&self) -> anyhow::Result<()> {
        self.shared.set_gsi_route(self.gsi, None)
    }
}

impl Drop for MshvIrqFdRoute {
    fn drop(&mut self) {
        // Clear routing first, then unregister irqfd.
        if let Err(e) = self.shared.set_gsi_route(self.gsi, None) {
            tracing::warn!(
                gsi = self.gsi,
                error = &*e as &dyn std::error::Error,
                "failed to clear GSI route on drop"
            );
        }
        // Only free the GSI if unregister succeeds. If it fails, the kernel
        // still has the irqfd registered, so reusing this GSI could cause
        // stale interrupt injection.
        match self.shared.unregister_irqfd(&self.event, self.gsi) {
            Ok(()) => self.shared.free_gsi(self.gsi),
            Err(e) => {
                tracing::warn!(
                    gsi = self.gsi,
                    error = &*e as &dyn std::error::Error,
                    "failed to unregister irqfd on drop; leaving GSI allocated to avoid reuse"
                );
            }
        }
    }
}

/// Pushes the full MSI routing table to the mshv kernel module.
///
/// This constructs the variable-length `mshv_user_irq_table` struct and calls
/// the `MSHV_SET_MSI_ROUTING` ioctl.
fn set_msi_routing_ioctl(vmfd: &VmFd, entries: &[mshv_user_irq_entry]) -> anyhow::Result<()> {
    let header_size = size_of::<mshv_user_irq_table>();
    let total_size = header_size + size_of_val(entries);
    let layout = std::alloc::Layout::from_size_align(total_size, align_of::<mshv_user_irq_table>())
        .context("invalid layout for MSI routing table")?;

    // SAFETY: layout has non-zero size (header is always > 0) and correct
    // alignment for mshv_user_irq_table. We zero-initialize the allocation
    // and fill it with valid data before passing to the ioctl.
    unsafe {
        let buf = std::alloc::alloc_zeroed(layout);
        if buf.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        let table = &mut *buf.cast::<mshv_user_irq_table>();
        table.nr = entries.len() as u32;

        if !entries.is_empty() {
            let dst = table.entries.as_mut_slice(entries.len());
            dst.copy_from_slice(entries);
        }

        let ret = libc::ioctl(
            vmfd.as_raw_fd(),
            mshv_ioctls::MSHV_SET_MSI_ROUTING() as _,
            buf,
        );

        std::alloc::dealloc(buf, layout);

        if ret < 0 {
            return Err(std::io::Error::last_os_error())
                .context("MSHV_SET_MSI_ROUTING ioctl failed");
        }
    }

    Ok(())
}
