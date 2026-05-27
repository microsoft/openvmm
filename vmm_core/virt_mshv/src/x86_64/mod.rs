// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific implementation of the mshv hypervisor backend.

mod vm_state;
mod vp_state;

use crate::Error;
use crate::ErrorInner;
use crate::KernelError;
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

use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hv1_hypercall::X64RegisterIo;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvProcessorVendor;
use hvdef::HvX64RegisterName;
use hvdef::HvX64RegisterPage;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_ioctls::InterruptRequest;
use mshv_ioctls::VcpuFd;
use pal::unix::pthread::Pthread;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use virt::Hv1;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::state::StateElement as _;
use virt::x86::apic_software_device::ApicSoftwareDevice;
use virt::x86::apic_software_device::ApicSoftwareDevices;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::reference_time::ReferenceTimeSource;
use x86defs::RFlags;
use x86defs::SegmentRegister;

impl virt::Hypervisor for LinuxMshv {
    type ProtoPartition<'a> = MshvProtoPartition<'a>;
    type Partition = MshvPartition;
    type Error = Error;

    fn platform_info(&self) -> virt::PlatformInfo {
        virt::PlatformInfo {}
    }

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<MshvProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(ErrorInner::IsolationNotSupported.into());
        }

        // Build partition creation flags. LAPIC is always enabled (the
        // hypervisor emulates the local APIC). X2APIC is only enabled when
        // the topology requests it.
        let mut pt_flags: u64 = 1 << mshv_bindings::MSHV_PT_BIT_LAPIC
            | 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES
            | 1 << mshv_bindings::MSHV_PT_BIT_CPU_AND_XSAVE_FEATURES;

        match config.processor_topology.apic_mode() {
            vm_topology::processor::x86::ApicMode::X2ApicSupported
            | vm_topology::processor::x86::ApicMode::X2ApicEnabled => {
                pt_flags |= 1 << mshv_bindings::MSHV_PT_BIT_X2APIC;
            }
            vm_topology::processor::x86::ApicMode::XApic => {}
        }

        if config.processor_topology.smt_enabled() {
            pt_flags |= 1 << mshv_bindings::MSHV_PT_BIT_SMT_ENABLED_GUEST;
        }

        let create_args = mshv_bindings::mshv_create_partition_v2 {
            pt_flags,
            pt_isolation: mshv_bindings::MSHV_PT_ISOLATION_NONE as u64,
            pt_num_cpu_fbanks: mshv_bindings::MSHV_NUM_CPU_FEATURES_BANKS as u16,
            pt_cpu_fbanks: [
                !u64::from(supported_processor_features()),
                !u64::from(supported_processor_features1()),
            ],
            pt_disabled_xsave: !u64::from(supported_xsave_features()),
            ..Default::default()
        };

        let vmfd = create_vm_with_retry(&self.mshv, &create_args)?;

        // Set synthetic processor features before initialization when the
        // guest interface is configured.
        if config.hv_config.is_some() {
            let synthetic_features = common_synthetic_features()
                .with_access_partition_reference_tsc(true)
                .with_access_guest_idle_reg(true)
                .with_access_frequency_regs(true)
                .with_enable_extended_gva_ranges_for_flush_virtual_address_list(true);

            vmfd.set_partition_property(
                HvPartitionPropertyCode::SyntheticProcFeatures.0,
                u64::from(synthetic_features),
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
        }

        vmfd.initialize()
            .map_err(|e| ErrorInner::CreateVMInitFailed(e.into()))?;

        // Tell the hypervisor how many VPs are in each socket.
        vmfd.set_partition_property(
            HvPartitionPropertyCode::ProcessorsPerSocket.0,
            config.processor_topology.reserved_vps_per_socket() as u64,
        )
        .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        MshvProtoPartition::new(config, vmfd)
    }
}

