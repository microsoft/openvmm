// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements support for KVM on x86_64.

#![cfg(all(target_os = "linux", guest_arch = "x86_64"))]

mod regs;
mod vm_state;
mod vp_state;

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::KvmProcessorBinder;
use crate::KvmRunVpError;
use crate::gsi;
use crate::gsi::GsiRouting;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hv1_emulator::message_queues::MessageQueues;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvSynicScontrol;
use hvdef::HvSynicSimpSiefp;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall::Control;
use inspect::Inspect;
use inspect::InspectMut;
use kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
use kvm::kvm_ioeventfd_flag_nr_datamatch;
use kvm::kvm_ioeventfd_flag_nr_deassign;
use pal_event::Event;
use parking_lot::Mutex;
use parking_lot::RwLock;
use pci_core::msi::MsiControl;
use pci_core::msi::MsiInterruptTarget;
use std::convert::Infallible;
use std::future::poll_fn;
use std::io;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;
use thiserror::Error;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::Hv1;
use virt::NeedsYield;
use virt::Partition;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::Processor;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::ResetPartition;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::DeliveryMode;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;
use virt::state::StateElement;
use virt::vm::AccessVmState;
use virt::x86::HardwareBreakpoint;
use virt::x86::max_physical_address_size_from_cpuid;
use virt::x86::vp::AccessVpState;
use vm_topology::processor::x86::ApicMode;
use vm_topology::processor::x86::X86VpInfo;
use vmcore::interrupt::Interrupt;
use vmcore::reference_time::GetReferenceTime;
use vmcore::reference_time::ReferenceTimeResult;
use vmcore::reference_time::ReferenceTimeSource;
use vmcore::synic::GuestEventPort;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;
use vp_state::KvmVpStateAccess;
use x86defs::cpuid::CpuidFunction;
use x86defs::msi::MsiAddress;
use x86defs::msi::MsiData;
use zerocopy::IntoBytes;

// HACK: on certain machines, pcat spams these MSRs during boot.
//
// As a workaround, avoid injecting a GFP on these mystery MSRs until we can get
// to the bottom of what's going on here.
const MYSTERY_MSRS: &[u32] = &[0x88, 0x89, 0x8a, 0x116, 0x118, 0x119, 0x11a, 0x11b, 0x11e];

#[derive(Debug)]
pub struct Kvm;

/// CPUID leaf and flag for GB page support.
const GB_PAGE_LEAF: u32 = 0x80000001;
const GB_PAGE_FLAG: u32 = 1 << 26;

/// Returns whether the host supports GB pages in the page table.
fn gb_pages_supported() -> bool {
    safe_intrinsics::cpuid(0x80000000, 0).eax >= GB_PAGE_LEAF
        && safe_intrinsics::cpuid(GB_PAGE_LEAF, 0).edx & GB_PAGE_FLAG != 0
}

impl virt::Hypervisor for Kvm {
    type ProtoPartition<'a> = KvmProtoPartition<'a>;
    type Partition = KvmPartition;
    type Error = KvmError;

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        if config.isolation.is_isolated() {
            return Err(KvmError::IsolationNotSupported);
        }

        let kvm = kvm::Kvm::new()?;
        let mut cpuid_entries = kvm
            .supported_cpuid()?
            .into_iter()
            .filter_map(|entry| {
                // Filter out KVM CPUID entries.
                if entry.function & 0xf0000000 == 0x40000000 {
                    return None;
                }
                let mut leaf =
                    CpuidLeaf::new(entry.function, [entry.eax, entry.ebx, entry.ecx, entry.edx]);
                if entry.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX != 0 {
                    leaf = leaf.indexed(entry.index);
                }
                Some(leaf)
            })
            .collect::<Vec<_>>();

        // Add in GB page support based on the host's capabilities. This bit
        // is incorrectly stripped by some versions of KVM (but is important
        // to have for our UEFI implementation).
        if gb_pages_supported()
            && cpuid_entries
                .iter()
                .any(|x| x.function == CpuidFunction::ExtendedVersionAndFeatures.0)
        {
            cpuid_entries.push(
                CpuidLeaf::new(
                    CpuidFunction::ExtendedVersionAndFeatures.0,
                    [0, 0, 0, GB_PAGE_FLAG],
                )
                .masked([0, 0, 0, GB_PAGE_FLAG]),
            );
        }

