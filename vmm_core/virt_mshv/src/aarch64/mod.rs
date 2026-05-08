// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64-specific implementation of the mshv hypervisor backend.

use crate::Error;
use crate::ErrorInner;
use crate::LinuxMshv;
use crate::MshvPartition;
use crate::MshvPartitionInner;
use crate::MshvProcessor;
use crate::MshvProcessorBinder;
use crate::MshvProtoPartition;
use crate::MshvVpRunner;
use crate::VcpuFdExt;
use crate::common_synthetic_features;
use crate::create_vm_with_retry;

use aarch64defs::EsrEl2;
use aarch64defs::ExceptionClass;
use aarch64defs::IssDataAbort;
use guestmem::DoorbellRegistration;
use hvdef::HvArm64RegisterName;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvInterruptControl;
use hvdef::HvInterruptType;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvPartitionPropertyCode;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use pal::unix::pthread::Pthread;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use virt::Hv1;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::aarch64::Aarch64PartitionCapabilities;
use virt::aarch64::gic_software_device::GicSoftwareDevice;
use virt::io::CpuIo;
use virt::irqcon::ControlGic as _;
use virt::irqcon::MsiRequest;
use virt::state::HvRegisterState;
use vmcore::reference_time::ReferenceTimeSource;
use zerocopy::FromZeros;

impl virt::Hypervisor for LinuxMshv {
    type ProtoPartition<'a> = MshvProtoPartition<'a>;
    type Partition = MshvPartition;
    type Error = Error;

    fn platform_info(&self) -> virt::PlatformInfo {
        virt::PlatformInfo {
            platform_gsiv: None,
            // TODO: query from hypervisor
            supports_gic_v3: true,
            supports_its: false,
        }
    }

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<MshvProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(ErrorInner::IsolationNotSupported.into());
        }

        let create_args = mshv_bindings::mshv_create_partition_v2 {
            pt_flags: 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES,
            pt_isolation: mshv_bindings::MSHV_PT_ISOLATION_NONE as u64,
            ..Default::default()
        };

        let vmfd = create_vm_with_retry(&self.mshv, &create_args)?;

        // Set synthetic processor features before initialization when the
        // guest interface is configured.
        if config.hv_config.is_some() {
            let synthetic_features = common_synthetic_features()
                .with_access_vp_regs(true)
                .with_sync_context(true);

            vmfd.set_partition_property(
                HvPartitionPropertyCode::SyntheticProcFeatures.0,
                u64::from(synthetic_features),
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
        }

        // Configure the GIC distributor base address.
        vmfd.set_partition_property(
            HvPartitionPropertyCode::GicdBaseAddress.0,
            config.processor_topology.gic_distributor_base(),
        )
        .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        // Set the virtual timer PPI (CNTV interrupt).
        vmfd.set_partition_property(
            HvPartitionPropertyCode::GicPpiOverflowInterruptFromCntv.0,
            config.processor_topology.virt_timer_ppi() as u64,
        )
        .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        // Set the PMU PPI if the topology provides one.
        if let Some(pmu_gsiv) = config.processor_topology.pmu_gsiv() {
            vmfd.set_partition_property(
                HvPartitionPropertyCode::GicPpiPerformanceMonitorsInterrupt.0,
                pmu_gsiv as u64,
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
        }

        vmfd.initialize()
            .map_err(|e| ErrorInner::CreateVMInitFailed(e.into()))?;

        MshvProtoPartition::new(config, vmfd)
    }
}

impl ProtoPartition for MshvProtoPartition<'_> {
    type Partition = MshvPartition;
    type ProcessorBinder = MshvProcessorBinder;
    type Error = Error;

    fn max_physical_address_size(&self) -> u8 {
        self.vmfd
            .get_partition_property(HvPartitionPropertyCode::PhysicalAddressWidth.0)
            .expect("failed to get physical address width") as u8
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        let caps = Aarch64PartitionCapabilities {
            supports_aarch32_el0: false,
        };

        let inner = Arc::new(MshvPartitionInner {
            vmfd: self.vmfd,
            bsp_vcpufd: self.bsp,
            memory: Default::default(),
            gm: config.guest_memory.clone(),
            mem_layout: config.mem_layout.clone(),
            vps: self.vps,
            caps,
            synic_ports: Default::default(),
            time_frozen: false.into(),
        });

        let partition = MshvPartition {
            synic_ports: Arc::new(virt::synic::SynicPorts::new(inner.clone())),
            inner,
        };

        let vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp| MshvProcessorBinder {
                partition: partition.inner.clone(),
                vpindex: vp.vp_index,
                vcpufd: None,
            })
            .collect();

        Ok((partition, vps))
    }
}

