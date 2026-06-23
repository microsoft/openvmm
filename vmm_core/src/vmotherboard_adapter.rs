// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Glue code to adapt OpenVMM-specific platform APIs to the types/traits
//! required by `vmotherboard`.

use crate::partition_unit::Halt;
use std::sync::Arc;
use virt::VpIndex;
use virt::io::CpuIo;
use vmm_core_defs::HaltReason;
use vmotherboard::Chipset;

/// This struct adds the necessary `CpuIo` implementation to a `Chipset`, so it
/// can be used directly in the VP dispatch loop and passed to the processor
/// implementations.
#[derive(Clone)]
pub struct AdaptedChipset {
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

impl AdaptedChipset {
    /// Create a new `AdaptedChipset` from a `Chipset` and a `FatalErrorPolicy`.
    pub fn new(chipset: Arc<Chipset>, fatal_policy: FatalErrorPolicy) -> Self {
        Self {
            chipset,
            fatal_policy,
        }
    }
}

impl CpuIo for AdaptedChipset {
    fn is_mmio(&self, address: u64) -> bool {
        self.chipset.is_mmio(address)
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        self.chipset.acknowledge_pic_interrupt()
    }

    fn handle_eoi(&self, irq: u32) {
        self.chipset.handle_eoi(irq)
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
