// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types related to supporting an interrupt controller.

pub use x86defs::apic::DeliveryMode;

use hvdef::HvInterruptType;
use inspect::Inspect;
use parking_lot::Mutex;
use std::fmt::Debug;
use x86defs::msi::MsiAddress;
use x86defs::msi::MsiData;

/// Trait for an interrupt controller that can deliver MSIs from an IO-APIC.
///
/// This is used to map line interrupts, which may be level triggered. Some
/// hypervisors (notably KVM) will not generate EOI exits for a
/// level-triggered interrupt request unless the request has been registered
/// as a route on one of the IO-APIC IRQs.
///
/// `irq` must be less than [`IRQ_LINES`] for both methods; callers (the
/// IO-APIC device) guarantee this, and implementations may panic otherwise.
pub trait IoApicRouting: Send + Sync {
    /// Sets the associated interrupt request for the given irq.
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>);

    /// Asserts the given irq, using the route established by `set_irq_route`.
    fn assert_irq(&self, irq: u8);
}

/// Trait for controlling interrupt states on a GICv3 interrupt controller.
pub trait ControlGic: Send + Sync {
    /// Sets the assertion state of a GICv3 SPI.
    fn set_spi_irq(&self, irq_id: u32, high: bool);
}

// The number of IRQ lines for the interrupt controller.
pub const IRQ_LINES: usize = 24;

/// A message-signaled interrupt request.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub struct MsiRequest {
    /// The MSI address.
    #[inspect(hex)]
    pub address: u64,
    /// The data payload.
    #[inspect(hex)]
    pub data: u32,
}

impl MsiRequest {
    /// Creates a new MSI request for an x86 system.
    pub fn new_x86(
        mode: DeliveryMode,
        destination: u32,
        is_logical_destination: bool,
        vector: u8,
        is_level_triggered: bool,
    ) -> Self {
        let address = MsiAddress::new()
            .with_address(x86defs::msi::MSI_ADDRESS)
            .with_redirection_hint(mode == DeliveryMode::LOWEST_PRIORITY)
            .with_virt_destination(destination as u16)
            .with_destination_mode_logical(is_logical_destination);

        let data = MsiData::new()
            .with_vector(vector)
            .with_delivery_mode(mode.0 & 0x7)
            .with_assert(is_level_triggered)
            .with_trigger_mode_level(is_level_triggered);

        Self {
            address: u32::from(address).into(),
            data: data.into(),
        }
    }

    /// Interprets the MSI address and data as an x86 MSI request.
    pub fn as_x86(&self) -> (MsiAddress, MsiData) {
        (
            MsiAddress::from(self.address as u32),
            MsiData::from(self.data),
        )
    }

    /// Constructs an interrupt control for sending this interrupt request to a
    /// Microsoft hypervisor.
    ///
    /// Note that this may produce an invalid interrupt control that the
    /// hypervisor will reject.
    pub fn hv_x86_interrupt_control(&self) -> hvdef::HvInterruptControl {
        let (address, data) = self.as_x86();
        let ty = match DeliveryMode(data.delivery_mode()) {
            DeliveryMode::FIXED => HvInterruptType::HvX64InterruptTypeFixed,
            DeliveryMode::LOWEST_PRIORITY => HvInterruptType::HvX64InterruptTypeLowestPriority,
            DeliveryMode::SMI => HvInterruptType::HvX64InterruptTypeSmi,
            DeliveryMode::REMOTE_READ => HvInterruptType::HvX64InterruptTypeRemoteRead,
            DeliveryMode::NMI => HvInterruptType::HvX64InterruptTypeNmi,
            DeliveryMode::INIT => HvInterruptType::HvX64InterruptTypeInit,
            DeliveryMode::SIPI => HvInterruptType::HvX64InterruptTypeSipi,
            // Use an invalid interrupt type to force the hypervisor to reject
            // this. Since other combinations of interrupt parameters are
            // invalid and we are deferring that validation to the hypervisor,
            // there is no reason to special case this one and add a failure
            // path from this function.
            _ => HvInterruptType(!0),
        };
        hvdef::HvInterruptControl::new()
            .with_interrupt_type(ty)
            .with_x86_level_triggered(data.trigger_mode_level())
            .with_x86_logical_destination_mode(address.destination_mode_logical())
    }
}

/// A set of IRQ routes.
///
/// This is used to implement [`IoApicRouting`] when the backing hypervisor does
/// not require such routes internally.
#[derive(Debug, Inspect)]
pub struct IrqRoutes {
    #[inspect(
        with = "|x| inspect::adhoc(|req| inspect::iter_by_index(x.lock().iter()).inspect(req))"
    )]
    routes: Mutex<Vec<Option<MsiRequest>>>,
}

impl Default for IrqRoutes {
    fn default() -> Self {
        Self::new()
    }
}

impl IrqRoutes {
    pub fn new() -> Self {
        let routes = vec![None; IRQ_LINES];
        Self {
            routes: Mutex::new(routes),
        }
    }