// ---------------------------------------------------------------------------
// Partition trait impls
// ---------------------------------------------------------------------------

impl virt::Partition for MshvPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = Error>> {
        Some(self)
    }

    fn doorbell_registration(
        self: &Arc<Self>,
        _minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        Some(self.clone())
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn request_msi(&self, _vtl: Vtl, request: MsiRequest) {
        self.inner.signal_msi(None, request.address, request.data);
    }

    fn request_yield(&self, vp_index: VpIndex) {
        let vp = self.inner.vp(vp_index);
        if vp.needs_yield.request_yield() {
            let thread = vp.thread.read();
            if let Some(thread) = *thread {
                if thread != Pthread::current() {
                    thread
                        .signal(libc::SIGRTMIN())
                        .expect("thread cancel signal failed");
                }
            }
        }
    }
}

impl virt::ResetPartition for MshvPartition {
    type Error = Error;

    fn reset(&self) -> Result<(), Error> {
        self.inner.freeze_time()?;
        Ok(())
    }
}

impl virt::Aarch64Partition for MshvPartition {
    fn control_gic(&self, _vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        self.inner.clone()
    }
}

impl virt::irqcon::ControlGic for MshvPartitionInner {
    fn set_spi_irq(&self, irq_id: u32, high: bool) {
        let input = hvdef::hypercall::AssertVirtualInterrupt {
            partition_id: 0,
            interrupt_control: HvInterruptControl::new()
                .with_interrupt_type(HvInterruptType::HvArm64InterruptTypeFixed)
                .with_arm64_asserted(high),
            destination_address: 0,
            requested_vector: irq_id,
            target_vtl: 0,
            rsvd0: 0,
            rsvd1: 0,
        };

        let mut args = mshv_bindings::mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallAssertVirtualInterrupt.0,
            in_sz: size_of_val(&input) as u16,
            in_ptr: std::ptr::addr_of!(input) as u64,
            ..Default::default()
        };
        if let Err(e) = self.vmfd.hvcall(&mut args) {
            tracelimit::warn_ratelimited!(
                irq_id,
                high,
                error = &e as &dyn std::error::Error,
                "failed to assert SPI"
            );
        }
    }
}

impl virt::aarch64::vm::AccessVmState for &'_ MshvPartition {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Hv1 for MshvPartition {
    type Error = Error;
    type Device = GicSoftwareDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        Some(ReferenceTimeSource::from(self.inner.clone() as Arc<_>))
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        Some(self)
    }

    fn synic(&self) -> Arc<dyn vmcore::synic::SynicPortAccess> {
        self.synic_ports.clone()
    }
}

impl virt::DeviceBuilder for MshvPartition {
    fn build(&self, _vtl: Vtl, _device_id: u64) -> Result<Self::Device, Self::Error> {
        Ok(GicSoftwareDevice::new(self.inner.clone()))
    }
}

impl SignalMsi for MshvPartitionInner {
    fn signal_msi(&self, _devid: Option<u32>, _address: u64, data: u32) {
        self.set_spi_irq(data, true);
    }
}

// ---------------------------------------------------------------------------
// Processor binding and run loop
// ---------------------------------------------------------------------------

impl virt::BindProcessor for MshvProcessorBinder {
    type Processor<'a>
        = MshvProcessor<'a>
    where
        Self: 'a;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let inner = &self.partition.vps[self.vpindex.index() as usize];

        let vcpufd = if self.vpindex.is_bsp() {
            &self.partition.bsp_vcpufd
        } else {
            if self.vcpufd.is_none() {
                let vcpufd = self
                    .partition
                    .vmfd
                    .create_vcpu(u8::try_from(self.vpindex.index()).expect("validated above"))
                    .map_err(|e| ErrorInner::CreateVcpu(e.into()))?;
                self.vcpufd = Some(vcpufd);
            }
            self.vcpufd.as_ref().unwrap()
        };