impl MshvProtoPartition<'_> {
    /// Build partition capabilities from partition properties instead of
    /// CPUID.
    fn caps_from_properties(&self) -> Result<virt::x86::X86PartitionCapabilities, Error> {
        use virt::x86::X86PartitionCapabilities;
        use virt::x86::XsaveCapabilities;
        use x86defs::cpuid::Vendor;
        use x86defs::xsave::XSAVE_VARIABLE_OFFSET;

        let vendor_id = self
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::ProcessorVendor.0)
            .map_err(|e| ErrorInner::GetPartitionProperty(e.into()))?;

        let vendor = match HvProcessorVendor(vendor_id as u32) {
            HvProcessorVendor::AMD => Vendor::AMD,
            HvProcessorVendor::INTEL => Vendor::INTEL,
            HvProcessorVendor::HYGON => Vendor::HYGON,
            v => return Err(ErrorInner::UnsupportedProcessorVendor(v).into()),
        };

        let xsave_states = self
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::XsaveStates.0)
            .map_err(|e| ErrorInner::GetPartitionProperty(e.into()))?;

        let max_xsave_data_size = self
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::MaxXsaveDataSize.0)
            .map_err(|e| ErrorInner::GetPartitionProperty(e.into()))?;

        let reset_rdx = {
            let mut assoc = [HvRegisterAssoc::from((HvX64RegisterName::Rdx, 0u64))];
            self.bsp
                .get_hvdef_regs(&mut assoc)
                .map_err(ErrorInner::Register)?;
            assoc[0].value.as_u64()
        };

        let x2apic = matches!(
            self.config.processor_topology.apic_mode(),
            vm_topology::processor::x86::ApicMode::X2ApicSupported
                | vm_topology::processor::x86::ApicMode::X2ApicEnabled
        );
        let x2apic_enabled = matches!(
            self.config.processor_topology.apic_mode(),
            vm_topology::processor::x86::ApicMode::X2ApicEnabled
        );

        Ok(X86PartitionCapabilities {
            vendor,
            hv1: self.config.hv_config.is_some(),
            hv1_reference_tsc_page: self.config.hv_config.is_some(),
            xsave: XsaveCapabilities {
                features: xsave_states,
                supervisor_features: 0,
                standard_len: XSAVE_VARIABLE_OFFSET as u32,
                compact_len: max_xsave_data_size as u32,
                feature_info: [Default::default(); 63],
            },
            x2apic,
            x2apic_enabled,
            reset_rdx,
            cet: false,
            cet_ss: false,
            sgx: false,
            tsc_aux: false,
            vtom: None,
            physical_address_width: self.max_physical_address_size(),
            can_freeze_time: false,
            xsaves_state_bv_broken: false,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
        })
    }

    fn max_physical_address_size(&self) -> u8 {
        self.vmfd
            .get_partition_property(HvPartitionPropertyCode::PhysicalAddressWidth.0)
            .expect("failed to get physical address width") as u8
    }
}