        match config.processor_topology.apic_mode() {
            ApicMode::XApic => {
                // Disable X2APIC.
                cpuid_entries.push(
                    CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, 0, 0]).masked([
                        0,
                        0,
                        1 << 21,
                        0,
                    ]),
                );
            }
            ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled => {}
        }

        // SGX is not supported on KVM.
        cpuid_entries.push(
            CpuidLeaf::new(CpuidFunction::SgxEnumeration.0, [0; 4]).indexed(2), // SGX enumeration is subleaf 2
        );

        if let Some(hv_config) = &config.hv_config {
            if hv_config.vtl2.is_some() {
                return Err(KvmError::Vtl2NotSupported);
            }

            let split_u128 = |x: u128| -> [u32; 4] {
                let bytes = x.to_le_bytes();
                [
                    u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                    u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                    u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                ]
            };

            use hvdef::*;
            let privileges = HvPartitionPrivilege::new()
                .with_access_partition_reference_counter(true)
                .with_access_hypercall_msrs(true)
                .with_access_vp_index(true)
                .with_access_frequency_msrs(true)
                .with_access_synic_msrs(true)
                .with_access_synthetic_timer_msrs(true)
                .with_access_vp_runtime_msr(true)
                .with_access_apic_msrs(true);

            let hv_cpuid = &[
                CpuidLeaf::new(
                    HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
                    [
                        HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS,
                        u32::from_le_bytes(*b"Micr"),
                        u32::from_le_bytes(*b"osof"),
                        u32::from_le_bytes(*b"t Hv"),
                    ],
                ),
                CpuidLeaf::new(
                    HV_CPUID_FUNCTION_HV_INTERFACE,
                    [u32::from_le_bytes(*b"Hv#1"), 0, 0, 0],
                ),
                CpuidLeaf::new(HV_CPUID_FUNCTION_MS_HV_VERSION, [0, 0, 0, 0]),
                CpuidLeaf::new(
                    HV_CPUID_FUNCTION_MS_HV_FEATURES,
                    split_u128(u128::from(
                        HvFeatures::new()
                            .with_privileges(privileges)
                            .with_frequency_regs_available(true),
                    )),
                ),
                CpuidLeaf::new(
                    HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION,
                    split_u128(
                        HvEnlightenmentInformation::new()
                            .with_deprecate_auto_eoi(true)
                            .with_long_spin_wait_count(0xffffffff) // no spin wait notifications
                            .into(),
                    ),
                ),
            ];

            cpuid_entries.extend(hv_cpuid);
        }

        let cpuid_entries = CpuidLeafSet::new(cpuid_entries);

        let vm = kvm.new_vm()?;
        vm.enable_split_irqchip(virt::irqcon::IRQ_LINES as u32)?;
        vm.enable_x2apic_api()?;
        vm.enable_unknown_msr_exits()?;

        Ok(KvmProtoPartition {
            vm,
            config,
            cpuid: cpuid_entries,
        })
    }

    fn is_available(&self) -> Result<bool, Self::Error> {
        match std::fs::metadata("/dev/kvm") {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(KvmError::AvailableCheck(err)),
        }
    }
}

/// A prototype partition.
pub struct KvmProtoPartition<'a> {
    vm: kvm::Partition,
    config: ProtoPartitionConfig<'a>,
    cpuid: CpuidLeafSet,
}