        // Set the GIC redistributor base for this VP (GICv3 only).
        if let Some(gicr) = inner.vp_info.gicr {
            vcpufd
                .set_hvdef_regs(&[HvRegisterAssoc::from((
                    HvArm64RegisterName::GicrBaseGpa,
                    gicr,
                ))])
                .map_err(ErrorInner::Register)?;
        }

        let runner = MshvVpRunner { vcpufd };

        Ok(MshvProcessor {
            partition: &self.partition,
            inner,
            vpindex: self.vpindex,
            runner,
            deliverability_notifications: HvDeliverabilityNotificationsRegister::new(),
        })
    }
}

impl MshvProcessor<'_> {
    async fn handle_memory_intercept(
        &mut self,
        message: &HvMessage,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let info = message.as_message::<hvdef::HvArm64MemoryInterceptMessage>();
        let syndrome = EsrEl2::from(info.syndrome);
        let ec = ExceptionClass(syndrome.ec());

        match ec {
            ExceptionClass::DATA_ABORT_LOWER => {
                let iss = IssDataAbort::from(syndrome.iss());
                if !iss.isv() {
                    return Err(dev.fatal_error(
                        "data abort with no valid ISS (instruction syndrome not valid)"
                            .to_string()
                            .into(),
                    ));
                }
                let len = 1usize << iss.sas();
                let reg = iss.srt();
                let gpa = info.guest_physical_address;

                if iss.wnr() {
                    let value = self.get_x(reg);
                    dev.write_mmio(self.vpindex, gpa, &value.to_ne_bytes()[..len])
                        .await;
                } else {
                    let mut data = [0u8; 8];
                    dev.read_mmio(self.vpindex, gpa, &mut data[..len]).await;
                    let value = if iss.sse() {
                        match len {
                            1 => data[0] as i8 as i64 as u64,
                            2 => u16::from_ne_bytes([data[0], data[1]]) as i16 as i64 as u64,
                            4 => u32::from_ne_bytes([data[0], data[1], data[2], data[3]]) as i32
                                as i64 as u64,
                            _ => u64::from_ne_bytes(data),
                        }
                    } else {
                        u64::from_ne_bytes(data) & ((1u128 << (len * 8)) - 1) as u64
                    };
                    self.set_x(reg, value);
                }

                let advance = if syndrome.il() { 4 } else { 2 };
                let new_pc = info.header.pc.wrapping_add(advance);
                self.set_pc(new_pc);
            }
            _ => {
                return Err(dev.fatal_error(
                    format!("unexpected exception class in memory intercept: {ec:?}").into(),
                ));
            }
        }

        Ok(())
    }

    fn handle_hypercall_intercept(&mut self, message: &HvMessage) {
        let info = message.as_message::<hvdef::HvArm64HypercallInterceptMessage>();

        let pre_advanced = false;
        let smccc = info.immediate == 0;

        let mut handler = MshvHypercallHandler {
            partition: self.partition,
            x: info.x,
            pc: info.header.pc,
            dirty: false,
        };

        MshvHypercallHandler::DISPATCHER.dispatch(
            &self.partition.gm,
            hv1_hypercall::Arm64RegisterIo::new(&mut handler, pre_advanced, smccc),
        );

        if handler.dirty {
            let mut assocs: Vec<HvRegisterAssoc> = Vec::with_capacity(19);
            for i in 0..18u32 {
                assocs.push(HvRegisterAssoc::from((
                    HvArm64RegisterName(HvArm64RegisterName::X0.0 + i),
                    handler.x[i as usize],
                )));
            }
            assocs.push(HvRegisterAssoc::from((
                HvArm64RegisterName::XPc,
                handler.pc,
            )));
            self.runner
                .vcpufd
                .set_hvdef_regs(&assocs)
                .expect("failed to write back hypercall registers");
        }
    }

    fn get_x(&self, reg: u8) -> u64 {
        if reg >= 31 {
            return 0; // XZR
        }
        let name = HvArm64RegisterName(HvArm64RegisterName::X0.0 + reg as u32);
        let mut assoc = [HvRegisterAssoc::from((name, 0u64))];
        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc)
            .expect("failed to read register");
        assoc[0].value.as_u64()
    }

    fn set_x(&self, reg: u8, value: u64) {
        if reg >= 31 {
            return;
        }
        let name = HvArm64RegisterName(HvArm64RegisterName::X0.0 + reg as u32);
        self.runner
            .vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((name, value))])
            .expect("failed to write register");
    }

    fn set_pc(&self, pc: u64) {
        self.runner
            .vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((HvArm64RegisterName::XPc, pc))])
            .expect("failed to write PC");
    }

    fn get_register_state<T, const N: usize>(&self) -> Result<T, Error>
    where
        T: HvRegisterState<HvArm64RegisterName, N>,
    {
        let mut regs = T::default();
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc[..])
            .map_err(ErrorInner::Register)?;

        regs.set_values(assoc.iter().map(|assoc| assoc.value));
        Ok(regs)
    }

    fn set_register_state<T, const N: usize>(&self, regs: &T) -> Result<(), Error>
    where
        T: HvRegisterState<HvArm64RegisterName, N>,
    {
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        regs.get_values(assoc.iter_mut().map(|assoc| &mut assoc.value));

        self.runner
            .vcpufd
            .set_hvdef_regs(&assoc[..])
            .map_err(ErrorInner::Register)?;

        Ok(())
    }

    pub(crate) async fn handle_exit(
        &mut self,
        exit: &HvMessage,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        match exit.header.typ {
            HvMessageType::HvMessageTypeUnrecoverableException => {
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
            HvMessageType::HvMessageTypeUnmappedGpa | HvMessageType::HvMessageTypeGpaIntercept => {
                self.handle_memory_intercept(exit, dev).await?;
            }
            HvMessageType::HvMessageTypeSynicSintDeliverable => {
                let info = exit.as_message::<hvdef::HvArm64SynicSintDeliverableMessage>();
                self.handle_sint_deliverable(info.deliverable_sints);
            }
            HvMessageType::HvMessageTypeHypercallIntercept => {
                tracing::trace!("HYPERCALL_INTERCEPT");
                self.handle_hypercall_intercept(exit);
            }
            HvMessageType::HvMessageTypeArm64ResetIntercept => {
                let info = exit.as_message::<hvdef::HvArm64ResetInterceptMessage>();
                match info.reset_type {
                    hvdef::HvArm64ResetType::POWER_OFF => {
                        return Err(VpHaltReason::PowerOff);
                    }
                    hvdef::HvArm64ResetType::REBOOT => {
                        return Err(VpHaltReason::Reset);
                    }
                    _ => {
                        tracelimit::warn_ratelimited!(
                            reset_type = ?info.reset_type,
                            "unknown reset type"
                        );
                        return Err(VpHaltReason::Reset);
                    }
                }
            }
            exit_type => {
                panic!("Unhandled vcpu exit code {exit_type:?}");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VP state access
// ---------------------------------------------------------------------------

impl virt::vp::AccessVpState for &'_ mut MshvProcessor<'_> {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<virt::aarch64::vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &virt::aarch64::vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn system_registers(&mut self) -> Result<virt::aarch64::vp::SystemRegisters, Self::Error> {
        self.get_register_state()
    }

    fn set_system_registers(
        &mut self,
        value: &virt::aarch64::vp::SystemRegisters,
    ) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }
}

// ---------------------------------------------------------------------------
// Hypercall handler
// ---------------------------------------------------------------------------

pub(crate) struct MshvHypercallHandler<'a> {
    pub(crate) partition: &'a MshvPartitionInner,
    x: [u64; 18],
    pc: u64,
    dirty: bool,
}

impl MshvHypercallHandler<'_> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

impl hv1_hypercall::Arm64RegisterState for MshvHypercallHandler<'_> {
    fn pc(&mut self) -> u64 {
        self.pc
    }

    fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
        self.dirty = true;
    }

    fn x(&mut self, n: u8) -> u64 {
        self.x[n as usize]
    }

    fn set_x(&mut self, n: u8, v: u64) {
        self.x[n as usize] = v;
        self.dirty = true;
    }
}