impl ProtoPartition for MshvProtoPartition<'_> {
    type Partition = MshvPartition;
    type ProcessorBinder = MshvProcessorBinder;
    type Error = Error;

    fn max_physical_address_size(&self) -> u8 {
        self.max_physical_address_size()
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        let cpuid = virt::CpuidLeafSet::new(config.cpuid.to_vec());

        // Apply CPUID overrides partition-wide.
        for leaf in cpuid.leaves().iter() {
            let input = hvdef::hypercall::RegisterInterceptResultCpuid {
                partition_id: 0,
                vp_index: hvdef::HV_ANY_VP,
                intercept_type: hvdef::hypercall::HvInterceptType::HvInterceptTypeX64Cpuid,
                parameters: hvdef::hypercall::HvRegisterX64CpuidResultParameters {
                    input: hvdef::hypercall::HvRegisterX64CpuidResultParametersInput {
                        eax: leaf.function,
                        ecx: leaf.index.unwrap_or(0),
                        subleaf_specific: u8::from(leaf.index.is_some()),
                        always_override: 1,
                        padding: 0,
                    },
                    result: hvdef::hypercall::HvRegisterX64CpuidResultParametersOutput {
                        eax: leaf.result[0],
                        eax_mask: leaf.mask[0],
                        ebx: leaf.result[1],
                        ebx_mask: leaf.mask[1],
                        ecx: leaf.result[2],
                        ecx_mask: leaf.mask[2],
                        edx: leaf.result[3],
                        edx_mask: leaf.mask[3],
                    },
                },
                _reserved: 0,
            };
            let mut args = mshv_bindings::mshv_root_hvcall {
                code: hvdef::HypercallCode::HvCallRegisterInterceptResult.0,
                in_sz: size_of_val(&input) as u16,
                in_ptr: std::ptr::addr_of!(input) as u64,
                ..Default::default()
            };
            self.vmfd
                .hvcall(&mut args)
                .map_err(|e| ErrorInner::RegisterCpuid(e.into()))?;
        }

        let caps = {
            let mut caps = match self.bsp.get_cpuid_values(0, 0, 0, 0) {
                Ok(_) => virt::PartitionCapabilities::from_cpuid(
                    self.config.processor_topology,
                    &mut |function, index| {
                        self.bsp
                            .get_cpuid_values(function, index, 0, 0)
                            .map_err(KernelError::from)
                            .expect("cpuid should not fail")
                    },
                )
                .map_err(ErrorInner::Capabilities)?,
                Err(_) => {
                    tracing::warn!(
                        "failed to query CPUID, falling back to partition properties, some features may be unavailable"
                    );
                    self.caps_from_properties()?
                }
            };
            caps.xsaves_state_bv_broken = true;
            caps.can_freeze_time = true;
            caps
        };

        let apic_id_map = self
            .config
            .processor_topology
            .vps_arch()
            .map(|vp| vp.apic_id)
            .collect();

        let inner = Arc::new(MshvPartitionInner {
            vmfd: self.vmfd,
            bsp_vcpufd: self.bsp,
            memory: Default::default(),
            gm: config.guest_memory.clone(),
            mem_layout: config.mem_layout.clone(),
            vps: self.vps,
            irq_routes: Default::default(),
            gsi_states: Mutex::new(Box::new(
                [crate::irqfd::GsiState::Unallocated; crate::irqfd::NUM_GSIS],
            )),
            caps,
            synic_ports: Default::default(),
            cpuid,
            software_devices: ApicSoftwareDevices::new(apic_id_map),
            time_frozen: Mutex::new(false),
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
        self.inner.request_msi(request)
    }

    fn as_signal_msi(&self, _vtl: Vtl) -> Option<Arc<dyn SignalMsi>> {
        Some(self.inner.clone())
    }

    fn irqfd(&self) -> Option<Arc<dyn virt::irqfd::IrqFd>> {
        Some(Arc::new(crate::irqfd::MshvIrqFd::new(self.inner.clone())))
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

impl virt::X86Partition for MshvPartition {
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        // TODO
        tracelimit::warn_ratelimited!(?vp_index, ?vtl, lint, "ignored lint pulse");
    }
}

impl virt::ResetPartition for MshvPartition {
    type Error = Error;

    fn reset(&self) -> Result<(), Error> {
        use virt::x86::vm::AccessVmState;

        for irq in 0..virt::irqcon::IRQ_LINES as u8 {
            self.inner.irq_routes.set_irq_route(irq, None);
        }

        self.inner.freeze_time()?;

        let bsp_vp_info = &self.inner.vps[0].vp_info;
        self.access_state(Vtl::Vtl0)
            .reset_all(bsp_vp_info)
            .map_err(|e| ErrorInner::ResetState(Box::new(e)))?;

        Ok(())
    }
}

impl Hv1 for MshvPartition {
    type Error = Error;
    type Device = ApicSoftwareDevice;

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
    fn build(&self, _vtl: Vtl, device_id: u64) -> Result<Self::Device, Self::Error> {
        Ok(self
            .inner
            .software_devices
            .new_device(self.inner.clone(), device_id)
            .map_err(ErrorInner::NewDevice)?)
    }
}

impl MshvPartitionInner {
    fn request_msi(&self, request: MsiRequest) {
        let (address, data) = request.as_x86();
        let control = request.hv_x86_interrupt_control();
        let mshv_req = InterruptRequest {
            interrupt_type: control.interrupt_type().0,
            apic_id: address.virt_destination().into(),
            vector: data.vector().into(),
            level_triggered: control.x86_level_triggered(),
            logical_destination_mode: control.x86_logical_destination_mode(),
            long_mode: false,
        };

        if let Err(err) = self.vmfd.request_virtual_interrupt(&mshv_req) {
            tracelimit::warn_ratelimited!(
                address = request.address,
                data = request.data,
                error = &err as &dyn std::error::Error,
                "failed to request msi"
            );
        }
    }
}

impl SignalMsi for MshvPartitionInner {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        self.request_msi(MsiRequest { address, data });
    }
}

impl virt::irqcon::IoApicRouting for MshvPartitionInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        self.irq_routes.set_irq_route(irq, request)
    }

    fn assert_irq(&self, irq: u8) {
        self.irq_routes
            .assert_irq(irq, |request| self.request_msi(request))
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

        let reg_page_ptr = vcpufd
            .get_vp_reg_page()
            .expect("register page must be mapped")
            .0
            .cast::<HvX64RegisterPage>();

        let runner = MshvVpRunner {
            vcpufd,
            reg_page: reg_page_ptr,
        };

        let this = MshvProcessor {
            partition: &self.partition,
            inner,
            vpindex: self.vpindex,
            runner,
            deliverability_notifications: HvDeliverabilityNotificationsRegister::new(),
        };

        // Set the APIC state.
        let apic_base =
            virt::vp::Apic::at_reset(&this.partition.caps, &this.inner.vp_info).apic_base;

        let regs = &[
            HvRegisterAssoc::from((
                HvX64RegisterName::InitialApicId,
                u64::from(inner.vp_info.apic_id),
            )),
            HvRegisterAssoc::from((HvX64RegisterName::ApicBase, apic_base)),
            HvRegisterAssoc::from((HvX64RegisterName::ApicId, u64::from(inner.vp_info.apic_id))),
        ];

        let reg_count = if this.partition.caps.x2apic { 2 } else { 3 };

        vcpufd
            .set_hvdef_regs(&regs[..reg_count])
            .map_err(ErrorInner::Register)?;

        Ok(this)
    }
}

