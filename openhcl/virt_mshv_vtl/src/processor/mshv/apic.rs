// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! APIC emulation support for mshv-backed partitions.

#![cfg(guest_arch = "x86_64")]

use crate::processor::mshv::x64::HypervisorBackedX86;
use crate::processor::UhProcessor;
use crate::processor::UhRunVpError;
use crate::GuestVtl;
use crate::UhPartitionInner;
use crate::WakeReason;
use hcl::ioctl::x64::MshvX64;
use hcl::ioctl::ProcessorRunner;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvRegisterValue;
use hvdef::HvX64InterruptStateRegister;
use hvdef::HvX64PendingEventReg0;
use hvdef::HvX64PendingInterruptionRegister;
use hvdef::HvX64PendingInterruptionType;
use hvdef::HvX64RegisterName;
use inspect::Inspect;
use virt::io::CpuIo;
use virt::x86::MsrError;
use virt::Processor;
use virt::VpIndex;
use virt::VpInfo;
use virt_support_apic::ApicClient;
use virt_support_apic::ApicWork;
use virt_support_apic::LocalApic;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;
use x86defs::RFlags;

#[derive(Inspect)]
pub(super) struct UhApicState {
    lapic: LocalApic,
    #[inspect(debug)]
    vtl: GuestVtl,
    pub(super) halted: bool,
    pub(super) startup_suspend: bool,
    nmi_pending: bool,
}

impl UhApicState {
    pub fn new(lapic: LocalApic, vtl: GuestVtl, vp_info: &VpInfo) -> Self {
        Self {
            lapic,
            vtl,
            halted: false,
            nmi_pending: false,
            startup_suspend: vtl == GuestVtl::Vtl0 && !vp_info.is_bsp(),
        }
    }

    pub fn base_address(&self) -> Option<u64> {
        self.lapic.base_address()
    }

    pub fn mmio_write(
        &mut self,
        partition: &UhPartitionInner,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        vmtime: &VmTimeAccess,
        dev: &impl CpuIo,
        address: u64,
        data: &[u8],
    ) {
        self.lapic
            .access(&mut UhApicClient {
                partition,
                runner,
                dev,
                vmtime,
                vtl: self.vtl,
            })
            .mmio_write(address, data);
    }

    pub fn mmio_read(
        &mut self,
        partition: &UhPartitionInner,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        vmtime: &VmTimeAccess,
        dev: &impl CpuIo,
        address: u64,
        data: &mut [u8],
    ) {
        self.lapic
            .access(&mut UhApicClient {
                partition,
                runner,
                dev,
                vmtime,
                vtl: self.vtl,
            })
            .mmio_read(address, data);
    }

    pub fn msr_write(
        &mut self,
        partition: &UhPartitionInner,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        vmtime: &VmTimeAccess,
        dev: &impl CpuIo,
        msr: u32,
        value: u64,
    ) -> Result<(), MsrError> {
        self.lapic
            .access(&mut UhApicClient {
                partition,
                runner,
                dev,
                vmtime,
                vtl: self.vtl,
            })
            .msr_write(msr, value)
    }

    pub fn msr_read(
        &mut self,
        partition: &UhPartitionInner,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        vmtime: &VmTimeAccess,
        dev: &impl CpuIo,
        msr: u32,
    ) -> Result<u64, MsrError> {
        self.lapic
            .access(&mut UhApicClient {
                partition,
                runner,
                dev,
                vmtime,
                vtl: self.vtl,
            })
            .msr_read(msr)
    }

    pub fn halt(&mut self) {
        self.halted = true;
    }

    fn handle_interrupt(
        &mut self,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        notifications: &mut HvDeliverabilityNotificationsRegister,
        vector: u8,
    ) -> Result<(), UhRunVpError> {
        const NAMES: &[HvX64RegisterName] = &[
            HvX64RegisterName::Rflags,
            HvX64RegisterName::Cr8,
            HvX64RegisterName::InterruptState,
            HvX64RegisterName::PendingInterruption,
            HvX64RegisterName::PendingEvent0,
        ];
        let mut values = [0u32.into(); NAMES.len()];
        runner
            .get_vp_registers(NAMES, &mut values)
            .map_err(UhRunVpError::EmulationState)?;

        let &[rflags, cr8, interrupt_state, pending_interruption, pending_event] = &values;
        let pending_interruption =
            HvX64PendingInterruptionRegister::from(pending_interruption.as_u64());
        let pending_event = HvX64PendingEventReg0::from(pending_event.as_u128());
        let interrupt_state = HvX64InterruptStateRegister::from(interrupt_state.as_u64());
        let rflags = RFlags::from(rflags.as_u64());
        let cr8 = cr8.as_u64();

        let priority = vector >> 4;

        if pending_interruption.interruption_pending()
            || interrupt_state.interrupt_shadow()
            || !rflags.interrupt_enable()
            || cr8 >= priority as u64
            || pending_event.event_pending()
        {
            if !notifications.interrupt_notification()
                || (notifications.interrupt_priority() != 0
                    && notifications.interrupt_priority() < priority)
            {
                notifications.set_interrupt_notification(true);
                notifications.set_interrupt_priority(priority);
            }

            return Ok(());
        }

        let interruption = HvX64PendingInterruptionRegister::new()
            .with_interruption_type(HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT.0)
            .with_interruption_vector(vector.into())
            .with_interruption_pending(true);

        runner
            .set_vp_register(
                HvX64RegisterName::PendingInterruption,
                u64::from(interruption).into(),
            )
            .map_err(UhRunVpError::EmulationState)?;

        self.halted = false;

        tracing::trace!(vector, "interrupted");

        self.lapic.acknowledge_interrupt(vector);
        Ok(())
    }