impl ProtoPartition for KvmProtoPartition<'_> {
    type Partition = KvmPartition;
    type Error = KvmError;
    type ProcessorBinder = KvmProcessorBinder;

    fn cpuid(&self, eax: u32, ecx: u32) -> [u32; 4] {
        self.cpuid.result(eax, ecx, &[0; 4])
    }

    fn max_physical_address_size(&self) -> u8 {
        max_physical_address_size_from_cpuid(&|eax, ecx| self.cpuid(eax, ecx))
    }

    fn build(
        mut self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        let mut cpuid = self.cpuid.into_leaves();
        cpuid.extend(config.cpuid);
        let cpuid = CpuidLeafSet::new(cpuid);

        let bsp_apic_id = self.config.processor_topology.vp_arch(VpIndex::BSP).apic_id;
        if bsp_apic_id != 0 {
            self.vm.set_bsp(bsp_apic_id)?;
        }

        let mut caps = virt::PartitionCapabilities::from_cpuid(
            self.config.processor_topology,
            &mut |function, index| cpuid.result(function, index, &[0; 4]),
        )
        .map_err(KvmError::Capabilities)?;

        caps.can_freeze_time = false;

        for vp_info in self.config.processor_topology.vps_arch() {
            self.vm.add_vp(vp_info.apic_id)?;
            let vp = self.vm.vp(vp_info.apic_id);
            if self.config.hv_config.is_some() {
                vp.enable_synic()?;

                // Set the VP index. Also, KVM incorrectly initializes SCONTROL
                // to 0. Set it to 1 on each processor.
                vp.set_msrs(&[
                    (
                        hvdef::HV_X64_MSR_VP_INDEX,
                        vp_info.base.vp_index.index().into(),
                    ),
                    (hvdef::HV_X64_MSR_SCONTROL, 1),
                ])?;
            }

            // Unlike the Microsoft hypervisor, KVM allows this MSR to be set and
            // defaults it to zero. Hard code the value here to the same as the
            // Microsoft hypervisor.
            vp.set_msrs(&[(
                x86defs::X86X_IA32_MSR_MISC_ENABLE,
                hv1_emulator::x86::MISC_ENABLE.into(),
            )])?;

            // Convert the CPUID entries and update the APIC ID in CPUID for
            // this VCPU.
            //
            // TODO: centralize this code, probably in the topology crate, for
            // use by other hypervisors.
            let cpuid_entries = cpuid
                .leaves()
                .iter()
                .map(|leaf| {
                    let mut entry = kvm::kvm_cpuid_entry2 {
                        function: leaf.function,
                        index: leaf.index.unwrap_or(0),
                        flags: if leaf.index.is_some() {
                            KVM_CPUID_FLAG_SIGNIFCANT_INDEX
                        } else {
                            0
                        },
                        eax: leaf.result[0],
                        ebx: leaf.result[1],
                        ecx: leaf.result[2],
                        edx: leaf.result[3],
                        padding: [0; 3],
                    };
                    match CpuidFunction(leaf.function) {
                        CpuidFunction::VersionAndFeatures => {
                            entry.ebx &= 0x00ffffff;
                            entry.ebx |= vp_info.apic_id << 24;
                        }
                        CpuidFunction::ExtendedTopologyEnumeration => {
                            entry.edx = vp_info.apic_id;
                        }
                        CpuidFunction::V2ExtendedTopologyEnumeration => {
                            entry.edx = vp_info.apic_id;
                        }
                        CpuidFunction::ProcessorTopologyDefinition => {
                            let eax =
                                x86defs::cpuid::ProcessorTopologyDefinitionEax::from(entry.eax);
                            entry.eax = eax.with_extended_apic_id(vp_info.apic_id).into();
                            let ebx =
                                x86defs::cpuid::ProcessorTopologyDefinitionEbx::from(entry.ebx);
                            entry.ebx = ebx
                                .with_compute_unit_id(
                                    (vp_info.apic_id
                                        % self.config.processor_topology.reserved_vps_per_socket()
                                        / (ebx.threads_per_compute_unit() as u32 + 1))
                                        as u8,
                                )
                                .into();
                            let ecx =
                                x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(entry.ecx);
                            entry.ecx = ecx
                                .with_node_id(
                                    (vp_info.apic_id
                                        / self.config.processor_topology.reserved_vps_per_socket())
                                        as u8,
                                )
                                .into();
                        }
                        _ => (),
                    }
                    entry
                })
                .collect::<Vec<_>>();

            vp.set_cpuid(&cpuid_entries)?;
        }

        let mut gsi_routing = GsiRouting::new();

        // Claim the IOAPIC routes.
        for gsi in 0..virt::irqcon::IRQ_LINES as u32 {
            gsi_routing.claim(gsi);
        }

        if self.config.hv_config.is_some() {
            // Setup GSI routes for signaling the synic.
            // TODO: set this up on every SINT, not just the VMBus one.
            for vp in self.config.processor_topology.vps() {
                let index = vp.vp_index.index();
                let gsi = VMBUS_BASE_GSI + index;
                gsi_routing.claim(gsi);
                gsi_routing.set(gsi, Some(kvm::RoutingEntry::HvSint { vp: index, sint: 2 }));
            }
        }

        kvm::init();

        gsi_routing.update_routes(&self.vm);

        let partition = KvmPartitionInner {
            kvm: self.vm,
            memory: Default::default(),
            hv1_enabled: self.config.hv_config.is_some(),
            gm: config.guest_memory.clone(),
            vps: self
                .config
                .processor_topology
                .vps_arch()
                .map(|vp_info| KvmVpInner {
                    needs_yield: NeedsYield::new(),
                    request_interrupt_window: false.into(),
                    eval: false.into(),
                    vp_info,
                    synic_message_queue: MessageQueues::new(),
                    siefp: Default::default(),
                })
                .collect(),
            gsi_routing: Mutex::new(gsi_routing),
            caps,
            cpuid,
        };

        let partition = KvmPartition {
            inner: Arc::new(partition),
        };

        let vps = self
            .config
            .processor_topology
            .vps()
            .map(|vp| KvmProcessorBinder {
                partition: partition.inner.clone(),
                vpindex: vp.vp_index,
                vmtime: self
                    .config
                    .vmtime
                    .access(format!("vp-{}", vp.vp_index.index())),
            })
            .collect::<Vec<_>>();

        if cfg!(debug_assertions) {
            (&partition).check_reset_all(&partition.inner.bsp().vp_info);
        }

        Ok((partition, vps))
    }
}

const VMBUS_BASE_GSI: u32 = virt::irqcon::IRQ_LINES as u32;

#[derive(Debug, Inspect)]
pub struct KvmVpInner {
    #[inspect(skip)]
    needs_yield: NeedsYield,
    request_interrupt_window: AtomicBool,
    eval: AtomicBool,
    vp_info: X86VpInfo,
    synic_message_queue: MessageQueues,
    #[inspect(hex, with = "|x| u64::from(*x.read())")]
    siefp: RwLock<HvSynicSimpSiefp>,
}

impl KvmVpInner {
    pub fn set_eval(&self, value: bool, ordering: Ordering) {
        self.eval.store(value, ordering);
    }

    pub fn vp_info(&self) -> &X86VpInfo {
        &self.vp_info
    }
}

impl ResetPartition for KvmPartition {
    type Error = KvmError;

    fn reset(&self) -> Result<(), Self::Error> {
        for vp in self.inner.vps() {
            self.inner
                .vp_state_access(vp.vp_info.base.vp_index)
                .reset_all(&vp.vp_info)
                .map_err(Box::new)?;
        }
        let mut this = self;
        this.reset_all(&self.inner.bsp().vp_info)
            .map_err(Box::new)?;
        Ok(())
    }
}

