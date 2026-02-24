// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and traits to model chipset devices, and associated chipset services.

#![forbid(unsafe_code)]

/// Implemented by any device that is considered part of the guest's "chipset"
/// (insofar as it exists on one or more system busses).
//
// DEVNOTE: In the past, `ChipsetDevice` included explicit bounds for things
// like `Inspect` and `SaveRestore`, traits that all OpenVMM devices are
// expected to implement. While this works, it comes with a few drawbacks:
// - It adds some heavy dependencies to an otherwise lightweight crate
// - It adds substantial boilerplate when implementing "test" devices, as those
//   traits need to be implemented + stubbed-out with `todo!()`s
pub trait ChipsetDevice: 'static + Send /* see DEVNOTE before adding bounds */ {
    /// Optionally returns a trait object to send IO port intercepts to.
    #[inline(always)]
    fn supports_pio(&mut self) -> Option<&mut dyn pio::PortIoIntercept> {
        None
    }

    /// Optionally returns a trait object to send MMIO port intercepts to.
    #[inline(always)]
    fn supports_mmio(&mut self) -> Option<&mut dyn mmio::MmioIntercept> {
        None
    }

    /// Optionally returns a trait object to send PCI config space accesses to.
    #[inline(always)]
    fn supports_pci(&mut self) -> Option<&mut dyn pci::PciConfigSpace> {
        None
    }

    /// Optionally returns a trait object to send poll requests to.
    #[inline(always)]
    fn supports_poll_device(&mut self) -> Option<&mut dyn poll_device::PollDevice> {
        None
    }

    /// Optionally returns a trait object to send interrupt line changes to.
    #[inline(always)]
    fn supports_line_interrupt_target(
        &mut self,
    ) -> Option<&mut dyn interrupt::LineInterruptTarget> {
        None
    }

    /// Optionally returns a trait object to send EOI requests to.
    #[inline(always)]
    fn supports_handle_eoi(&mut self) -> Option<&mut dyn interrupt::HandleEoi> {
        None
    }

    /// Optionally returns a trait object with which to acknowledge PIC
    /// interrupts.
    #[inline(always)]
    fn supports_acknowledge_pic_interrupt(
        &mut self,
    ) -> Option<&mut dyn interrupt::AcknowledgePicInterrupt> {
        None
    }

    /// Optionally returns a trait object which implements TDISP host
    /// communication.
    #[inline(always)]
    fn supports_tdisp(&mut self) -> Option<&mut dyn tdisp::TdispHostDeviceTarget> {
        None
    }
}

/// Shared by `mmio` and `pio`
macro_rules! io_region {
    ($register:ident, $control:ident, $addr:ty) => {
        /// A trait to register device-specific IO intercept regions.
        pub trait $register {
            /// Registers a new IO region of the given length.
            fn new_io_region(&mut self, region_name: &str, len: $addr) -> Box<dyn $control>;
        }

        /// A trait to map/unmap a device-specific IO memory region.
        pub trait $control: Send + Sync {
            /// Return the region's name.
            fn region_name(&self) -> &str;

            /// Enables the IO region at the given address.
            ///
            /// This method will never fail, as devices are not expected to gracefully
            /// handle the case where an IO region overlaps with an existing region.
            fn map(&mut self, addr: $addr);

            /// Disables the IO region.
            fn unmap(&mut self);

            /// Return the currently mapped address.
            ///
            /// Returns `None` if the region is currently unmapped.
            fn addr(&self) -> Option<$addr>;

            /// Return the length of the region.
            fn len(&self) -> $addr;

            /// Return the offset of `addr` from the region's base address.
            ///
            /// Returns `None` if the provided `addr` is outside of the memory
            /// region, or the region is currently unmapped.
            ///
            /// # Example
            ///
            /// ```ignore
            /// let foo_region = register.new_io_region("foo", 0x10);
            /// foo_region.map(0x1000);
            /// assert_eq!(foo_region.offset_of(0x1003), Some(3));
            /// assert_eq!(foo_region.offset_of(0x900), None);
            /// assert_eq!(foo_region.offset_of(0x1020), None);
            /// foo_region.unmap();
            /// assert_eq!(foo_region.offset_of(0x1003), None);
            /// ```
            fn offset_of(&self, addr: $addr) -> Option<$addr>;
        }

        // DEVNOTE: we explicitly want to implement Inspect using trait method
        // (as opposed to adding a `: Inspect` bound) so we can ensure a
        // consistent inspect tree regardless of backing implementation.
        impl inspect::Inspect for dyn $control {
            fn inspect(&self, req: inspect::Request<'_>) {
                req.respond()
                    .field("name", self.region_name())
                    .hex("len", self.len())
                    .field("addr", self.addr().map(inspect::AsHex));
            }
        }
    };
}

pub mod interrupt;
pub mod io;
pub mod mmio;
pub mod pci;
pub mod pio;
pub mod poll_device;
