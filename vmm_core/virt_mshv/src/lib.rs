// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux /dev/mshv implementation of the virt::generic interfaces.

#![cfg(all(target_os = "linux", guest_is_native, guest_arch = "x86_64"))]
#![expect(missing_docs)]
// UNSAFETY: Calling HV APIs and manually managing memory.
#![expect(unsafe_code)]

pub mod irqfd;
mod vm_state;
mod vp_state;

use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use hv1_emulator::message_queues::MessageQueues;
use hv1_hypercall::X64RegisterIo;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvX64RegisterName;
use hvdef::HvX64RegisterPage;
use hvdef::Vtl;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_EXECUTE;
use hvdef::hypercall::HvRegisterAssoc;
use inspect::Inspect;
use inspect::InspectMut;
use mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE;
use mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE;
use mshv_bindings::mshv_install_intercept;
use mshv_bindings::mshv_user_mem_region;
use mshv_ioctls::InterruptRequest;
use mshv_ioctls::Mshv;
use mshv_ioctls::MshvError;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;
use mshv_ioctls::set_bits;
use pal::unix::pthread::*;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use pci_core::msi::SignalMsi;
use std::convert::Infallible;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::Once;
use std::sync::Weak;
use thiserror::Error;
use virt::Hv1;
use virt::NeedsYield;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::state::StateElement as _;
use virt::x86::apic_software_device::ApicSoftwareDevice;
use virt::x86::apic_software_device::ApicSoftwareDevices;
use virt::x86::max_physical_address_size_from_cpuid;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use vm_topology::processor::TargetVpInfo;
use vmcore::interrupt::Interrupt;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::synic::GuestEventPort;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use zerocopy::IntoBytes;

/// Extension trait for [`VcpuFd`] to accept hvdef register types directly.
trait VcpuFdExt {
    fn get_hvdef_regs(&self, regs: &mut [HvRegisterAssoc]) -> Result<(), MshvError>;
    fn set_hvdef_regs(&self, regs: &[HvRegisterAssoc]) -> Result<(), MshvError>;
}

impl VcpuFdExt for VcpuFd {
    fn get_hvdef_regs(&self, regs: &mut [HvRegisterAssoc]) -> Result<(), MshvError> {
        use mshv_bindings::hv_register_assoc;
        const {
            assert!(size_of::<HvRegisterAssoc>() == size_of::<hv_register_assoc>());
            assert!(align_of::<HvRegisterAssoc>() >= align_of::<hv_register_assoc>());
        }
        // SAFETY: HvRegisterAssoc and hv_register_assoc have the same layout.
        self.get_reg(unsafe {
            std::mem::transmute::<&mut [HvRegisterAssoc], &mut [hv_register_assoc]>(regs)
        })
    }

    fn set_hvdef_regs(&self, regs: &[HvRegisterAssoc]) -> Result<(), MshvError> {
        use mshv_bindings::hv_register_assoc;
        const {
            assert!(size_of::<HvRegisterAssoc>() == size_of::<hv_register_assoc>());
            assert!(align_of::<HvRegisterAssoc>() >= align_of::<hv_register_assoc>());
        }
        // SAFETY: HvRegisterAssoc and hv_register_assoc have the same layout.
        self.set_reg(unsafe {
            std::mem::transmute::<&[HvRegisterAssoc], &[hv_register_assoc]>(regs)
        })
    }
}