impl Partition for KvmPartition {
    fn supports_reset(&self) -> Option<&dyn ResetPartition<Error = Self::Error>> {
        Some(self)
    }

    fn doorbell_registration(
        self: &Arc<Self>,
        _minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        Some(self.clone())
    }

    fn msi_interrupt_target(self: &Arc<Self>, _vtl: Vtl) -> Option<Arc<dyn MsiInterruptTarget>> {
        Some(Arc::new(KvmMsiTarget(self.inner.clone())))
    }

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn request_yield(&self, vp_index: VpIndex) {
        tracing::trace!(vp_index = vp_index.index(), "request yield");
        let Some(vp) = self.inner.vp(vp_index) else {
            return;
        };
        if vp.needs_yield.request_yield() {
            self.inner.evaluate_vp(vp_index);
        }
    }

    fn request_msi(&self, _vtl: Vtl, request: MsiRequest) {
        self.inner.request_msi(request);
    }
}

impl virt::X86Partition for KvmPartition {
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting> {
        self.inner.clone()
    }

    fn pulse_lint(&self, vp_index: VpIndex, _vtl: Vtl, lint: u8) {
        let Some(vp) = self.inner.vp(vp_index) else {
            tracelimit::warn_ratelimited!(?vp_index, "pulse_lint for invalid vp_index");
            return;
        };
        if lint == 0 {
            tracing::trace!(vp_index = vp_index.index(), "request interrupt window");
            vp.request_interrupt_window.store(true, Ordering::Relaxed);
            self.inner.evaluate_vp(vp_index);
        } else {
            // TODO
            tracing::warn!("ignored lint1 pulse");
        }
    }
}

impl PartitionAccessState for KvmPartition {
    type StateAccess<'a> = &'a KvmPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);

        self
    }
}

impl Hv1 for KvmPartition {
    type Error = KvmError;
    type Device = virt::x86::apic_software_device::ApicSoftwareDevice;

    fn reference_time_source(&self) -> Option<ReferenceTimeSource> {
        self.inner
            .hv1_enabled
            .then(|| ReferenceTimeSource::from(self.inner.clone() as Arc<dyn GetReferenceTime>))
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        None
    }
}

impl GetReferenceTime for KvmPartitionInner {
    fn now(&self) -> ReferenceTimeResult {
        // Although we can query the reference time MSR for a VP, we are not
        // running in the context of a VP, and so such a query will hang if the
        // VP is running. Instead, query the KVM clock, which is the backing
        // clock for the reference time counter within KVM.
        //
        // This also gives us the system time, in some configurations.
        let clock = self.kvm.get_clock_ns().unwrap();
        ReferenceTimeResult {
            ref_time: clock.clock / 100,
            system_time: (clock.flags & kvm::KVM_CLOCK_REALTIME != 0)
                .then(|| jiff::Timestamp::from_nanosecond(clock.realtime as i128).unwrap()),
        }
    }
}

impl virt::BindProcessor for KvmProcessorBinder {
    type Processor<'a> = KvmProcessor<'a>;
    type Error = KvmError;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        // FUTURE: create the vcpu here to get better NUMA affinity.

        let inner = &self.partition.vps[self.vpindex.index() as usize];
        let kvm = self.partition.kvm.vp(inner.vp_info.apic_id);
        let mut vp = KvmProcessor {
            partition: &self.partition,
            inner,
            runner: kvm.runner(),
            kvm,
            vpindex: self.vpindex,
            guest_debug_db: [0; 4],
            scontrol: HvSynicScontrol::new().with_enabled(true),
            siefp: 0.into(),
            simp: 0.into(),
            vmtime: &mut self.vmtime,
        };

        // 1. Reset the APIC state to clear the directed EOI bit, which is
        //    set by KVM by default but our IO-APIC does not support.
        // 2. Enable x2apic if the partition needs it.
        // 3. Reset register state since KVM does not have the right
        //    architectural values.
        let vp_info = inner.vp_info;
        let mut state = vp.access_state(Vtl::Vtl0);
        state.set_registers(&virt::x86::vp::Registers::at_reset(
            &self.partition.caps,
            &vp_info,
        ))?;
        state.set_apic(&virt::x86::vp::Apic::at_reset(
            &self.partition.caps,
            &vp_info,
        ))?;

        if cfg!(debug_assertions) {
            vp.access_state(Vtl::Vtl0).check_reset_all(&vp_info);
        }

        Ok(vp)
    }
}

#[derive(InspectMut)]
pub struct KvmProcessor<'a> {
    #[inspect(skip)]
    partition: &'a KvmPartitionInner,
    #[inspect(flatten)]
    inner: &'a KvmVpInner,
    #[inspect(skip)]
    runner: kvm::VpRunner<'a>,
    #[inspect(skip)]
    kvm: kvm::Processor<'a>,
    vpindex: VpIndex,
    vmtime: &'a mut VmTimeAccess,
    #[inspect(iter_by_index)]
    guest_debug_db: [u64; 4],
    #[inspect(hex, with = "|&x| u64::from(x)")]
    scontrol: HvSynicScontrol,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    siefp: HvSynicSimpSiefp,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    simp: HvSynicSimpSiefp,
}

