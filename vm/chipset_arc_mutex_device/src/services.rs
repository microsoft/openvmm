// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Chipset-facing traits required to wire-up a Arc + Mutex backed
//! [`ChipsetDevice`](chipset_device::ChipsetDevice).

use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::RegisterPortIoIntercept;

/// Compile-time type metadata bundle specified by concrete [`ChipsetServices`]
/// implementations.
///
/// If an implementation doesn't support a particular service, the service's
/// corresponding type should be set to [`Unimplemented`].
///
/// Rather than having `ChipsetServices` directly encode all these type (and
/// make the type signatures for the `supports_XXX` methods absolutely
/// _nightmarish_), we split them all out into their own trait, which can be
/// references via a single type parameter.
pub trait ChipsetServicesMeta {
    // DEVNOTE: ideally, these would all have a default `= Unimplemented`, but
    // associated type defaults are still unstable...
    //
    /// Concrete type that impls `RegisterMmioIntercept`
    type RegisterMmioIntercept: RegisterMmioIntercept;
    /// Concrete type that impls `RegisterPortIoIntercept`
    type RegisterPortIoIntercept: RegisterPortIoIntercept;
}

/// The intermediary that allows a device to wire itself up to various VM
/// chipset services.
pub trait ChipsetServices {
    /// A bundle of associated types used by the concrete implementation.
    type M: ChipsetServicesMeta;

    /// Support for MMIO intercepts.
    #[inline(always)]
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioInterceptServices<M = Self::M>> {
        None
    }

    /// Support for Port IO intercepts.
    #[inline(always)]
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoInterceptServices<M = Self::M>> {
        None
    }

    /// Support for PCI configuration space.
    #[inline(always)]
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpaceServices<M = Self::M>> {
        None
    }

    /// Support for poll.
    #[inline(always)]
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDeviceServices<M = Self::M>> {
        None
    }
}

/// Implemented by chipsets that can support [`MmioIntercept`] devices.
///
/// [`MmioIntercept`]: chipset_device::mmio::MmioIntercept
pub trait MmioInterceptServices: ChipsetServices {
    /// Obtain an instance of [`RegisterMmioIntercept`]
    fn register_mmio(&self) -> <Self::M as ChipsetServicesMeta>::RegisterMmioIntercept;

    /// Return `true` if any [`MmioInterceptServices`] method has been invoked.
    fn is_being_used(&self) -> bool;
}

/// Implemented by chipsets that can support [`PciConfigSpace`] devices.
///
/// [`PciConfigSpace`]: chipset_device::pci::PciConfigSpace
pub trait PciConfigSpaceServices: ChipsetServices {
    /// Register the device at the specified (bus, device, function)
    fn register_static_pci(&mut self, bus: u8, device: u8, function: u8);

    /// Return `true` if any [`PciConfigSpaceServices`] method has been invoked.
    fn is_being_used(&self) -> bool;
}

/// Implemented by chipsets that can support [`PortIoIntercept`] devices.
///
/// [`PortIoIntercept`]: chipset_device::pio::PortIoIntercept
pub trait PortIoInterceptServices: ChipsetServices {
    /// Obtain an instance of [`RegisterPortIoIntercept`]
    fn register_pio(&self) -> <Self::M as ChipsetServicesMeta>::RegisterPortIoIntercept;

    /// Return `true` if any [`PortIoInterceptServices`] method has been invoked.
    fn is_being_used(&self) -> bool;
}

/// Implemented by chipsets that can support [`PollDevice`] devices.
///
/// [`PollDevice`]: chipset_device::poll_device::PollDevice
pub trait PollDeviceServices: ChipsetServices {
    /// Register for asynchronous polling.
    fn register_poll(&mut self);

    /// Return `true` if [`Self::register_poll`] has been invoked.
    fn is_being_used(&self) -> bool;
}

/// A placeholder type to represent a bit of unimplemented functionality.
pub enum Unimplemented {}

impl RegisterMmioIntercept for Unimplemented {
    fn new_io_region(
        &mut self,
        _: &str,
        _: u64,
    ) -> Box<dyn chipset_device::mmio::ControlMmioIntercept> {
        unreachable!()
    }
}

impl RegisterPortIoIntercept for Unimplemented {
    fn new_io_region(
        &mut self,
        _: &str,
        _: u16,
    ) -> Box<dyn chipset_device::pio::ControlPortIoIntercept> {
        unreachable!()
    }
}