    /// Sets the associated interrupt request for the given irq.
    pub fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        self.routes.lock()[irq as usize] = request;
    }

    /// Asserts the given irq, using the route established by `set_irq_route`.
    ///
    /// Calls `assert` to deliver the interrupt.
    pub fn assert_irq(&self, irq: u8, assert: impl FnOnce(MsiRequest)) {
        let request = self.routes.lock()[irq as usize];
        match request {
            Some(request) => {
                assert(request);
            }
            None => {
                tracelimit::warn_ratelimited!(irq, "irq for masked interrupt");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    // Test: Build x86 MSI requests for several delivery modes, a 15-bit
    //       destination, and both destination and trigger modes, then decode
    //       them.
    // Expected: The MSI address and data hold exactly the requested fields,
    //       with the redirection hint set only for lowest-priority delivery.
    #[test]
    fn x86_msi_request_encodes_requested_fields() {
        // Each case varies every field relative to the others:
        // - 0x12 fits the classic 8-bit destination field. 0x7fff (the
        //   largest) and 0x1234 exceed it and use the 15-bit virtual
        //   destination that Hyper-V and KVM split across the destination
        //   and extended-destination fields.
        // - Lowest priority is the only mode that sets the redirection hint;
        //   fixed and NMI must leave it clear.
        // - Logical/physical destination and level/edge trigger each appear
        //   both ways. 0x02 is the NMI vector; 0x31 and 0xfe are ordinary
        //   vectors, with 0xfe near the top of the 8-bit range.
        for (mode, destination, logical, vector, level) in [
            (DeliveryMode::FIXED, 0x12, false, 0x31, false),
            (DeliveryMode::LOWEST_PRIORITY, 0x7fff, true, 0xfe, true),
            (DeliveryMode::NMI, 0x1234, false, 0x02, false),
        ] {
            let request = MsiRequest::new_x86(mode, destination, logical, vector, level);

            let (address, data) = request.as_x86();
            assert_eq!(request.address >> 32, 0);
            assert_eq!(address.address(), x86defs::msi::MSI_ADDRESS);
            assert_eq!(address.virt_destination(), destination as u16);
            assert_eq!(address.destination_mode_logical(), logical);
            assert_eq!(
                address.redirection_hint(),
                mode == DeliveryMode::LOWEST_PRIORITY
            );
            assert_eq!(data.vector(), vector);
            assert_eq!(DeliveryMode(data.delivery_mode()), mode);
            assert_eq!(data.trigger_mode_level(), level);
            assert_eq!(data.assert(), level);
        }
    }

    // Test: Convert MSI requests with each delivery mode to a Microsoft
    //       hypervisor interrupt control.
    // Expected: Each architectural delivery mode maps to the matching
    //       hypervisor interrupt type with the request's trigger and
    //       destination modes; ExtINT, which has no MSI meaning, maps to an
    //       invalid type so the hypervisor rejects it.
    #[test]
    fn hv_interrupt_control_maps_delivery_modes() {
        for (mode, expected) in [
            (
                DeliveryMode::FIXED,
                HvInterruptType::HvX64InterruptTypeFixed,
            ),
            (
                DeliveryMode::LOWEST_PRIORITY,
                HvInterruptType::HvX64InterruptTypeLowestPriority,
            ),
            (DeliveryMode::SMI, HvInterruptType::HvX64InterruptTypeSmi),
            (
                DeliveryMode::REMOTE_READ,
                HvInterruptType::HvX64InterruptTypeRemoteRead,
            ),
            (DeliveryMode::NMI, HvInterruptType::HvX64InterruptTypeNmi),
            (DeliveryMode::INIT, HvInterruptType::HvX64InterruptTypeInit),
            (DeliveryMode::SIPI, HvInterruptType::HvX64InterruptTypeSipi),
            (DeliveryMode::EXTINT, HvInterruptType(!0)),
        ] {
            let control = MsiRequest::new_x86(mode, 3, true, 0x40, true).hv_x86_interrupt_control();

            assert_eq!(control.interrupt_type(), expected, "{mode:?}");
            assert!(control.x86_level_triggered(), "{mode:?}");
            assert!(control.x86_logical_destination_mode(), "{mode:?}");
        }

        let edge_physical = MsiRequest::new_x86(DeliveryMode::FIXED, 3, false, 0x40, false)
            .hv_x86_interrupt_control();
        assert!(!edge_physical.x86_level_triggered());
        assert!(!edge_physical.x86_logical_destination_mode());
    }

    // Test: Assert IRQs with no route, with a route, after replacing the
    //       route, and after removing it.
    // Expected: Only routed IRQs are delivered, using the most recently set
    //       route, and other IRQs are unaffected.
    #[test]
    fn irq_routes_deliver_only_the_current_route() {
        let routes = IrqRoutes::new();
        let first = MsiRequest::new_x86(DeliveryMode::FIXED, 1, false, 0x30, true);
        let second = MsiRequest::new_x86(DeliveryMode::FIXED, 2, false, 0x31, true);
        let assert_irq = |irq| {
            let mut delivered = None;
            routes.assert_irq(irq, |request| delivered = Some(request));
            delivered
        };

        assert_eq!(assert_irq(5), None);

        routes.set_irq_route(5, Some(first));
        assert_eq!(assert_irq(5), Some(first));
        assert_eq!(assert_irq(6), None);

        routes.set_irq_route(5, Some(second));
        assert_eq!(assert_irq(5), Some(second));

        routes.set_irq_route(5, None);
        assert_eq!(assert_irq(5), None);

        routes.set_irq_route((IRQ_LINES - 1) as u8, Some(first));
        assert_eq!(assert_irq((IRQ_LINES - 1) as u8), Some(first));
    }
}