impl KvmProcessor<'_> {
    /// Delivers any pending PIC interrupt.
    ///
    /// The VP must be known to be stopped and must have an open interrupt
    /// window.
    fn deliver_pic_interrupt(&mut self, dev: &impl CpuIo) -> Result<(), KvmRunVpError> {
        if let Some(vector) = dev.acknowledge_pic_interrupt() {
            self.runner
                .inject_extint_interrupt(vector)
                .map_err(KvmRunVpError::ExtintInterrupt)?;
        }
        Ok(())
    }

    /// Tries to deliver any pending synic messages for a VP.
    fn try_deliver_synic_messages(&mut self) -> Option<VmTime> {
        if !self.scontrol.enabled() && self.simp.enabled() {
            return None;
        }
        self.inner
            .synic_message_queue
            .post_pending_messages(!0, |sint, message| {
                match self.write_sint_message(sint, message) {
                    Ok(true) => {
                        self.partition
                            .kvm
                            .irq_line(VMBUS_BASE_GSI + self.vpindex.index(), true)
                            .unwrap();
                        Ok(())
                    }
                    Ok(false) => Err(HvError::ObjectInUse),
                    Err(err) => {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            sint,
                            "failed to write message"
                        );
                        Err(HvError::OperationFailed)
                    }
                }
            });

        (self.inner.synic_message_queue.pending_sints() != 0).then(|| {
            // FUTURE: instead, poll on the resample eventfd for the
            // relevant SINTs, or get KVM to add a proper EOM exit
            self.vmtime.now().wrapping_add(Duration::from_millis(1))
        })
    }

    /// Writes a message to a synic message page. It is assumed there are no
    /// competing writers to the page (the VP should be stopped, so neither
    /// the guest nor KVM should be writing to the page), so no special
    /// synchronization is required.
    fn write_sint_message(&mut self, sint: u8, msg: &HvMessage) -> Result<bool, GuestMemoryError> {
        let simp = self.simp.base_gpn() * HV_PAGE_SIZE + sint as u64 * 256;
        let typ: u32 = self.partition.gm.read_plain(simp)?;
        if typ != 0 {
            self.partition.gm.write_at(simp + 5, &[1u8])?;
            let typ: u32 = self.partition.gm.read_plain(simp)?;
            if typ != 0 {
                return Ok(false);
            }
        }
        self.partition.gm.write_at(simp + 4, &msg.as_bytes()[4..])?;
        self.partition.gm.write_plain(simp, &msg.header.typ)?;
        Ok(true)
    }
}

struct KvmMsi {
    address_lo: u32,
    address_hi: u32,
    data: u32,
}

impl KvmMsi {
    fn new(request: MsiRequest) -> Self {
        let request_address = MsiAddress::from(request.address as u32);
        let request_data = MsiData::from(request.data);

        // Although architecturally the destination mode bit is only supposed to
        // be considered when the redirection hint bit is set, KVM always gets
        // the destination mode from this bit instead of from the MSI data.
        let address_lo = MsiAddress::new()
            .with_address(x86defs::msi::MSI_ADDRESS)
            .with_destination(request_address.destination())
            .with_destination_mode_logical(request_address.destination_mode_logical())
            .with_redirection_hint(request_data.delivery_mode() == DeliveryMode::LOWEST_PRIORITY.0)
            .into();

        // High bits of the destination go into the high bits of the address.
        let address_hi = (request_address.virt_destination() & !0xff).into();
        let data = MsiData::new()
            .with_delivery_mode(request_data.delivery_mode())
            .with_assert(request_data.assert())
            .with_destination_mode_logical(request_data.destination_mode_logical())
            .with_trigger_mode_level(request_data.trigger_mode_level())
            .with_vector(request_data.vector())
            .into();

        Self {
            address_lo,
            address_hi,
            data,
        }
    }
}

impl KvmPartitionInner {
    fn request_msi(&self, request: MsiRequest) {
        let KvmMsi {
            address_lo,
            address_hi,
            data,
        } = KvmMsi::new(request);
        if let Err(err) = self.kvm.request_msi(&kvm::kvm_msi {
            address_lo,
            address_hi,
            data,
            flags: 0,
            devid: 0,
            pad: [0; 12],
        }) {
            tracelimit::warn_ratelimited!(
                address = request.address,
                data = request.data,
                error = &err as &dyn std::error::Error,
                "failed to request MSI"
            );
        }
    }
}

impl IoApicRouting for KvmPartitionInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        let entry = request.map(|request| {
            let KvmMsi {
                address_lo,
                address_hi,
                data,
            } = KvmMsi::new(request);
            kvm::RoutingEntry::Msi {
                address_lo,
                address_hi,
                data,
            }
        });
        let mut gsi_routing = self.gsi_routing.lock();
        if gsi_routing.set(irq as u32, entry) {
            gsi_routing.update_routes(&self.kvm);
        }
    }

    fn assert_irq(&self, irq: u8) {
        if let Err(err) = self.kvm.irq_line(irq as u32, true) {
            tracing::error!(
                irq,
                error = &err as &dyn std::error::Error,
                "failed to assert irq"
            );
        }
    }
}

