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
    fatal_policy: FatalErrorPolicy,
}

#[derive(Clone)]
pub enum FatalErrorPolicy {
    /// Panic the process, running the given closure immediately before panicking.
    Panic(Arc<dyn Fn() + Send + Sync>),
    /// Convert the failure to a debugger break, and send the error over the
    /// given channel.
    DebugBreak(mesh::Sender<Box<dyn std::error::Error + Send + Sync>>),
}

impl ChipsetPlusSynic {
    #[expect(missing_docs)]
    pub fn new(
        synic_ports: Arc<SynicPorts>,
        chipset: Arc<Chipset>,
        fatal_policy: FatalErrorPolicy,
    ) -> Self {
        Self {
            synic_ports,
            chipset,
            fatal_policy,
        }
    }
}

impl CpuIo for ChipsetPlusSynic {
    fn is_mmio(&self, address: u64) -> bool {
        self.chipset.is_mmio(address)
    }

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

    fn read_mmio(&self, vp: VpIndex, address: u64, data: &mut [u8]) -> impl Future<Output = ()> {
        self.chipset.mmio_read(vp.index(), address, data)
    }

    fn write_mmio(&self, vp: VpIndex, address: u64, data: &[u8]) -> impl Future<Output = ()> {
        self.chipset.mmio_write(vp.index(), address, data)
    }

    fn read_io(&self, vp: VpIndex, port: u16, data: &mut [u8]) -> impl Future<Output = ()> {
        self.chipset.io_read(vp.index(), port, data)
    }

    fn write_io(&self, vp: VpIndex, port: u16, data: &[u8]) -> impl Future<Output = ()> {
        self.chipset.io_write(vp.index(), port, data)
    }

    #[track_caller]
    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> virt::VpHaltReason {
        tracing::error!(
            err = error.as_ref() as &dyn std::error::Error,
            "fatal error"
        );
        match &self.fatal_policy {
            FatalErrorPolicy::Panic(prep) => {
                prep();
                panic!("fatal error: {}", error)
            }
            FatalErrorPolicy::DebugBreak(channel) => {
                channel.send(error);
                virt::VpHaltReason::SingleStep
            }
        }
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