#[derive(Debug)]
pub struct LinuxMshv;

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
            return Err(Error::IsolationNotSupported);
        }

        // Open /dev/mshv.
        let mshv = Mshv::new().map_err(Error::OpenMshv)?;

        // Build partition creation flags based on the requested
        // configuration. LAPIC is always enabled (the hypervisor emulates
        // the local APIC). X2APIC is only enabled when the topology
        // requests it.
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

        // pt_cpu_fbanks expects *disabled* processor features (bit = 1
        // means disabled). Invert our supported masks so unsupported
        // features are disabled. The hypervisor will further intersect
        // with host capabilities.
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

        // Create the VM with our explicit partition configuration.
        let vmfd: VmFd;
        loop {
            match mshv.create_vm_with_args(&create_args) {
                Ok(fd) => vmfd = fd,
                Err(e) => {
                    if e.errno() == libc::EINTR {
                        continue;
                    } else {
                        return Err(Error::CreateVMFailed);
                    }
                }
            }
            break;
        }

        // Set synthetic processor features before initialization when the
        // guest interface is configured. These control which Hyper-V
        // enlightenments are exposed to the guest.
        if config.hv_config.is_some() {
            let synthetic_features = hvdef::HvPartitionSyntheticProcessorFeatures::new()
                .with_hypervisor_present(true)
                .with_hv1(true)
                .with_access_vp_run_time_reg(true)
                .with_access_partition_reference_counter(true)
                .with_access_synic_regs(true)
                .with_access_synthetic_timer_regs(true)
                .with_access_intr_ctrl_regs(true)
                .with_access_hypercall_regs(true)
                .with_access_vp_index(true)
                .with_access_partition_reference_tsc(true)
                .with_access_guest_idle_reg(true)
                .with_access_frequency_regs(true)
                .with_enable_extended_gva_ranges_for_flush_virtual_address_list(true)
                .with_fast_hypercall_output(true)
                .with_direct_synthetic_timers(true)
                .with_extended_processor_masks(true)
                .with_tb_flush_hypercalls(true)
                .with_synthetic_cluster_ipi(true)
                .with_notify_long_spin_wait(true)
                .with_query_numa_distance(true)
                .with_signal_events(true)
                .with_retarget_device_interrupt(true);

            vmfd.set_partition_property(
                mshv_bindings::hv_partition_property_code_HV_PARTITION_PROPERTY_SYNTHETIC_PROC_FEATURES,
                u64::from(synthetic_features),
            )
            .map_err(|e| Error::SetPartitionProperty(e.into()))?;
        }

        vmfd.initialize()
            .map_err(|e| Error::CreateVMInitFailed(e.into()))?;

        // Tell the hypervisor how many VPs are in each socket so it can
        // generate the correct topology CPUID leaves (01h, 04h, 0Bh, 1Fh,
        // AMD 80000008h/1Dh/1Eh) automatically.
        vmfd.set_partition_property(
            mshv_bindings::hv_partition_property_code_HV_PARTITION_PROPERTY_PROCESSORS_PER_SOCKET,
            config.processor_topology.reserved_vps_per_socket() as u64,
        )
        .map_err(|e| Error::SetPartitionProperty(e.into()))?;

        // Create virtual CPUs.
        let mut vps: Vec<MshvVpInner> = Vec::new();

        // /dev/mshv only supports 256 VPs right now for some reason.
        if config.processor_topology.vp_count() > u8::MAX as u32 {
            return Err(Error::TooManyVps(config.processor_topology.vp_count()));
        }

        // We have to create the BSP now to get access to some partition state.
        // Everything else is created later.
        let bsp = vmfd.create_vcpu(0).map_err(Error::CreateVcpu)?;

        for vp in config.processor_topology.vps_arch() {
            vps.push(MshvVpInner {
                vp_info: vp,
                thread: RwLock::new(None),
                needs_yield: NeedsYield::new(),
                message_queues: MessageQueues::new(),
                deliverability_notifications: Mutex::new(
                    HvDeliverabilityNotificationsRegister::new(),
                ),
            });
        }

        // Install required intercepts
        let intercept_args = mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type: hvdef::hypercall::HvInterceptType::HvInterceptTypeHypercall.0,
            intercept_parameter: Default::default(),
        };
        vmfd.install_intercept(intercept_args)
            .map_err(Error::InstallIntercept)?;

        // Intercept unknown SynIC connections so the VMM can handle
        // HvPostMessage / HvSignalEvent for guest-initiated connections
        // that the hypervisor doesn't recognise natively.
        vmfd.install_intercept(mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type:
                hvdef::hypercall::HvInterceptType::HvInterceptTypeUnknownSynicConnection.0,
            intercept_parameter: Default::default(),
        })
        .map_err(Error::InstallIntercept)?;

        // Intercept HvRetargetDeviceInterrupt for device IDs the
        // hypervisor doesn't know about (i.e. all software VPCI devices)
        // so the VMM can update the interrupt routing table.
        vmfd.install_intercept(mshv_install_intercept {
            access_type_mask: HV_INTERCEPT_ACCESS_MASK_EXECUTE,
            intercept_type:
                hvdef::hypercall::HvInterceptType::HvInterceptTypeRetargetInterruptWithUnknownDeviceId.0,
            intercept_parameter: Default::default(),
        })
        .map_err(Error::InstallIntercept)?;

        // Set up a signal for forcing vcpufd.run() ioctl to exit.
        static SIGNAL_HANDLER_INIT: Once = Once::new();
        // SAFETY: The signal handler does not perform any actions that are forbidden
        // for signal handlers to perform, as it performs nothing.
        SIGNAL_HANDLER_INIT.call_once(|| unsafe {
            signal_hook::low_level::register(libc::SIGRTMIN(), || {
                // Signal handler does nothing other than enabling run_fd() iotcl to
                // return with EINTR, when the associated signal is sent to run_fd() thread.
            })
            .unwrap();
        });

        if let Some(hv_config) = &config.hv_config {
            if hv_config.vtl2.is_some() {
                return Err(Error::Vtl2NotSupported);
            }
        }

        Ok(MshvProtoPartition {
            config,
            vmfd,
            vps,
            bsp,
        })
    }
}