struct KvmDoorbellEntry {
    partition: Weak<KvmPartitionInner>,
    event: Event,
    guest_address: u64,
    value: u64,
    length: u32,
    flags: u32,
}

impl KvmDoorbellEntry {
    pub fn new(
        partition: &Arc<KvmPartitionInner>,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<KvmDoorbellEntry> {
        let flags = if value.is_some() {
            1 << kvm_ioeventfd_flag_nr_datamatch
        } else {
            0
        };
        let value = value.unwrap_or(0);
        let length = length.unwrap_or(0);

        // Dup the fd since it's needed to deassign the ioeventfd later.
        let event = fd.clone();

        if let Err(err) = partition.kvm.ioeventfd(
            value,
            guest_address,
            length,
            event.as_fd().as_raw_fd(),
            flags,
        ) {
            tracing::warn!(
                guest_address,
                error = &err as &dyn std::error::Error,
                "Failed to register doorbell",
            );
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Failed to register doorbell",
            ));
        }

        Ok(Self {
            partition: Arc::downgrade(partition),
            guest_address,
            value,
            length,
            flags,
            event,
        })
    }
}

impl Drop for KvmDoorbellEntry {
    fn drop(&mut self) {
        if let Some(partition) = self.partition.upgrade() {
            let flags: u32 = self.flags | (1 << kvm_ioeventfd_flag_nr_deassign);
            if let Err(err) = partition.kvm.ioeventfd(
                self.value,
                self.guest_address,
                self.length,
                self.event.as_fd().as_raw_fd(),
                flags,
            ) {
                tracing::warn!(
                    guest_address = self.guest_address,
                    error = &err as &dyn std::error::Error,
                    "Failed to unregister doorbell",
                );
            }
        }
    }
}

impl DoorbellRegistration for KvmPartition {
    fn register_doorbell(
        &self,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        fd: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(KvmDoorbellEntry::new(
            &self.inner,
            guest_address,
            value,
            length,
            fd,
        )?))
    }
}

struct KvmHypercallExit<'a, T> {
    bus: &'a T,
    registers: KvmHypercallRegisters,
}

struct KvmHypercallRegisters {
    input: u64,
    params: [u64; 2],
    result: u64,
}

impl<T: CpuIo> KvmHypercallExit<'_, T> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

impl<'a, T: CpuIo> hv1_hypercall::AsHandler<KvmHypercallExit<'a, T>>
    for &mut KvmHypercallExit<'a, T>
{
    fn as_handler(&mut self) -> &mut KvmHypercallExit<'a, T> {
        self
    }
}

impl<T> hv1_hypercall::HypercallIo for KvmHypercallExit<'_, T> {
    fn advance_ip(&mut self) {
        // KVM automatically does this.
    }

    fn retry(&mut self, _control: u64) {
        unimplemented!("KVM cannot retry hypercalls");
    }

    fn control(&mut self) -> u64 {
        // KVM automatically converts HvSignalEvent to a fast hypercall,
        // but it does not update the control register accordingly.
        let mut control = Control::from(self.registers.input);
        if control.code() == HypercallCode::HvCallSignalEvent.0 {
            control.set_fast(true);
        }
        control.into()
    }

    fn input_gpa(&mut self) -> u64 {
        self.registers.params[0]
    }

    fn output_gpa(&mut self) -> u64 {
        self.registers.params[1]
    }

    fn fast_register_pair_count(&mut self) -> usize {
        1
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        false
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], _output_register_pairs: usize) -> usize {
        self.fast_regs(0, buf);
        0
    }

    fn fast_output(&mut self, _starting_pair_index: usize, _buf: &[[u64; 2]]) {}

    fn vtl_input(&mut self) -> u64 {
        unimplemented!()
    }

    fn set_result(&mut self, n: u64) {
        self.registers.result = n;
    }

    fn fast_regs(&mut self, _starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        if let [b, ..] = buf {
            *b = self.registers.params;
        }
    }
}

impl<T: CpuIo> hv1_hypercall::PostMessage for KvmHypercallExit<'_, T> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.bus
            .post_synic_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl<T: CpuIo> hv1_hypercall::SignalEvent for KvmHypercallExit<'_, T> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.bus.signal_synic_event(Vtl::Vtl0, connection_id, flag)
    }
}