impl MshvProcessor<'_> {
    async fn emulate(
        &mut self,
        message: &HvMessage,
        devices: &impl CpuIo,
        interruption_pending: bool,
    ) -> Result<(), VpHaltReason> {
        let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
            gm: &self.partition.gm,
            kx_gm: &self.partition.gm,
            ux_gm: &self.partition.gm,
        };

        let mut support = MshvEmulationState {
            partition: self.partition,
            vcpufd: self.runner.vcpufd,
            reg_page: self.runner.reg_page(),
            vp_index: self.vpindex,
            message,
            interruption_pending,
        };
        virt_support_x86emu::emulate::emulate(&mut support, &emu_mem, devices).await
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
            HvMessageType::HvMessageTypeX64IoPortIntercept => {
                self.handle_io_port_intercept(exit, dev).await?;
            }
            HvMessageType::HvMessageTypeUnmappedGpa | HvMessageType::HvMessageTypeGpaIntercept => {
                self.handle_mmio_intercept(exit, dev).await?;
            }
            HvMessageType::HvMessageTypeSynicSintDeliverable => {
                tracing::trace!("SYNIC_SINT_DELIVERABLE");
                let info = exit.as_message::<hvdef::HvX64SynicSintDeliverableMessage>();
                self.handle_sint_deliverable(info.deliverable_sints);
            }
            HvMessageType::HvMessageTypeHypercallIntercept => {
                tracing::trace!("HYPERCALL_INTERCEPT");
                self.handle_hypercall_intercept(exit, dev);
            }
            HvMessageType::HvMessageTypeX64ApicEoi => {
                let msg = exit.as_message::<hvdef::HvX64ApicEoiMessage>();
                dev.handle_eoi(msg.interrupt_vector);
            }
            exit_type => {
                panic!("Unhandled vcpu exit code {exit_type:?}");
            }
        }
        Ok(())
    }

    async fn handle_io_port_intercept(
        &mut self,
        message: &HvMessage,
        devices: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let info = message.as_message::<hvdef::HvX64IoPortInterceptMessage>();
        let access_info = info.access_info;

        if access_info.string_op() || access_info.rep_prefix() {
            let interruption_pending = info.header.execution_state.interruption_pending();
            self.emulate(message, devices, interruption_pending).await?
        } else {
            let mut ret_rax = info.rax;
            virt_support_x86emu::emulate::emulate_io(
                self.vpindex,
                info.header.intercept_access_type == hvdef::HvInterceptAccessType::WRITE,
                info.port_number,
                &mut ret_rax,
                access_info.access_size(),
                devices,
            )
            .await;

            let insn_len = info.header.instruction_len() as u64;

            let rp = self.runner.reg_page();
            rp.gp_registers[x86emu::Gp::RAX as usize] = ret_rax;
            rp.rip = info.header.rip + insn_len;
            rp.dirty.set_general_purpose(true);
            rp.dirty.set_instruction_pointer(true);
        }

        Ok(())
    }

    async fn handle_mmio_intercept(
        &mut self,
        message: &HvMessage,
        devices: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let info = message.as_message::<hvdef::HvX64MemoryInterceptMessage>();
        let interruption_pending = info.header.execution_state.interruption_pending();
        self.emulate(message, devices, interruption_pending).await
    }

    fn handle_hypercall_intercept(&mut self, message: &HvMessage, _devices: &impl CpuIo) {
        let info = message.as_message::<hvdef::HvX64HypercallInterceptMessage>();
        let is_64bit =
            info.header.execution_state.cr0_pe() && info.header.execution_state.efer_lma();

        let mut handler = MshvHypercallHandler {
            partition: self.partition,
            reg_page: self.runner.reg_page(),
        };

        MshvHypercallHandler::DISPATCHER.dispatch(
            &self.partition.gm,
            X64RegisterIo::new(&mut handler, is_64bit),
        );
    }
}