/// Returns whether MSHV is available on this machine.
pub fn is_available() -> Result<bool, Error> {
    match std::fs::metadata("/dev/mshv") {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::AvailableCheck(err)),
    }
}

/// Prototype partition.
pub struct MshvProtoPartition<'a> {
    config: ProtoPartitionConfig<'a>,
    vmfd: VmFd,
    vps: Vec<MshvVpInner>,
    bsp: VcpuFd,
}

impl ProtoPartition for MshvProtoPartition<'_> {
    type Partition = MshvPartition;
    type ProcessorBinder = MshvProcessorBinder;
    type Error = Error;

    fn max_physical_address_size(&self) -> u8 {
        max_physical_address_size_from_cpuid(&|eax, ecx| {
            self.bsp
                .get_cpuid_values(eax, ecx, 0, 0)
                .expect("cpuid should not fail")
        })
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        // Use the non-topology CPUID overrides from the caller (feature
        // flags, etc.). Topology CPUID leaves are handled natively by
        // the hypervisor via the ProcessorsPerSocket partition property
        // and SMT creation flag set during partition creation.
        let cpuid = virt::CpuidLeafSet::new(config.cpuid.to_vec());

        // Apply CPUID overrides partition-wide.
        // The hypervisor handles per-VP APIC ID fixups in topology leaves
        // automatically.
        for leaf in cpuid.leaves().iter() {
            let input = hvdef::hypercall::RegisterInterceptResultCpuid {
                partition_id: 0, // overwritten by kernel
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
            self.vmfd.hvcall(&mut args).map_err(Error::RegisterCpuid)?;
        }

        // Get caps via cpuid
        let caps = virt::PartitionCapabilities::from_cpuid(
            self.config.processor_topology,
            &mut |function, index| {
                self.bsp
                    .get_cpuid_values(function, index, 0, 0)
                    .expect("cpuid should not fail")
            },
        )
        .map_err(Error::Capabilities)?;

        let apic_id_map = self
            .config
            .processor_topology
            .vps_arch()
            .map(|vp| vp.apic_id)
            .collect();

        // Attach all the resources created above to a Partition object.
        let inner = Arc::new(MshvPartitionInner {
            vmfd: self.vmfd,
            memory: Default::default(),
            gm: config.guest_memory.clone(),
            vps: self.vps,
            irq_routes: Default::default(),
            gsi_states: Mutex::new(Box::new([irqfd::GsiState::Unallocated; irqfd::NUM_GSIS])),
            caps,
            synic_ports: Default::default(),
            cpuid,
            software_devices: ApicSoftwareDevices::new(apic_id_map),
        });

        let partition = MshvPartition {
            synic_ports: Arc::new(virt::synic::SynicPorts::new(inner.clone())),
            inner,
        };

        let mut bsp = Some(self.bsp);
        let vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp| MshvProcessorBinder {
                partition: partition.inner.clone(),
                vpindex: vp.vp_index,
                vcpufd: bsp.take(),
            })
            .collect();

        Ok((partition, vps))
    }
}