impl Processor for KvmProcessor<'_> {
    type StateAccess<'a>
        = KvmVpStateAccess<'a>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <KvmVpStateAccess<'_> as AccessVpState>::Error> {
        let mut control = 0;
        let mut db = [0; 4];
        let mut dr7 = 0;
        if let Some(state) = state {
            control |= kvm::KVM_GUESTDBG_ENABLE;
            if state.single_step {
                control |= kvm::KVM_GUESTDBG_SINGLESTEP;
            }
            for (i, bp) in state.breakpoints.iter().enumerate() {
                if let Some(bp) = bp {
                    control |= kvm::KVM_GUESTDBG_USE_HW_BP;
                    db[i] = bp.address;
                    dr7 |= bp.dr7_bits(i);
                }
            }
        }
        self.kvm.set_guest_debug(control, db, dr7)?;
        // Remember the debug registers to retrieve the address later.
        self.guest_debug_db = db;
        Ok(())
    }

    async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        loop {
            self.inner.needs_yield.maybe_yield().await;
            stop.check()?;

            if self.partition.hv1_enabled {
                // Deliver pending synic messages now, while KVM is not
                // accessing the message page.
                if let Some(next) = self.try_deliver_synic_messages() {
                    self.vmtime.set_timeout_if_before(next)
                } else {
                    self.vmtime.cancel_timeout();
                }
            }

            // Check for pending PIC interrupts.
            //
            // Check and clear this with a relaxed ordering since `evaluate_vp`
            // (called when this is set) will force the VP to exit, causing us
            // to re-check.
            if self.inner.request_interrupt_window.load(Ordering::Relaxed) {
                self.inner
                    .request_interrupt_window
                    .store(false, Ordering::Relaxed);
                if self.runner.check_or_request_interrupt_window() {
                    self.deliver_pic_interrupt(dev)
                        .map_err(|e| dev.fatal_error(e.into()))?;
                }
            }

            // Arm the timer. If it has expired, then loop around to scan for
            // synic messages again.
            if poll_fn(|cx| Poll::Ready(self.vmtime.poll_timeout(cx).is_ready())).await {
                continue;
            }

            // Run the VP and handle exits until `evaluate_vp` is called or the
            // thread is otherwise interrupted.
            //
            // Don't break out of the loop while there is a pending exit so that
            // the register state is up-to-date for save.
            let mut pending_exit = false;
            loop {
                let exit = if self.inner.eval.load(Ordering::Relaxed) || stop.check().is_err() {
                    // Break out of the loop as soon as there is no pending exit.
                    if !pending_exit {
                        self.inner.eval.store(false, Ordering::Relaxed);
                        break;
                    }
                    // Complete the current exit.
                    self.runner.complete_exit()
                } else {
                    // Run the VP.
                    self.runner.run()
                };

                let exit = exit.map_err(|err| dev.fatal_error(KvmRunVpError::Run(err).into()))?;
                pending_exit = true;
                match exit {
                    kvm::Exit::Interrupted => {
                        tracing::trace!("interrupted");
                        pending_exit = false;
                    }
                    kvm::Exit::InterruptWindow => {
                        self.deliver_pic_interrupt(dev)
                            .map_err(|e| dev.fatal_error(e.into()))?;
                    }
                    kvm::Exit::IoIn { port, data, size } => {
                        for data in data.chunks_mut(size as usize) {
                            dev.read_io(self.vpindex, port, data).await;
                        }
                    }
                    kvm::Exit::IoOut { port, data, size } => {
                        for data in data.chunks(size as usize) {
                            dev.write_io(self.vpindex, port, data).await;
                        }
                    }
                    kvm::Exit::MmioWrite { address, data } => {
                        dev.write_mmio(self.vpindex, address, data).await
                    }
                    kvm::Exit::MmioRead { address, data } => {
                        dev.read_mmio(self.vpindex, address, data).await
                    }
                    kvm::Exit::MsrRead { index, data, error } => {
                        if MYSTERY_MSRS.contains(&index) {
                            tracelimit::warn_ratelimited!(index, "stubbed out mystery MSR read");
                            *data = 0;
                        } else {
                            tracelimit::error_ratelimited!(index, "unrecognized msr read");
                            *error = 1;
                        }
                    }
                    kvm::Exit::MsrWrite { index, data, error } => {
                        if MYSTERY_MSRS.contains(&index) {
                            tracelimit::warn_ratelimited!(index, "stubbed out mystery MSR write");
                        } else {
                            tracelimit::error_ratelimited!(index, data, "unrecognized msr write");
                            *error = 1;
                        }
                    }
                    kvm::Exit::Shutdown => {
                        return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                    }
                    kvm::Exit::SynicUpdate {
                        msr: _msr,
                        control,
                        siefp,
                        simp,
                    } => {
                        self.scontrol = control.into();
                        self.siefp = siefp.into();
                        self.simp = simp.into();
                        *self.inner.siefp.write() = if self.scontrol.enabled() {
                            siefp.into()
                        } else {
                            0.into()
                        };
                    }
                    kvm::Exit::HvHypercall {
                        input,
                        result,
                        params,
                    } => {
                        // N.B. this can only be SIGNAL_EVENT or POST_MESSAGE.
                        let mut handler = KvmHypercallExit {
                            bus: dev,
                            registers: KvmHypercallRegisters {
                                input,
                                params,
                                result: 0,
                            },
                        };
                        KvmHypercallExit::DISPATCHER.dispatch(&self.partition.gm, &mut handler);
                        *result = handler.registers.result;
                    }
                    kvm::Exit::Debug {
                        exception: _,
                        pc: _,
                        dr6,
                        dr7,
                    } => {
                        if dr6 & x86defs::DR6_BREAKPOINT_MASK != 0 {
                            let i = dr6.trailing_zeros() as usize;
                            let bp = HardwareBreakpoint::from_dr7(dr7, self.guest_debug_db[i], i);
                            return Err(VpHaltReason::HwBreak(bp));
                        } else if dr6 & x86defs::DR6_SINGLE_STEP != 0 {
                            return Err(VpHaltReason::SingleStep);
                        } else {
                            tracing::warn!(dr6, "debug exit with unknown dr6 condition");
                        }
                    }
                    kvm::Exit::Eoi { irq } => {
                        dev.handle_eoi(irq.into());
                    }
                    kvm::Exit::InternalError { error, .. } => {
                        return Err(dev.fatal_error(KvmRunVpError::InternalError(error).into()));
                    }
                    kvm::Exit::EmulationFailure { instruction_bytes } => {
                        return Err(dev.fatal_error(
                            EmulationError {
                                instruction_bytes: instruction_bytes.to_vec(),
                            }
                            .into(),
                        ));
                    }
                    kvm::Exit::FailEntry {
                        hardware_entry_failure_reason,
                    } => {
                        tracing::error!(hardware_entry_failure_reason, "VP entry failed");
                        return Err(dev.fatal_error(KvmRunVpError::InvalidVpState.into()));
                    }
                }
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.partition.vp_state_access(self.vpindex)
    }
}