// ---------------------------------------------------------------------------
// x86 emulation support
// ---------------------------------------------------------------------------

struct MshvEmulationState<'a> {
    partition: &'a MshvPartitionInner,
    vcpufd: &'a VcpuFd,
    reg_page: &'a mut HvX64RegisterPage,
    vp_index: VpIndex,
    message: &'a HvMessage,
    interruption_pending: bool,
}

impl EmulatorSupport for MshvEmulationState<'_> {
    fn vp_index(&self) -> VpIndex {
        self.vp_index
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.partition.caps.vendor
    }

    fn gp(&mut self, reg: x86emu::Gp) -> u64 {
        self.reg_page.gp_registers[reg as usize]
    }

    fn set_gp(&mut self, reg: x86emu::Gp, v: u64) {
        self.reg_page.gp_registers[reg as usize] = v;
        self.reg_page.dirty.set_general_purpose(true);
    }

    fn rip(&mut self) -> u64 {
        self.reg_page.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.reg_page.rip = v;
        self.reg_page.dirty.set_instruction_pointer(true);
    }

    fn segment(&mut self, reg: x86emu::Segment) -> SegmentRegister {
        virt::x86::SegmentRegister::from(self.reg_page.segment[reg as usize]).into()
    }

    fn efer(&mut self) -> u64 {
        self.reg_page.efer
    }

    fn cr0(&mut self) -> u64 {
        self.reg_page.cr0
    }

    fn rflags(&mut self) -> RFlags {
        RFlags::from(self.reg_page.rflags)
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.reg_page.rflags = v.into();
        self.reg_page.dirty.set_flags(true);
    }

    fn xmm(&mut self, reg: usize) -> u128 {
        assert!(reg < 16);
        if reg < 6 {
            self.reg_page.xmm[reg]
        } else {
            let name = HvX64RegisterName(HvX64RegisterName::Xmm0.0 + reg as u32);
            let mut assoc = [HvRegisterAssoc::from((name, 0u128))];
            let _ = self.vcpufd.get_hvdef_regs(&mut assoc);
            assoc[0].value.as_u128()
        }
    }

    fn set_xmm(&mut self, reg: usize, value: u128) {
        assert!(reg < 16);
        if reg < 6 {
            self.reg_page.xmm[reg] = value;
            self.reg_page.dirty.set_xmm(true);
        } else {
            let name = HvX64RegisterName(HvX64RegisterName::Xmm0.0 + reg as u32);
            let assoc = [HvRegisterAssoc::from((name, value))];
            self.vcpufd.set_hvdef_regs(&assoc).unwrap();
        }
    }

    fn flush(&mut self) {}

    fn instruction_bytes(&self) -> &[u8] {
        match self.message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                let info = self
                    .message
                    .as_message::<hvdef::HvX64MemoryInterceptMessage>();
                &info.instruction_bytes[..info.instruction_byte_count as usize]
            }
            HvMessageType::HvMessageTypeX64IoPortIntercept => {
                let info = self
                    .message
                    .as_message::<hvdef::HvX64IoPortInterceptMessage>();
                &info.instruction_bytes[..info.instruction_byte_count as usize]
            }
            _ => unreachable!(),
        }
    }

    fn physical_address(&self) -> Option<u64> {
        match self.message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                let info = self
                    .message
                    .as_message::<hvdef::HvX64MemoryInterceptMessage>();
                Some(info.guest_physical_address)
            }
            _ => None,
        }
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        match self.message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {}
            _ => return None,
        }

        let message = self
            .message
            .as_message::<hvdef::HvX64MemoryInterceptMessage>();

        if !message.memory_access_info.gva_gpa_valid() {
            return None;
        }

        if let Ok(translate_mode) = TranslateMode::try_from(message.header.intercept_access_type) {
            Some(virt_support_x86emu::emulate::InitialTranslation {
                gva: message.guest_virtual_address,
                gpa: message.guest_physical_address,
                translate_mode,
            })
        } else {
            None
        }
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        self.vcpufd
            .set_hvdef_regs(&[
                HvRegisterAssoc::from((
                    HvX64RegisterName::PendingEvent0,
                    u128::from(event_info.reg_0),
                )),
                HvRegisterAssoc::from((
                    HvX64RegisterName::PendingEvent1,
                    u128::from(event_info.reg_1),
                )),
            ])
            .unwrap();
    }

    fn is_gpa_mapped(&self, gpa: u64, _write: bool) -> bool {
        self.partition
            .mem_layout
            .ram()
            .iter()
            .any(|r| r.range.contains_addr(gpa))
    }

    fn lapic_base_address(&self) -> Option<u64> {
        None
    }

    fn lapic_read(&mut self, _address: u64, _data: &mut [u8]) {
        unreachable!()
    }

    fn lapic_write(&mut self, _address: u64, _data: &[u8]) {
        unreachable!()
    }
}