    fn handle_nmi(
        &mut self,
        runner: &mut ProcessorRunner<'_, MshvX64>,
        notifications: &mut HvDeliverabilityNotificationsRegister,
    ) -> Result<(), UhRunVpError> {
        const NAMES: &[HvX64RegisterName] = &[
            HvX64RegisterName::InterruptState,
            HvX64RegisterName::PendingInterruption,
            HvX64RegisterName::PendingEvent0,
        ];
        let mut values = [0u32.into(); NAMES.len()];
        runner
            .get_vp_registers(NAMES, &mut values)
            .map_err(UhRunVpError::EmulationState)?;

        let &[interrupt_state, pending_interruption, pending_event] = &values;
        let pending_interruption =
            HvX64PendingInterruptionRegister::from(pending_interruption.as_u64());
        let pending_event = HvX64PendingEventReg0::from(pending_event.as_u128());
        let interrupt_state = HvX64InterruptStateRegister::from(interrupt_state.as_u64());

        if pending_interruption.interruption_pending()
            || interrupt_state.nmi_masked()
            || interrupt_state.interrupt_shadow()
            || pending_event.event_pending()
        {
            if !notifications.nmi_notification() {
                notifications.set_nmi_notification(true);
            }

            return Ok(());
        }

        let interruption = HvX64PendingInterruptionRegister::new()
            .with_interruption_type(HvX64PendingInterruptionType::HV_X64_PENDING_NMI.0)
            .with_interruption_vector(2)
            .with_interruption_pending(true);

        runner
            .set_vp_register(
                HvX64RegisterName::PendingInterruption,
                u64::from(interruption).into(),
            )
            .map_err(UhRunVpError::EmulationState)?;

        self.halted = false;
        self.nmi_pending = false;

        tracing::trace!("nmi");

        Ok(())
    }
}

impl UhProcessor<'_, HypervisorBackedX86> {
    /// Returns true if the VP is ready to run, false if it is halted.
    pub(super) fn poll_apic(
        &mut self,
        vtl: GuestVtl,
        scan_irr: bool,
    ) -> Result<(), UhRunVpError> {
        let Some(lapics) = self.backing.lapics.as_mut() else {
            return Ok(());
        };

        let lapic = &mut lapics[vtl];
        let ApicWork {
            init,
            extint,
            sipi,
            nmi,
            interrupt,
        } = lapic.lapic.scan(&mut self.vmtime, scan_irr);

        if nmi || lapic.nmi_pending {
            lapic.nmi_pending = true;
            lapic.handle_nmi(
                &mut self.runner,
                &mut self.backing.next_deliverability_notifications,
            )?;
        }

        if let Some(vector) = interrupt {
            lapic.handle_interrupt(
                &mut self.runner,
                &mut self.backing.next_deliverability_notifications,
                vector,
            )?;
        }

        if extint {
            todo!();
        }

        // TODO WHP GUEST VSM: An INIT/SIPI targeted at a VP with more than one guest VTL enabled is ignored.
        if init {
            self.handle_init(vtl)?;
        }

        if let Some(vector) = sipi {
            self.handle_sipi(vtl, vector)?;
        }

        Ok(())
    }

    fn handle_init(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError> {
        let vp_info = self.inner.vp_info;
        let mut access = self.access_state(vtl.into());
        virt::x86::vp::x86_init(&mut access, &vp_info).map_err(UhRunVpError::State)
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, vector: u8) -> Result<(), UhRunVpError> {
        let lapic = &mut self.backing.lapics.as_mut().unwrap()[vtl];
        if lapic.startup_suspend {
            let address = (vector as u64) << 12;
            let cs: hvdef::HvX64SegmentRegister = hvdef::HvX64SegmentRegister {
                base: address,
                limit: 0xffff,
                selector: (address >> 4) as u16,
                attributes: 0x9b,
            };
            self.runner
                .set_vp_registers([
                    (HvX64RegisterName::Cs, HvRegisterValue::from(cs)),
                    (HvX64RegisterName::Rip, 0u64.into()),
                ])
                .map_err(UhRunVpError::EmulationState)?;
            lapic.startup_suspend = false;
            lapic.halted = false;
        }
        Ok(())
    }
}

struct UhApicClient<'a, 'b, T> {
    partition: &'a UhPartitionInner,
    runner: &'a mut ProcessorRunner<'b, MshvX64>,
    dev: &'a T,
    vmtime: &'a VmTimeAccess,
    vtl: GuestVtl,
}

impl<T: CpuIo> ApicClient for UhApicClient<'_, '_, T> {
    fn cr8(&mut self) -> u32 {
        self.runner
            .get_vp_register(HvX64RegisterName::Cr8)
            .unwrap()
            .as_u32()
    }

    fn set_cr8(&mut self, value: u32) {
        self.runner
            .set_vp_register(HvX64RegisterName::Cr8, value.into())
            .unwrap();
    }

    fn set_apic_base(&mut self, value: u64) {
        self.runner
            .set_vp_register(HvX64RegisterName::ApicBase, value.into())
            .unwrap();
    }

    fn wake(&mut self, vp_index: VpIndex) {
        self.partition
            .vp(vp_index)
            .unwrap()
            .wake(self.vtl, WakeReason::INTCON);
    }

    fn eoi(&mut self, vector: u8) {
        debug_assert_eq!(self.vtl, GuestVtl::Vtl0);
        self.dev.handle_eoi(vector.into())
    }

    fn now(&mut self) -> VmTime {
        self.vmtime.now()
    }

    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]) {
        unreachable!()
    }
}