impl virt::Synic for KvmPartition {
    fn post_message(&self, _vtl: Vtl, vp_index: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        let Some(vp) = self.inner.vp(vp_index) else {
            tracelimit::warn_ratelimited!(?vp_index, "post_message for invalid vp_index");
            return;
        };

        let wake = vp
            .synic_message_queue
            .enqueue_message(sint, &HvMessage::new(HvMessageType(typ), 0, payload));

        if wake {
            self.inner.evaluate_vp(vp_index);
        }
    }

    fn new_guest_event_port(
        &self,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(KvmGuestEventPort {
            partition: Arc::downgrade(&self.inner),
            gm: self.inner.gm.clone(),
            params: Arc::new(Mutex::new(KvmEventPortParams {
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

#[derive(Debug, Error)]
#[error("KVM emulation failure: instruction {instruction_bytes:02x?}")]
struct EmulationError {
    instruction_bytes: Vec<u8>,
}

/// `GuestEventPort` implementation for KVM partitions.
#[derive(Debug, Clone)]
struct KvmGuestEventPort {
    partition: Weak<KvmPartitionInner>,
    gm: GuestMemory,
    params: Arc<Mutex<KvmEventPortParams>>,
}

#[derive(Debug, Copy, Clone)]
struct KvmEventPortParams {
    vp: VpIndex,
    sint: u8,
    flag: u16,
}

impl GuestEventPort for KvmGuestEventPort {
    fn interrupt(&self) -> Interrupt {
        let this = self.clone();
        Interrupt::from_fn(move || {
            let KvmEventPortParams {
                vp: vp_index,
                sint,
                flag,
            } = *this.params.lock();
            let Some(partition) = this.partition.upgrade() else {
                return;
            };
            let Some(vp) = partition.vp(vp_index) else {
                tracelimit::warn_ratelimited!(
                    ?vp_index,
                    sint,
                    flag,
                    "signal event for invalid vp_index"
                );
                return;
            };
            let siefp = vp.siefp.read();
            if !siefp.enabled() {
                return;
            }
            let byte_gpa = siefp.base_gpn() * HV_PAGE_SIZE + sint as u64 * 256 + flag as u64 / 8;
            let mut byte = 0;
            let mask = 1 << (flag % 8);
            while byte & mask == 0 {
                match this.gm.compare_exchange(byte_gpa, byte, byte | mask) {
                    Ok(Ok(_)) => {
                        drop(siefp);
                        partition
                            .kvm
                            .irq_line(VMBUS_BASE_GSI + vp_index.index(), true)
                            .unwrap();

                        break;
                    }
                    Ok(Err(b)) => byte = b,
                    Err(err) => {
                        tracelimit::warn_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "failed to write event flag to guest memory"
                        );
                        break;
                    }
                }
            }
        })
    }

    fn set_target_vp(&mut self, vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
        self.params.lock().vp = VpIndex::new(vp);
        Ok(())
    }
}

#[derive(Debug)]
struct GsiMsi {
    gsi: gsi::GsiRoute,
}

struct KvmMsiTarget(Arc<KvmPartitionInner>);

impl MsiInterruptTarget for KvmMsiTarget {
    fn new_interrupt(&self) -> Box<dyn MsiControl> {
        let event = Event::new();
        let interrupt = self.0.new_route(Some(event)).expect("BUGBUG");
        Box::new(GsiMsi { gsi: interrupt })
    }
}

impl MsiControl for GsiMsi {
    fn enable(&mut self, address: u64, data: u32) {
        let request = MsiRequest { address, data };
        let KvmMsi {
            address_lo,
            address_hi,
            data,
        } = KvmMsi::new(request);

        self.gsi.enable(kvm::RoutingEntry::Msi {
            address_lo,
            address_hi,
            data,
        });
    }

    fn disable(&mut self) {
        self.gsi.disable();
    }

    fn signal(&mut self, _address: u64, _data: u32) {
        self.gsi.irqfd_event().unwrap().signal()
    }
}