impl TranslateGvaSupport for MshvEmulationState<'_> {
    fn guest_memory(&self) -> &GuestMemory {
        &self.partition.gm
    }

    fn acquire_tlb_lock(&mut self) {}

    fn registers(&mut self) -> TranslationRegisters {
        TranslationRegisters {
            cr0: self.reg_page.cr0,
            cr4: self.reg_page.cr4,
            efer: self.reg_page.efer,
            cr3: self.reg_page.cr3,
            rflags: self.reg_page.rflags,
            ss: virt::x86::SegmentRegister::from(
                self.reg_page.segment[x86emu::Segment::SS as usize],
            )
            .into(),
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Hypercall handler
// ---------------------------------------------------------------------------

impl hv1_hypercall::X64RegisterState for MshvHypercallHandler<'_> {
    fn rip(&mut self) -> u64 {
        self.reg_page.rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.reg_page.rip = rip;
        self.reg_page.dirty.set_instruction_pointer(true);
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        self.reg_page.gp_registers[n as usize]
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        self.reg_page.gp_registers[n as usize] = value;
        self.reg_page.dirty.set_general_purpose(true);
    }

    fn xmm(&mut self, n: usize) -> u128 {
        self.reg_page.xmm[n]
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.reg_page.xmm[n] = value;
        self.reg_page.dirty.set_xmm(true);
    }
}

pub(crate) struct MshvHypercallHandler<'a> {
    pub(crate) partition: &'a MshvPartitionInner,
    pub(crate) reg_page: &'a mut HvX64RegisterPage,
}

