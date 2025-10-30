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

/// An message-signaled interrupt request.
#[derive(Debug, Copy, Clone, Inspect)]
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