#[derive(Inspect)]
pub struct MshvPartition {
    #[inspect(flatten)]
    inner: Arc<MshvPartitionInner>,
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<MshvPartitionInner>>,
}

#[derive(Inspect)]
struct MshvPartitionInner {
    #[inspect(skip)]
    vmfd: VmFd,
    #[inspect(skip)]
    memory: Mutex<MshvMemoryRangeState>,
    gm: GuestMemory,
    #[inspect(skip)]
    vps: Vec<MshvVpInner>,
    irq_routes: virt::irqcon::IrqRoutes,
    #[inspect(skip)]
    gsi_states: Mutex<Box<[irqfd::GsiState; irqfd::NUM_GSIS]>>,
    caps: virt::PartitionCapabilities,
    synic_ports: virt::synic::SynicPortMap,
    cpuid: virt::CpuidLeafSet,
    software_devices: ApicSoftwareDevices,
}

struct MshvVpInner {
    vp_info: TargetVpInfo,
    thread: RwLock<Option<Pthread>>,
    needs_yield: NeedsYield,
    message_queues: MessageQueues,
    deliverability_notifications: Mutex<HvDeliverabilityNotificationsRegister>,
}

struct MshvVpInnerCleaner<'a> {
    vpinner: &'a MshvVpInner,
}

impl Drop for MshvVpInnerCleaner<'_> {
    fn drop(&mut self) {
        self.vpinner.thread.write().take();
    }
}

impl virt::Partition for MshvPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = Error>> {
        None
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
        Some(Arc::new(irqfd::MshvIrqFd::new(self.inner.clone())))
    }

    fn request_yield(&self, vp_index: VpIndex) {
        let vp = self.inner.vp(vp_index);
        if vp.needs_yield.request_yield() {
            // Send a signal to the thread who called vcpufd.run() to force an exit.
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

impl PartitionAccessState for MshvPartition {
    type StateAccess<'a> = &'a MshvPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);

        self
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
        self.inner
            .software_devices
            .new_device(self.inner.clone(), device_id)
            .map_err(Error::NewDevice)
    }
}

impl GetReferenceTime for MshvPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        // Use the partition property instead of a VP register to avoid
        // deadlocking when VPs are running.
        let ref_time = self
            .vmfd
            .get_partition_property(
                mshv_bindings::hv_partition_property_code_HV_PARTITION_PROPERTY_REFERENCE_TIME,
            )
            .unwrap();
        ReferenceTimeResult {
            ref_time,
            system_time: None,
        }
    }
}

impl MshvPartitionInner {
    fn vp(&self, vp_index: VpIndex) -> &MshvVpInner {
        &self.vps[vp_index.index() as usize]
    }

    fn post_message(&self, vp_index: VpIndex, sint: u8, message: &HvMessage) {
        let request_notification = self
            .vp(vp_index)
            .message_queues
            .enqueue_message(sint, message);

        if request_notification {
            self.request_sint_notifications(vp_index, 1 << sint);
        }
    }

    fn request_sint_notifications(&self, vp_index: VpIndex, sints: u16) {
        let mut notifications = self.vp(vp_index).deliverability_notifications.lock();
        if notifications.sints() != sints {
            notifications.set_sints(sints);
            self.vmfd
                .register_deliverabilty_notifications(vp_index.index(), (*notifications).into())
                .expect("Requesting deliverability is not a fallable operation");
        }
    }
}