impl MshvHypercallHandler<'_> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvRetargetDeviceInterrupt,
        ],
    );
}

impl hv1_hypercall::RetargetDeviceInterrupt for MshvHypercallHandler<'_> {
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: hv1_hypercall::HvInterruptParameters<'_>,
    ) -> hvdef::HvResult<()> {
        let target_processors = Vec::from_iter(params.target_processors);
        let vpci_params = vmcore::vpci_msi::VpciInterruptParameters {
            vector: params.vector,
            multicast: params.multicast,
            target_processors: &target_processors,
        };

        self.partition
            .software_devices
            .retarget_interrupt(device_id, address, data, &vpci_params)
    }
}

// ---------------------------------------------------------------------------
// CPU feature lists
// ---------------------------------------------------------------------------

/// Processor features (bank 0) that we support exposing to guests.
fn supported_processor_features() -> hvdef::HvX64PartitionProcessorFeatures {
    hvdef::HvX64PartitionProcessorFeatures::new()
        .with_sse3_support(true)
        .with_lahf_sahf_support(true)
        .with_ssse3_support(true)
        .with_sse4_1_support(true)
        .with_sse4_2_support(true)
        .with_sse4a_support(true)
        .with_xop_support(true)
        .with_pop_cnt_support(true)
        .with_cmpxchg16b_support(true)
        .with_altmovcr8_support(true)
        .with_lzcnt_support(true)
        .with_mis_align_sse_support(true)
        .with_mmx_ext_support(true)
        .with_amd3d_now_support(true)
        .with_extended_amd3d_now_support(true)
        .with_page_1gb_support(true)
        .with_aes_support(true)
        .with_pclmulqdq_support(true)
        .with_pcid_support(true)
        .with_fma4_support(true)
        .with_f16c_support(true)
        .with_rd_rand_support(true)
        .with_rd_wr_fs_gs_support(true)
        .with_smep_support(true)
        .with_enhanced_fast_string_support(true)
        .with_bmi1_support(true)
        .with_bmi2_support(true)
        .with_movbe_support(true)
        .with_npiep1_support(true)
        .with_dep_x87_fpu_save_support(true)
        .with_rd_seed_support(true)
        .with_adx_support(true)
        .with_intel_prefetch_support(true)
        .with_smap_support(true)
        .with_hle_support(true)
        .with_rtm_support(true)
        .with_rdtscp_support(true)
        .with_clflushopt_support(true)
        .with_clwb_support(true)
        .with_sha_support(true)
        .with_x87_pointers_saved_support(true)
        .with_invpcid_support(true)
        .with_ibrs_support(true)
        .with_stibp_support(true)
        .with_ibpb_support(true)
        .with_unrestricted_guest_support(true)
        .with_mdd_support(true)
        .with_fast_short_rep_mov_support(true)
        .with_rdcl_no_support(true)
        .with_ibrs_all_support(true)
        .with_ssb_no_support(true)
        .with_rsb_a_no_support(true)
        .with_rd_pid_support(true)
        .with_umip_support(true)
        .with_mbs_no_support(true)
        .with_mb_clear_support(true)
        .with_taa_no_support(true)
        .with_tsx_ctrl_support(true)
}

