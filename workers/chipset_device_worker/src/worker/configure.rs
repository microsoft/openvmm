// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device_resources::ConfigureChipsetDevice;

pub(super) struct RemoteConfigureChipsetDevice {}

impl ConfigureChipsetDevice for RemoteConfigureChipsetDevice {
    fn new_line(
        &mut self,
        _id: chipset_device_resources::LineSetId,
        _name: &str,
        _vector: u32,
    ) -> vmcore::line_interrupt::LineInterrupt {
        todo!()
    }

    fn add_line_target(
        &mut self,
        _id: chipset_device_resources::LineSetId,
        _source_range: std::ops::RangeInclusive<u32>,
        _target_start: u32,
    ) {
        todo!()
    }

    fn omit_saved_state(&mut self) {
        todo!()
    }
}

pub(super) struct RemoteRegisterMmio {}

impl RegisterMmioIntercept for RemoteRegisterMmio {
    fn new_io_region(&mut self, _region_name: &str, _len: u64) -> Box<dyn ControlMmioIntercept> {
        todo!()
    }
}

pub(super) struct RemoteRegisterPio {}

impl RegisterPortIoIntercept for RemoteRegisterPio {
    fn new_io_region(&mut self, _region_name: &str, _len: u16) -> Box<dyn ControlPortIoIntercept> {
        todo!()
    }
}
