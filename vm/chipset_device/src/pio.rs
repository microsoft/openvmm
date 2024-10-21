// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Port IO intercepts

use crate::io::IoResult;
use crate::ChipsetDevice;
use std::ops::RangeInclusive;

/// Implemented by devices which use port IO intercepts.
pub trait PortIoIntercept: ChipsetDevice {
    /// Dispatch an IO port read to the device with the given address.
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult;
    /// Dispatch an IO port write to the device with the given address.
    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult;

    /// Report a set of static io port regions (region_name, port_range) that
    /// cannot be remapped at runtime and are always registered.
    ///
    /// _Note:_ This is a convenience method that makes it easy for simple
    /// devices to declare some fixed IO regions without having to do through
    /// the rigamarole of obtaining a reference to an instance of
    /// [`RegisterPortIoIntercept`] + manually registering fixed ranges as part
    /// of device init.
    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[]
    }
}

io_region!(RegisterPortIoIntercept, ControlPortIoIntercept, u16);

/// A zero sized type that has a no-op `impl` of [`RegisterPortIoIntercept`].
///
/// As the name suggests, this type should be used when a [`ChipsetDevice`] is
/// hosted outside of a traditional "chipset" context, where some external code
/// is responsible for managing the device's port IO intercepts.
///
/// That said, if you find yourself reaching for this type (outside the context
/// of a test), you're probably doing something wrong. Consider implementing a
/// proper chipset to host the device on instead.
pub struct ExternallyManagedPortIoIntercepts;

impl RegisterPortIoIntercept for ExternallyManagedPortIoIntercepts {
    fn new_io_region(&mut self, _region_name: &str, _len: u16) -> Box<dyn ControlPortIoIntercept> {
        Box::new(())
    }
}

impl ControlPortIoIntercept for () {
    fn region_name(&self) -> &str {
        "(noop)"
    }
    fn map(&mut self, _addr: u16) {}
    fn unmap(&mut self) {}
    fn addr(&self) -> Option<u16> {
        None
    }
    fn len(&self) -> u16 {
        0
    }
    fn offset_of(&self, _addr: u16) -> Option<u16> {
        None
    }
}