/// Processor features (bank 1) that we support exposing to guests.
fn supported_processor_features1() -> hvdef::HvX64PartitionProcessorFeatures1 {
    hvdef::HvX64PartitionProcessorFeatures1::new()
        .with_a_count_m_count_support(true)
        .with_tsc_invariant_support(true)
        .with_cl_zero_support(true)
        .with_rdpru_support(true)
        .with_la57_support(true)
        .with_mbec_support(true)
        .with_nested_virt_support(true)
        .with_psfd_support(true)
        .with_cet_ss_support(true)
        .with_cet_ibt_support(true)
        .with_vmx_exception_inject_support(true)
        .with_umwait_tpause_support(true)
        .with_movdiri_support(true)
        .with_movdir64b_support(true)
        .with_cldemote_support(true)
        .with_serialize_support(true)
        .with_tsc_deadline_tmr_support(true)
        .with_tsc_adjust_support(true)
        .with_fz_l_rep_movsb(true)
        .with_fs_rep_stosb(true)
        .with_fs_rep_cmpsb(true)
        .with_tsx_ld_trk_support(true)
        .with_vmx_ins_outs_exit_info_support(true)
        .with_sbdr_ssdp_no_support(true)
        .with_fbsdp_no_support(true)
        .with_psdp_no_support(true)
        .with_fb_clear_support(true)
        .with_btc_no_support(true)
        .with_ibpb_rsb_flush_support(true)
        .with_stibp_always_on_support(true)
        .with_perf_global_ctrl_support(true)
        .with_npt_execute_only_support(true)
        .with_npt_ad_flags_support(true)
        .with_npt_1gb_page_support(true)
        .with_cmpccxadd_support(true)
        .with_prefetch_i_support(true)
        .with_sha512_support(true)
        .with_rfds_no_support(true)
        .with_rfds_clear_support(true)
        .with_sm3_support(true)
        .with_sm4_support(true)
}

/// XSAVE features that we support exposing to guests.
fn supported_xsave_features() -> hvdef::HvX64PartitionProcessorXsaveFeatures {
    hvdef::HvX64PartitionProcessorXsaveFeatures::new()
        .with_xsave_support(true)
        .with_xsaveopt_support(true)
        .with_avx_support(true)
        .with_avx2_support(true)
        .with_fma_support(true)
        .with_mpx_support(true)
        .with_avx512_support(true)
        .with_avx512_dq_support(true)
        .with_avx512_cd_support(true)
        .with_avx512_bw_support(true)
        .with_avx512_vl_support(true)
        .with_xsave_comp_support(true)
        .with_xsave_supervisor_support(true)
        .with_xcr1_support(true)
        .with_avx512_bitalg_support(true)
        .with_avx512_ifma_support(true)
        .with_avx512_vbmi_support(true)
        .with_avx512_vbmi2_support(true)
        .with_avx512_vnni_support(true)
        .with_gfni_support(true)
        .with_vaes_support(true)
        .with_avx512_vpopcntdq_support(true)
        .with_vpclmulqdq_support(true)
        .with_avx512_bf16_support(true)
        .with_avx512_vp2_intersect_support(true)
        .with_avx512_fp16_support(true)
        .with_xfd_support(true)
        .with_amx_tile_support(true)
        .with_amx_bf16_support(true)
        .with_amx_int8_support(true)
        .with_avx_vnni_support(true)
        .with_avx_ifma_support(true)
        .with_avx_ne_convert_support(true)
        .with_avx_vnni_int8_support(true)
        .with_avx_vnni_int16_support(true)
        .with_avx10_1_256_support(true)
        .with_avx10_1_512_support(true)
        .with_amx_fp16_support(true)
}
