// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Glue code to adapt OpenVMM-specific platform APIs to the types/traits
//! required by `vmotherboard`.

use crate::partition_unit::Halt;
use crate::synic::SynicPorts;
use hvdef::Vtl;
use std::sync::Arc;
use virt::VpIndex;
use virt::io::CpuIo;
use vmm_core_defs::HaltReason;
use vmotherboard::Chipset;

#[expect(missing_docs)]
#[derive(Clone)]
pub struct ChipsetPlusSynic {
    pub synic_ports: Arc<SynicPorts>,
    pub chipset: Arc<Chipset>,
}

impl ChipsetPlusSynic {
    #[expect(missing_docs)]
    pub fn new(synic_ports: Arc<SynicPorts>, chipset: Arc<Chipset>) -> Self {
        Self {
            synic_ports,
            chipset,
        }
    }
}

impl CpuIo for ChipsetPlusSynic {
    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        self.chipset.acknowledge_pic_interrupt()
    }

    fn handle_eoi(&self, irq: u32) {
        self.chipset.handle_eoi(irq)
    }

    fn signal_synic_event(&self, vtl: Vtl, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.synic_ports.on_signal_event(vtl, connection_id, flag)
    }

    fn post_synic_message(
        &self,
        vtl: Vtl,
        connection_id: u32,
        secure: bool,
        message: &[u8],
    ) -> hvdef::HvResult<()> {
        self.synic_ports
            .on_post_message(vtl, connection_id, secure, message)
    }

    fn read_mmio(
        &self,
        vp: VpIndex,
        address: u64,
        data: &mut [u8],
    ) -> impl std::future::Future<Output = ()> {
        self.chipset.mmio_read(vp.index(), address, data)
    }

    fn write_mmio(
        &self,
        vp: VpIndex,
        address: u64,
        data: &[u8],
    ) -> impl std::future::Future<Output = ()> {
        self.chipset.mmio_write(vp.index(), address, data)
    }

    fn read_io(
        &self,
        vp: VpIndex,
        port: u16,
        data: &mut [u8],
    ) -> impl std::future::Future<Output = ()> {
        self.chipset.io_read(vp.index(), port, data)
    }

    fn write_io(
        &self,
        vp: VpIndex,
        port: u16,
        data: &[u8],
    ) -> impl std::future::Future<Output = ()> {
        self.chipset.io_write(vp.index(), port, data)
    }
}

impl vmotherboard::PowerEventHandler for Halt {
    fn on_power_event(&self, evt: vmotherboard::PowerEvent) {
        let reason = match evt {
            vmotherboard::PowerEvent::PowerOff => HaltReason::PowerOff,
            vmotherboard::PowerEvent::Reset => HaltReason::Reset,
            vmotherboard::PowerEvent::Hibernate => HaltReason::Hibernate,
        };
        self.halt(reason)
    }
}

impl vmotherboard::DebugEventHandler for Halt {
    fn on_debug_break(&self, vp: Option<u32>) {
        self.halt(HaltReason::DebugBreak { vp })
    }
}