pub struct MshvProcessorBinder {
    partition: Arc<MshvPartitionInner>,
    vcpufd: Option<VcpuFd>,
    vpindex: VpIndex,
}

struct MshvVpRunner<'a> {
    vcpufd: &'a VcpuFd,
    reg_page: *mut HvX64RegisterPage,
}

impl MshvVpRunner<'_> {
    fn run(&mut self) -> Result<HvMessage, MshvError> {
        self.vcpufd.run().map(|msg| {
            // SAFETY: hv_message and HvMessage have the same size
            // (256 bytes) and compatible layout (header + 240-byte
            // payload).
            unsafe { std::mem::transmute::<mshv_bindings::hv_message, HvMessage>(msg) }
        })
    }

    fn reg_page(&mut self) -> &mut HvX64RegisterPage {
        // SAFETY: VP is stopped (returned from run()), so we have exclusive
        // access. The raw pointer was obtained from the kernel's mmap of
        // the register page and remains valid for the VP's lifetime.
        unsafe { &mut *self.reg_page }
    }
}

impl virt::BindProcessor for MshvProcessorBinder {
    type Processor<'a>
        = MshvProcessor<'a>
    where
        Self: 'a;
    type Error = Error;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let inner = &self.partition.vps[self.vpindex.index() as usize];

        if self.vcpufd.is_none() {
            let vcpufd = self
                .partition
                .vmfd
                .create_vcpu(u8::try_from(self.vpindex.index()).expect("validated above"))
                .map_err(Error::CreateVcpu)?;
            self.vcpufd = Some(vcpufd);
        };

        let vcpufd = self.vcpufd.as_ref().unwrap();

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
        };

        // Set the APIC state: APIC IDs and APIC base register (the latter to
        // make sure the X2APIC enabled state is consistent with the partition
        // settings).
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

        // When X2APIC is supported, the APIC ID register is statically assigned
        // to the initial APIC ID and cannot be changed. When it is not supported,
        // it must be explicitly set.
        let reg_count = if this.partition.caps.x2apic { 2 } else { 3 };

        vcpufd
            .set_hvdef_regs(&regs[..reg_count])
            .map_err(Error::Register)?;

        Ok(this)
    }
}

pub struct MshvProcessor<'a> {
    partition: &'a MshvPartitionInner,
    inner: &'a MshvVpInner,
    vpindex: VpIndex,
    runner: MshvVpRunner<'a>,
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

    fn handle_synic_deliverable_exit(&self, message: &HvMessage, _devices: &impl CpuIo) {
        let info = message.as_message::<hvdef::HvX64SynicSintDeliverableMessage>();
        self.flush_messages(info.deliverable_sints);
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

    fn flush_messages(&self, deliverable_sints: u16) {
        let nonempty_sints =
            self.inner
                .message_queues
                .post_pending_messages(deliverable_sints, |sint, message| {
                    match self.partition.vmfd.post_message_direct(
                        self.vpindex.index(),
                        sint,
                        message.as_bytes(),
                    ) {
                        Ok(()) => {
                            tracing::trace!(sint, "sint message posted successfully");
                            Ok(())
                        }
                        Err(e) => {
                            // TODO: handle errors appropriately
                            tracing::trace!(error = %e, "dropping sint message");
                            Err(HvError::ObjectInUse)
                        }
                    }
                });

        {
            // To avoid an additional get_reg hypercall, clear w/ deliverable sints mask
            let mut notifications = self.inner.deliverability_notifications.lock();
            let remaining_sints = notifications.sints() & !deliverable_sints;
            notifications.set_sints(remaining_sints);
        }

        if nonempty_sints != 0 {
            self.partition
                .request_sint_notifications(self.vpindex, nonempty_sints);
        }
    }
}

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

    fn flush(&mut self) {
        // Writes already went to the register page with dirty bits set.
        // No additional work needed.
    }

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
        // TODO: No VTL2 supported so always return Ok.
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

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        self.partition
            .memory
            .lock()
            .ranges
            .iter()
            .flatten()
            .any(|range| {
                (range.guest_pfn..range.guest_pfn + range.size).contains(&gpa)
                    && (!write
                        || range.flags & set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE)
                            == set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE))
            })
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

    fn acquire_tlb_lock(&mut self) {
        // The hypervisor automatically acquires the TLB lock for exo partitions.
    }

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

// TODO: Chunk this up into smaller types.
#[derive(Error, Debug)]
pub enum Error {
    #[error("operation not supported")]
    NotSupported,
    #[error("create_vm failed")]
    CreateVMFailed,
    #[error("failed to initialize VM")]
    CreateVMInitFailed(#[source] anyhow::Error),
    #[error("failed to create VCPU")]
    CreateVcpu(#[source] MshvError),
    #[error("vtl2 not supported")]
    Vtl2NotSupported,
    #[error("isolation not supported")]
    IsolationNotSupported,
    #[error("failed to stat /dev/mshv")]
    AvailableCheck(#[source] io::Error),
    #[error("failed to open /dev/mshv")]
    OpenMshv(#[source] MshvError),
    #[error("failed to set partition property")]
    SetPartitionProperty(#[source] anyhow::Error),
    #[error("register access error")]
    Register(#[source] MshvError),
    #[error("install instercept failed")]
    InstallIntercept(#[source] MshvError),
    #[error("failed to register cpuid override")]
    RegisterCpuid(#[source] MshvError),
    #[error("host does not support required cpu capabilities")]
    Capabilities(#[source] virt::PartitionCapabilitiesError),
    #[error("too many virtual processors: {0}")]
    TooManyVps(u32),
    #[error("failed to create virtual device")]
    NewDevice(#[source] virt::x86::apic_software_device::DeviceIdInUse),
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
    fn signal_msi(&self, _rid: u32, address: u64, data: u32) {
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

#[derive(Debug, Default)]
struct MshvMemoryRangeState {
    ranges: Vec<Option<mshv_user_mem_region>>,
}

impl virt::PartitionMemoryMapper for MshvPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

// TODO: figure out a better abstraction that also works for KVM and WHP.
impl virt::PartitionMemoryMap for MshvPartitionInner {
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        let mut state = self.memory.lock();

        // Memory slots cannot be resized but can be moved within the guest
        // address space. Find the existing slot if there is one.
        let mut slot_to_use = None;
        for (slot, range) in state.ranges.iter_mut().enumerate() {
            match range {
                Some(range) if range.userspace_addr == data as u64 => {
                    slot_to_use = Some(slot);
                    break;
                }
                Some(_) => (),
                None => slot_to_use = Some(slot),
            }
        }
        if slot_to_use.is_none() {
            slot_to_use = Some(state.ranges.len());
            state.ranges.push(None);
        }
        let slot_to_use = slot_to_use.unwrap();

        let mut flags = 0;
        if writable {
            flags |= set_bits!(u8, MSHV_SET_MEM_BIT_WRITABLE);
        }
        if exec {
            flags |= set_bits!(u8, MSHV_SET_MEM_BIT_EXECUTABLE);
        }
        let mem_region = mshv_user_mem_region {
            size: size as u64,
            guest_pfn: addr >> HV_PAGE_SHIFT,
            userspace_addr: data as u64,
            flags,
            rsvd: [0; 7],
        };

        self.vmfd.map_user_memory(mem_region)?;
        state.ranges[slot_to_use] = Some(mem_region);
        Ok(())
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let unmap_start = addr >> HV_PAGE_SHIFT;
        let unmap_end = (addr + size) >> HV_PAGE_SHIFT;
        let mut state = self.memory.lock();
        for entry in &mut state.ranges {
            let Some(region) = entry.as_ref() else {
                continue;
            };
            let region_start = region.guest_pfn;
            let region_end = region.guest_pfn + (region.size >> HV_PAGE_SHIFT);
            if unmap_start <= region_start && region_end <= unmap_end {
                // Region is fully contained in the unmap range.
                self.vmfd.unmap_user_memory(*region)?;
                *entry = None;
            } else {
                assert!(
                    region_end <= unmap_start || unmap_end <= region_start,
                    "unmap range partially overlaps a mapped region"
                );
            }
        }
        Ok(())
    }
}

/// Holds the state needed to deassign an MSHV ioeventfd on drop.
///
/// The kernel's `mshv_deassign_ioeventfd` matches entries by (eventfd,
/// addr, len, datamatch/wildcard), so we must keep all of these alive
/// for the deassign ioctl.
struct MshvDoorbellEntry {
    partition: Weak<MshvPartitionInner>,
    event: Event,
    guest_address: u64,
    datamatch: u64,
    len: u32,
    flags: u32,
}

impl MshvDoorbellEntry {
    fn new(
        partition: &Arc<MshvPartitionInner>,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<MshvDoorbellEntry> {
        let flags = if value.is_some() {
            1 << mshv_bindings::MSHV_IOEVENTFD_BIT_DATAMATCH
        } else {
            0
        };
        let datamatch = value.unwrap_or(0);
        let len = length.unwrap_or(0);
        let event = fd.clone();

        let ioeventfd = mshv_bindings::mshv_user_ioeventfd {
            datamatch,
            addr: guest_address,
            len,
            fd: event.as_fd().as_raw_fd(),
            flags,
            ..Default::default()
        };
        // SAFETY: `partition.vmfd` is valid because it is owned by
        // `MshvPartitionInner`. The `ioeventfd` struct is properly
        // initialized on the stack.
        let ret = unsafe {
            libc::ioctl(
                partition.vmfd.as_raw_fd(),
                mshv_ioctls::MSHV_IOEVENTFD() as _,
                std::ptr::from_ref(&ioeventfd),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            partition: Arc::downgrade(partition),
            event,
            guest_address,
            datamatch,
            len,
            flags,
        })
    }
}

impl Drop for MshvDoorbellEntry {
    fn drop(&mut self) {
        if let Some(partition) = self.partition.upgrade() {
            let ioeventfd = mshv_bindings::mshv_user_ioeventfd {
                datamatch: self.datamatch,
                addr: self.guest_address,
                len: self.len,
                fd: self.event.as_fd().as_raw_fd(),
                flags: self.flags | (1 << mshv_bindings::MSHV_IOEVENTFD_BIT_DEASSIGN),
                ..Default::default()
            };
            // SAFETY: `partition.vmfd` is valid because we successfully
            // upgraded the weak reference. The `ioeventfd` struct is
            // properly initialized on the stack.
            let ret = unsafe {
                libc::ioctl(
                    partition.vmfd.as_raw_fd(),
                    mshv_ioctls::MSHV_IOEVENTFD() as _,
                    std::ptr::from_ref(&ioeventfd),
                )
            };
            assert!(
                ret >= 0,
                "failed to unregister doorbell at {:#x}: {}",
                self.guest_address,
                io::Error::last_os_error()
            );
        }
    }
}

impl DoorbellRegistration for MshvPartition {
    fn register_doorbell(
        &self,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(MshvDoorbellEntry::new(
            &self.inner,
            guest_address,
            value,
            length,
            fd,
        )?))
    }
}

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

struct MshvHypercallHandler<'a> {
    partition: &'a MshvPartitionInner,
    reg_page: &'a mut HvX64RegisterPage,
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

impl hv1_hypercall::PostMessage for MshvHypercallHandler<'_> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_post_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl hv1_hypercall::SignalEvent for MshvHypercallHandler<'_> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_signal_event(Vtl::Vtl0, connection_id, flag)
    }
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

impl InspectMut for MshvProcessor<'_> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl virt::Processor for MshvProcessor<'_> {
    type StateAccess<'a>
        = &'a mut Self
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <&mut Self as virt::vp::AccessVpState>::Error> {
        Err(Error::NotSupported)
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        let vpinner = self.inner;
        let _cleaner = MshvVpInnerCleaner { vpinner };

        // Ensure this thread is uniquely running the VP, and store the thread
        // ID to support cancellation.
        assert!(vpinner.thread.write().replace(Pthread::current()).is_none());

        loop {
            vpinner.needs_yield.maybe_yield().await;
            stop.check()?;

            match self.runner.run() {
                Ok(exit) => match exit.header.typ {
                    HvMessageType::HvMessageTypeUnrecoverableException => {
                        return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                    }
                    HvMessageType::HvMessageTypeX64IoPortIntercept => {
                        self.handle_io_port_intercept(&exit, dev).await?;
                    }
                    HvMessageType::HvMessageTypeUnmappedGpa
                    | HvMessageType::HvMessageTypeGpaIntercept => {
                        self.handle_mmio_intercept(&exit, dev).await?;
                    }
                    HvMessageType::HvMessageTypeSynicSintDeliverable => {
                        tracing::trace!("SYNIC_SINT_DELIVERABLE");
                        self.handle_synic_deliverable_exit(&exit, dev);
                    }
                    HvMessageType::HvMessageTypeHypercallIntercept => {
                        tracing::trace!("HYPERCALL_INTERCEPT");
                        self.handle_hypercall_intercept(&exit, dev);
                    }
                    HvMessageType::HvMessageTypeX64ApicEoi => {
                        let msg = exit.as_message::<hvdef::HvX64ApicEoiMessage>();
                        dev.handle_eoi(msg.interrupt_vector);
                    }
                    exit => {
                        panic!("Unhandled vcpu exit code {exit:?}");
                    }
                },

                Err(e) => match e.errno() {
                    libc::EAGAIN | libc::EINTR => {}
                    _ => tracing::error!(
                        error = &e as &dyn std::error::Error,
                        "vcpufd.run returned error"
                    ),
                },
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        self
    }
}

impl virt::synic::Synic for MshvPartitionInner {
    fn port_map(&self) -> &virt::synic::SynicPortMap {
        &self.synic_ports
    }

    fn post_message(&self, _vtl: Vtl, vp: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        self.post_message(vp, sint, &HvMessage::new(HvMessageType(typ), 0, payload));
    }

    fn new_guest_event_port(
        self: Arc<Self>,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(MshvGuestEventPort {
            partition: Arc::downgrade(&self),
            params: Arc::new(Mutex::new(MshvEventPortParams {
                vp: VpIndex::new(vp),
                sint,
                flag,
            })),
        })
    }

    fn prefer_os_events(&self) -> bool {
        false
    }
}

/// `GuestEventPort` implementation for MSHV partitions.
#[derive(Debug, Clone)]
struct MshvGuestEventPort {
    partition: Weak<MshvPartitionInner>,
    params: Arc<Mutex<MshvEventPortParams>>,
}

#[derive(Debug, Copy, Clone)]
struct MshvEventPortParams {
    vp: VpIndex,
    sint: u8,
    flag: u16,
}

impl GuestEventPort for MshvGuestEventPort {
    fn interrupt(&self) -> Interrupt {
        let partition = self.partition.clone();
        let params = self.params.clone();
        Interrupt::from_fn(move || {
            let MshvEventPortParams { vp, sint, flag } = *params.lock();
            if let Some(partition) = partition.upgrade() {
                partition
                    .vmfd
                    .signal_event_direct(vp.index(), sint, flag)
                    .unwrap_or_else(|_| {
                        panic!(
                            "Failed signal synic sint {} on vp {:?} with flag {}",
                            sint, vp, flag
                        )
                    });
            }
        })
    }

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.params.lock().vp = VpIndex::new(vp);
        Ok(())
    }
}

/// Processor features (bank 0) that we support exposing to guests.
/// This matches the set that WHP enables by default.
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
/// This matches the set that WHP enables by default.
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
/// This matches the set that WHP enables by default.
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
