// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements support for KVM on x86_64.

#![cfg(all(target_os = "linux", guest_arch = "x86_64"))]

mod regs;
pub(crate) mod snp;
mod vm_state;
mod vp_state;

pub(crate) use vp_state::seg_reg;
pub(crate) use vp_state::table_reg;

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::KvmProcessorBinder;
use crate::KvmRunVpError;
use crate::SnpLaunchState;
use crate::gsi::GsiRouting;
use crate::gsi::KvmIrqFdState;
use crate::gsi::MsiRouteBuilder;
use crate::memory::KvmMemoryBackingMode;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hv1_emulator::message_queues::MessageQueues;
use hv1_emulator::pages::OverlayPage;
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
use pci_core::msi::SignalMsi;
use std::convert::Infallible;
use std::fs::OpenOptions;
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
use vm_topology::processor::ProcessorTopology;
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
pub struct Kvm {
    kvm: kvm::Kvm,
}

impl Kvm {
    /// Creates a new KVM hypervisor instance.
    pub fn new() -> Result<Self, KvmError> {
        Ok(Self {
            kvm: kvm::Kvm::new()?,
        })
    }

    /// Creates a KVM hypervisor instance from a pre-opened `/dev/kvm` fd.
    pub fn from_kvm(file: std::fs::File) -> Result<Self, KvmError> {
        let kvm = kvm::Kvm::from(file);
        Ok(Self { kvm })
    }
}

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

    fn platform_info(&self) -> virt::PlatformInfo {
        virt::PlatformInfo {}
    }

    fn recognizes_nested_virt(&self) -> bool {
        true
    }

    fn new_partition<'a>(
        &mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        match config.isolation {
            virt::IsolationType::None => {}
            virt::IsolationType::Snp => {
                if config.hv_config.is_some() {
                    return Err(KvmError::UnsupportedIsolationConfiguration(
                        "SNP does not support Hyper-V enlightenments or VTL2",
                    ));
                }
            }
            virt::IsolationType::Vbs | virt::IsolationType::Tdx | virt::IsolationType::Cca => {
                return Err(KvmError::IsolationNotSupported);
            }
        }

        let nested_virt = config.nested_virt;
        let supported_cpuid = self.kvm.supported_cpuid()?;

        // KVM's in-kernel LAPIC only exposes the CMCI LVT register (APIC
        // offset 0x2F0) when the guest's IA32_MCG_CAP advertises MCG_CMCI_P.
        // Query which MCE capability bits this host allows us to set so that
        // bind() can advertise CMCI to the guest where supported (Intel).
        let supported_mce_cap = self.kvm.supported_mce_cap()?;

        // Determine the CPU vendor from CPUID leaf 0.
        let vendor = supported_cpuid
            .iter()
            .find(|e| e.function == CpuidFunction::VendorAndMaxFunction.0)
            .map(|e| x86defs::cpuid::Vendor::from_ebx_ecx_edx(e.ebx, e.ecx, e.edx))
            .unwrap_or(x86defs::cpuid::Vendor([0; 12]));

        if !vendor.is_intel_compatible() && !vendor.is_amd_compatible() {
            return Err(KvmError::UnsupportedCpuVendor);
        }

        let mut cpuid_entries = supported_cpuid
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

        // When nested virt is disabled, strip the virtualization
        // CPUID bit for the host's vendor.
        if !nested_virt {
            let (function, ecx_mask) = if vendor.is_intel_compatible() {
                (
                    CpuidFunction::VersionAndFeatures.0,
                    x86defs::cpuid::VersionAndFeaturesEcx::new()
                        .with_vmx(true)
                        .into(),
                )
            } else {
                (
                    CpuidFunction::ExtendedVersionAndFeatures.0,
                    x86defs::cpuid::ExtendedVersionAndFeaturesEcx::new()
                        .with_svm(true)
                        .into(),
                )
            };
            cpuid_entries.push(CpuidLeaf::new(function, [0, 0, 0, 0]).masked([0, 0, ecx_mask, 0]));
        }

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

            // Query KVM's supported Hyper-V CPUID leaves to find the
            // nested virtualization features leaf (0x4000000A), but only
            // expose it when nested virtualization is enabled.
            let kvm_hv_cpuid = self.kvm.supported_hv_cpuid()?;
            let nested_leaf = if nested_virt {
                kvm_hv_cpuid
                    .iter()
                    .find(|e| e.function == HV_CPUID_FUNCTION_MS_HV_NESTED_FEATURES)
            } else {
                None
            };

            let max_function = if nested_leaf.is_some() {
                HV_CPUID_FUNCTION_MS_HV_NESTED_FEATURES
            } else {
                HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS
            };

            let hv_cpuid = &[
                CpuidLeaf::new(
                    HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
                    [
                        max_function,
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

            // Pass through KVM's nested virtualization features so that
            // a guest hypervisor (e.g., Hyper-V) can launch.
            if let Some(leaf) = nested_leaf {
                cpuid_entries.push(CpuidLeaf::new(
                    HV_CPUID_FUNCTION_MS_HV_NESTED_FEATURES,
                    [leaf.eax, leaf.ebx, leaf.ecx, leaf.edx],
                ));
            }
        }

        let cpuid_entries = CpuidLeafSet::new(cpuid_entries);

        // If nested virt was requested, verify the host actually
        // supports it (VMX on Intel, SVM on AMD).
        if nested_virt {
            let supported = if vendor.is_intel_compatible() {
                x86defs::cpuid::VersionAndFeaturesEcx::from(
                    cpuid_entries.result(CpuidFunction::VersionAndFeatures.0, 0, &[0; 4])[2],
                )
                .vmx()
            } else {
                x86defs::cpuid::ExtendedVersionAndFeaturesEcx::from(
                    cpuid_entries.result(CpuidFunction::ExtendedVersionAndFeatures.0, 0, &[0; 4])
                        [2],
                )
                .svm()
            };
            if !supported {
                return Err(KvmError::NestedVirtUnsupported);
            }
        }

        let sev = match config.isolation {
            virt::IsolationType::Snp => Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/sev")
                    .map_err(crate::snp::SnpError::OpenSev)?,
            ),
            virt::IsolationType::None => None,
            virt::IsolationType::Vbs | virt::IsolationType::Tdx | virt::IsolationType::Cca => {
                unreachable!()
            }
        };

        let vm = match config.isolation {
            virt::IsolationType::None => self.kvm.new_vm(kvm::VmType::Default)?,
            virt::IsolationType::Snp => {
                let vm = self.kvm.new_vm(kvm::VmType::Snp)?;
                vm.enable_hypercall_exits(1 << kvm::KVM_HC_MAP_GPA_RANGE_UAPI)?;
                vm
            }
            virt::IsolationType::Vbs | virt::IsolationType::Tdx | virt::IsolationType::Cca => {
                unreachable!()
            }
        };
        if let Some(sev) = &sev {
            vm.sev_snp_init(sev.as_fd())?;
        }
        vm.enable_split_irqchip(virt::irqcon::IRQ_LINES as u32)?;
        vm.enable_x2apic_api()?;
        vm.enable_unknown_msr_exits()?;

        Ok(KvmProtoPartition {
            vm,
            sev,
            config,
            cpuid: cpuid_entries,
            nested_virt,
            supported_mce_cap,
        })
    }
}

/// A prototype partition.
pub struct KvmProtoPartition<'a> {
    vm: kvm::Partition,
    sev: Option<std::fs::File>,
    config: ProtoPartitionConfig<'a>,
    cpuid: CpuidLeafSet,
    nested_virt: bool,
    /// MCE capability bits (`IA32_MCG_CAP`) the host allows setting, from
    /// `KVM_X86_GET_MCE_CAP_SUPPORTED`.
    supported_mce_cap: u64,
}

impl ProtoPartition for KvmProtoPartition<'_> {
    type Partition = KvmPartition;
    type Error = KvmError;
    type ProcessorBinder = KvmProcessorBinder;

    fn max_physical_address_size(&self) -> u8 {
        max_physical_address_size_from_cpuid(&|eax, ecx| self.cpuid.result(eax, ecx, &[0; 4]))
    }

    fn build(
        mut self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        // Build topology leaves using the base cpuid before consuming it.
        let mut topology_leaves = Vec::new();
        virt::x86::topology::topology_cpuid(
            self.config.processor_topology,
            &|eax, ecx| self.cpuid.result(eax, ecx, &[0; 4]),
            &mut topology_leaves,
        )
        .map_err(KvmError::TopologyCpuid)?;

        // Work around a KVM bug where PSFD is advertised in guest CPUID
        // but the SPEC_CTRL MSR is not accessible. Check the KVM-reported
        // CPUID (before user overrides) since that determines what KVM
        // will allow.
        let psfd_fixup = strip_psfd_leaf(&self.cpuid);

        let mut cpuid = self.cpuid.into_leaves();
        cpuid.extend(config.cpuid);
        cpuid.extend(topology_leaves);
        cpuid.extend(psfd_fixup);
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
        caps.nested_virt = self.nested_virt;

        // Create all VCPUs now so that they are assigned dense, sequential
        // vcpu_idx values (KVM assigns vcpu_idx in creation order).  KVM's
        // Hyper-V enlightenment code has a fast O(1) VP-index-to-vcpu lookup
        // that only works when vp_index == vcpu_idx; if the indices diverge
        // (e.g. because VCPUs were created in arbitrary order from bind()),
        // every synic interrupt delivery and VP-set operation falls back to
        // an O(n) linear scan.  Per-VP initialization (CPUID, MSRs, synic)
        // is deferred to bind().
        for vp_info in self.config.processor_topology.vps_arch() {
            self.vm.add_vp(vp_info.apic_id)?;
        }

        // The vps exist, so the guest timestamp counter now has an origin - and it is not
        // the one the partition reference clock got when the vm was created, several
        // hundred microseconds earlier. Close that before anything can observe it. This
        // has to come after the loop rather than after the bsp alone, or the loop's own
        // duration (one vcpu creation per vp) stays in the answer.
        //
        // Done for every partition, not only the ones that enlighten the guest: the two
        // origins disagreeing is a property of how the partition is built, and a guest
        // that calibrates a counter against a clock is entitled to find them consistent
        // whether or not it does so through the hyper-v interfaces.
        //
        // The per-vp `Tsc` element that guest state initialization writes later is a plain
        // zero and cannot undo this: the kernel substitutes the partition's current offset
        // for a host write of zero, and that is now the offset written here.
        let alignment = align_guest_tsc_to_reference_clock(
            &self.vm,
            bsp_apic_id,
            self.config
                .processor_topology
                .vps_arch()
                .map(|vp_info| vp_info.apic_id),
        )?;
        tracing::info!(
            reference_time = self.vm.get_clock()?.clock_ns / 100,
            guest_tsc_before = alignment.map(|a| a.before),
            guest_tsc_after = alignment.map(|a| a.after),
            tsc_pairing = alignment.map(|a| a.pairing),
            tsc_pairing_error_ns = alignment.map(|a| a.pairing_error_ns),
            tsc_requested_lead_ns = alignment.map(|a| a.requested_lead_ns),
            tsc_achieved_lead_ns = alignment.map(|a| a.achieved_lead_ns),
            "partition created"
        );

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

        let ram_ranges: Vec<_> = config
            .mem_layout
            .ram()
            .iter()
            .map(|range| range.range)
            .chain(config.mem_layout.vtl2_range())
            .collect();
        let memory_backing_mode = match self.config.isolation {
            virt::IsolationType::None => KvmMemoryBackingMode::Userspace,
            virt::IsolationType::Snp => {
                KvmMemoryBackingMode::guest_memfd(&self.vm, ram_ranges.iter().copied(), true)?
            }
            virt::IsolationType::Vbs | virt::IsolationType::Tdx | virt::IsolationType::Cca => {
                unreachable!()
            }
        };

        let partition = Arc::new(KvmPartitionInner {
            kvm: self.vm,
            sev: self.sev,
            snp_launch_state: Mutex::new(SnpLaunchState::NotStarted),
            memory: Default::default(),
            memory_backing_mode,
            ram_ranges,
            hv1_enabled: self.config.hv_config.is_some(),
            gm: config.guest_memory.clone(),
            bsp_cpuid: kvm_cpuid_entries(
                &cpuid,
                &self.config.processor_topology.vp_arch(VpIndex::BSP),
                self.config.processor_topology,
            ),
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
            reserved_vps_per_socket: self.config.processor_topology.reserved_vps_per_socket(),
            mce_cmci_supported: x86defs::McgCap::from(self.supported_mce_cap).cmci_p(),
            synic_ports: Default::default(),
        });

        let partition = KvmPartition {
            synic_ports: Arc::new(virt::synic::SynicPorts::new(partition.clone())),
            irqfd_state: Arc::new(KvmIrqFdState::new(partition.clone())),
            inner: partition,
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

        fn kvm_cpuid_entries(
            cpuid: &CpuidLeafSet,
            vp_info: &X86VpInfo,
            processor_topology: &ProcessorTopology,
        ) -> Vec<kvm::kvm_cpuid_entry2> {
            cpuid
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
                                    (vp_info.apic_id % processor_topology.reserved_vps_per_socket()
                                        / (ebx.threads_per_compute_unit() as u32 + 1))
                                        as u8,
                                )
                                .into();
                            let ecx =
                                x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(entry.ecx);
                            entry.ecx = ecx
                                .with_node_id(
                                    (vp_info.apic_id / processor_topology.reserved_vps_per_socket())
                                        as u8,
                                )
                                .into();
                        }
                        _ => (),
                    }
                    entry
                })
                .collect()
        }

        Ok((partition, vps))
    }
}

/// KVM's `guest_has_spec_ctrl_msr()` decides whether a guest may access
/// the SPEC_CTRL MSR by checking for IBRS, STIBP, and SSBD in CPUID.
/// However, KVM also passes through AMD PSFD without including it in that
/// check. PSFD is architecturally controlled via the SPEC_CTRL MSR, so a
/// guest that sees PSFD and infers SPEC_CTRL MSR support (as Hyper-V
/// does) will #GP when writing the MSR.
///
/// Returns a leaf that strips PSFD when it should not be advertised.
fn strip_psfd_leaf(cpuid: &CpuidLeafSet) -> Option<CpuidLeaf> {
    use x86defs::cpuid::ExtendedAddressSpaceSizesEbx;
    use x86defs::cpuid::ExtendedFeatureSubleaf0Edx;

    let leaf7 = cpuid.result(CpuidFunction::ExtendedFeatures.0, 0, &[0; 4]);
    let leaf80000008 = cpuid.result(CpuidFunction::ExtendedAddressSpaceSizes.0, 0, &[0; 4]);

    let edx = ExtendedFeatureSubleaf0Edx::from(leaf7[3]);
    let ebx = ExtendedAddressSpaceSizesEbx::from(leaf80000008[1]);

    // Mirror KVM's guest_has_spec_ctrl_msr() check.
    let has_spec_ctrl_msr = edx.ibrs() || ebx.ibrs() || ebx.stibp() || ebx.ssbd();
    if !has_spec_ctrl_msr && ebx.psfd() {
        let psfd_mask = ExtendedAddressSpaceSizesEbx::new().with_psfd(true);
        Some(
            CpuidLeaf::new(CpuidFunction::ExtendedAddressSpaceSizes.0, [0, 0, 0, 0]).masked([
                0,
                u32::from(psfd_mask),
                0,
                0,
            ]),
        )
    } else {
        None
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
        let mut this = self;
        // Sampled before the reset so the line below carries BOTH halves of the
        // partition's clock. A reset that moves only one of them is what wedges a guest
        // that calibrates one against the other, and the pair is the only reading that
        // distinguishes that from a healthy reset.
        let reference_time_before = self.inner.now().ref_time;
        this.reset_all(&self.inner.bsp().vp_info)
            .map_err(Box::new)?;
        // `reset_all` has just returned the reference clock to zero; put the counter back
        // on that same origin. Both halves of the partition clock restart together, which
        // is what recreating the partition would do.
        let tsc = align_guest_tsc_to_reference_clock(
            &self.inner.kvm,
            self.inner.bsp().vp_info.apic_id,
            self.inner.vps.iter().map(|vp| vp.vp_info.apic_id),
        )?;
        tracing::info!(
            reference_time_before,
            reference_time_after = self.inner.now().ref_time,
            guest_tsc_before = tsc.map(|t| t.before),
            guest_tsc_after = tsc.map(|t| t.after),
            tsc_pairing = tsc.map(|t| t.pairing),
            tsc_pairing_error_ns = tsc.map(|t| t.pairing_error_ns),
            tsc_requested_lead_ns = tsc.map(|t| t.requested_lead_ns),
            tsc_achieved_lead_ns = tsc.map(|t| t.achieved_lead_ns),
            "machine reset"
        );
        Ok(())
    }
}

/// The FLOOR under how far AHEAD of the partition reference clock the guest timestamp
/// counter is left, in nanoseconds.
///
/// Not zero, and the sign is the point rather than a detail. A guest hypervisor computes a
/// synthetic timer deadline from ITS OWN counter and KVM tests that deadline against the
/// reference clock (`stimer_start`, arch/x86/kvm/hyperv.c: `time_now =
/// get_time_ref_counter(...)`, then `if (time_now >= stimer->count)` takes the
/// fire-immediately branch). So the guest's deadline is `counter_now + horizon` while the
/// test is against `clock_now`, and the two sides are only comparable to the extent the
/// counter and the clock share an origin:
///
/// * counter BEHIND the clock by D - the guest's `counter_now` reads D low, so every
///   deadline lands D early and every arm with a horizon shorter than D reads as already
///   past. That is the storm. Measured on this host with the counter 22.08 us behind:
///   3589 past-dated arms a second, clustered at a past-lag of 25.4 us, exactly D plus the
///   arm's own delivery latency.
/// * counter AHEAD of the clock by D - every deadline lands D late, so even a
///   zero-horizon arm is D in the future and the immediate branch is not reached at all.
///   Measured with the counter 21 to 38 us ahead: under 0.05 past-dated arms a second.
///
/// A long horizon is harmless and a short one is the failure, which is the right
/// principle; it is the mapping to the sign that is counter-intuitive. A counter that
/// TRAILS makes deadlines read EARLY, not far away.
///
/// So exactly-on-the-clock is not the target: at a lead of zero a zero-horizon arm still
/// satisfies `time_now >= count` and fires immediately. That was measured rather than
/// argued from the source: an interleaved A/B of this floor against a lead of zero put 46
/// of 63 past-dated arms into a near class between -8.2 and -78.9 us (median -19.5 us),
/// none of them within 3 us of zero, and this floor removed that class outright. A zero
/// lead is materially worse, not merely "arms that were genuinely due".
///
/// What this floor has to cover, and the only thing it has to cover, is the delay L
/// between the guest READING its counter to compute a deadline and KVM evaluating that
/// deadline against the clock. Over L the clock advances and the guest's number does not,
/// so a zero-horizon arm reads as already past whenever the lead is at or under L.
///
/// L is a DISTRIBUTION, not a value, which is the whole reason this is 20 us and not 4.
/// Measured directly on this host by pairing `kvm_exit(MSR_WRITE)` with the `set_count` and
/// the `get_time_ref_counter` in `stimer_start` (~98k arms per run, 100% paired, two
/// independent arms agreeing): p50 3.2 us, p90 3.9, p99 12-14, p99.9 18.4-18.7, max 72-123.
/// The tail runs 20x the median, so sizing this off a TYPICAL L would leave everything past
/// the p99 uncovered - and an uncovered arm does not cost one late timer, it re-arms into
/// the storm. Hence a percentile, and hence a high one: 20 us is L's p99.9 rounded up. The
/// cost either side of that choice is asymmetric, a rare late delivery above it against an
/// unbounded failure below it. It is also the bottom of the 21-to-38 us achieved band that
/// measured clean (under 0.05 past-dated arms a second), so it sits at the edge of measured
/// data rather than extrapolated underneath it.
///
/// Firing a timer early to cover a latency that cannot be removed is not a new idea; it is
/// what KVM does for the LAPIC timer (`lapic_timer_advance`, arch/x86/kvm/lapic.c, "programs
/// the host timer event to fire early ... to account for the delay between taking the
/// VM-Exit ... and the subsequent VM-Enter"), applied in `start_sw_tscdeadline` as
/// `ktime_sub_ns(expire, timer_advance_ns)` behind a `ns > timer_advance_ns` guard. What
/// differs is the STAGE, and that is why the compensation sits here rather than nearer the
/// timer: KVM's advance is applied when a deadline BECOMES a host timer, and by then
/// `stimer_start` has already sorted the arm into past or future. The sorting is the thing
/// that goes wrong, so the only stage left to compensate at is the origin the deadline is
/// computed from.
///
/// Do NOT "simplify" this into an adaptive lead to match `adjust_lapic_timer_advance`. That
/// can close a loop because KVM holds `guest_tsc - tsc_deadline` on every expiry; this code
/// runs twice in a partition's life, at creation and at reset, and observes nothing
/// afterwards. Making it adaptive means first putting the ftrace pairing above into the arm
/// path, which is a different change with its own cost.
///
/// The cost of the floor is a synthetic timer delivered up to 20 us late, about 1% of the
/// 1.978 ms one-shot period this guest arms.
///
/// This is a floor, not the lead: the lead asked for is this plus a term for how well the
/// alignment knows its own inputs, so a host whose measurement is poor gets more margin
/// and one whose measurement is exact pays only this. See [`guest_tsc_lead_ns`].
const GUEST_TSC_LEAD_FLOOR_NS: u64 = 20_000;

/// The lead to ask for, given how well the counter/clock pairing behind the correction is
/// known.
///
/// A single constant is wrong in both directions. Too large and every synthetic timer is
/// delivered that late for nothing. Too small and the alignment's OWN error can swallow
/// it: the correction lands the counter at `requested` plus or minus that error, so a
/// request under the error can leave the counter BEHIND the clock - the exact failure the
/// lead exists to prevent, and silently, because the error was measured and then not acted
/// on.
///
/// So the request is the floor plus the error. At the worst case of the residual the
/// ACHIEVED lead is still at least the floor, and when the pairing is exact - KVM's own
/// counter/clock pair, error zero - nothing is paid for accuracy that was not needed.
fn guest_tsc_lead_ns(pairing_error_ns: u64) -> u64 {
    GUEST_TSC_LEAD_FLOOR_NS.saturating_add(pairing_error_ns)
}

/// How many times the reference clock read is bracketed by counter reads, the narrowest
/// bracket winning.
///
/// One bracket is enough for correctness - the midpoint estimate is unbiased either way -
/// but its error is half the bracket width, and a bracket that catches a preemption or a
/// host interrupt is wide. Taking the narrowest of a few costs two register reads each and
/// bounds the residual by the best sample rather than by an arbitrary one.
const REFERENCE_CLOCK_BRACKET_SAMPLES: usize = 3;

/// What an alignment did, as measured afterwards rather than as intended.
///
/// Absent when the kernel cannot express the alignment, so that a reader can tell "the
/// counter did not move" from "we never asked it to".
#[derive(Copy, Clone)]
struct GuestTscAlignment {
    before: u64,
    after: u64,
    /// Where the counter/clock pairing the correction was computed from came from.
    pairing: &'static str,
    /// How far that pairing could be out, in nanoseconds. Zero when it was exact.
    pairing_error_ns: u64,
    /// The lead asked for, and the lead a fresh reading found afterwards. The second is
    /// the one that decides whether the guest is safe; they differ by the residual.
    requested_lead_ns: u64,
    achieved_lead_ns: i64,
}

/// Where the guest counter's value at the instant of a reference clock read came from.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum CounterPairing {
    /// KVM reported the host counter it sampled the clock at, and translating it into the
    /// guest's view lands inside the bracket measured around the same read. Exact: the
    /// kernel and the caller are describing one instant, not two.
    Exact(u64),
    /// KVM did not report a host counter. Only the masterclock branch of `__get_kvmclock`
    /// fills the field, so this says the partition was not on the masterclock at the
    /// moment of the read.
    NotReported,
    /// KVM reported a host counter whose translation does NOT land inside the bracket, by
    /// this many ticks. Not usable: the guest sees `scale(host tsc) + offset`, so adding
    /// the offset alone assumes the scale is the identity, which holds only while the
    /// guest counter runs at the host's own rate. A host that scales it fails here by far
    /// more than a bracket, which is what makes the bracket a workable check on the
    /// assumption rather than a formality.
    Disagrees(u64),
}

impl CounterPairing {
    /// A stable name for the log, so a reader can tell which path an alignment took, and
    /// on the fallback why.
    fn source(&self) -> &'static str {
        match self {
            CounterPairing::Exact(_) => "kvm_host_tsc",
            CounterPairing::NotReported => "bracket_host_tsc_not_reported",
            CounterPairing::Disagrees(_) => "bracket_host_tsc_disagreed",
        }
    }
}

/// Translates the host counter KVM paired with a reference clock read into the guest's
/// view of it, and checks that answer against the bracket measured around the same read.
///
/// `KVM_GET_CLOCK` reports `host_tsc` "at the instant when KVM_GET_CLOCK was called"
/// (`Documentation/virt/kvm/api.rst`, 4.29) and computes the clock it returns from exactly
/// that counter value (`__get_kvmclock` ends with
/// `data->clock = __pvclock_read_cycles(&hv_clock, data->host_tsc)`). That is the pairing
/// this whole function set is trying to establish, handed over for free - so use it, and
/// bracket only where it is genuinely absent.
///
/// The bracket does not go away, it changes job: from producing the estimate to checking
/// the exact value. The translation `host tsc + offset` is only the guest's view while the
/// counter is not scaled, and the bracket is a bound that the true value must lie inside,
/// so requiring the translation to land within it tests the assumption instead of
/// asserting it.
fn pair_guest_tsc_with_reference_clock(
    clock_host_tsc: Option<u64>,
    tsc_offset: u64,
    bracketed_guest_tsc: u64,
    bracket_error_ticks: u64,
) -> CounterPairing {
    let Some(host_tsc) = clock_host_tsc else {
        return CounterPairing::NotReported;
    };
    let paired = host_tsc.wrapping_add(tsc_offset);
    // Modular distance: the counter is 64 bits, and either value can be the larger one
    // when the pair straddles a wrap.
    let disagreement = paired
        .wrapping_sub(bracketed_guest_tsc)
        .min(bracketed_guest_tsc.wrapping_sub(paired));
    if disagreement <= bracket_error_ticks {
        CounterPairing::Exact(paired)
    } else {
        CounterPairing::Disagrees(disagreement)
    }
}

/// The reference clock, and the guest counter's value at the instant it was sampled.
struct ReferenceClockSample {
    reference_clock_ns: u64,
    /// The counter at the instant the clock was read.
    guest_tsc: u64,
    /// How far `guest_tsc` can be from the truth, in nanoseconds. Zero when it came from
    /// KVM's own pairing rather than from an estimate.
    error_ns: u64,
    /// Which of those two it was.
    pairing: CounterPairing,
}

/// Reads the partition reference clock and the guest counter as one paired observation.
///
/// Preferred form: KVM reports the host counter it sampled the clock at, and the guest's
/// view of that counter is the exact answer. Fallback: the clock read is a `KVM_GET_CLOCK`
/// ioctl costing real host time - about 22 us here - so the counter is read on BOTH sides
/// of it and the clock attributed to the midpoint. The ioctl's own duration then cancels
/// instead of surviving as a bias, and what is left is half the bracket width in an
/// unknown direction. The bracket is measured either way, because it is also what the
/// exact answer is checked against.
fn sample_reference_clock_against_counter(
    vm: &kvm::Partition,
    bsp: &kvm::Processor<'_>,
    tsc_offset: u64,
    guest_tsc_khz: u32,
) -> Result<ReferenceClockSample, KvmError> {
    let mut best: Option<ReferenceClockSample> = None;
    for _ in 0..REFERENCE_CLOCK_BRACKET_SAMPLES {
        let mut opening = [0u64; 1];
        bsp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut opening)?;
        let clock = vm.get_clock()?;
        let mut closing = [0u64; 1];
        bsp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut closing)?;

        let bracketed = counter_at_bracket_midpoint(opening[0], closing[0]);
        let error_ticks = bracket_width_ticks(opening[0], closing[0]) / 2;
        let pairing =
            pair_guest_tsc_with_reference_clock(clock.host_tsc, tsc_offset, bracketed, error_ticks);
        let sample = match pairing {
            CounterPairing::Exact(guest_tsc) => ReferenceClockSample {
                reference_clock_ns: clock.clock_ns,
                guest_tsc,
                error_ns: 0,
                pairing,
            },
            CounterPairing::NotReported | CounterPairing::Disagrees(_) => ReferenceClockSample {
                reference_clock_ns: clock.clock_ns,
                guest_tsc: bracketed,
                error_ns: ns_from_guest_tsc_ticks(error_ticks, guest_tsc_khz),
                pairing,
            },
        };
        // An exact pairing cannot be improved on, so stop: the remaining brackets exist
        // only to narrow an estimate that is no longer being made.
        if sample.error_ns == 0 {
            return Ok(sample);
        }
        if best
            .as_ref()
            .is_none_or(|best| sample.error_ns < best.error_ns)
        {
            best = Some(sample);
        }
    }
    // The loop runs a fixed, non-zero number of times, so a sample always exists. Written
    // as an expect rather than an unwrap so a later edit to the constant that made it zero
    // fails with the reason rather than as a bare panic.
    Ok(best.expect("at least one bracket is always sampled"))
}

/// Asks the kernel to re-evaluate its masterclock, so the clock reads that follow carry
/// the host counter they were sampled at.
///
/// `KVM_GET_CLOCK` fills `host_tsc` only on the masterclock branch (`__get_kvmclock`,
/// arch/x86/kvm/x86.c, under `ka->use_master_clock`), and `use_master_clock` is a CACHED
/// bool that only `pvclock_update_vm_gtod_copy` writes. On a cold start it was computed
/// once, from `kvm_arch_init_vm`, when the vm had no vcpus at all and the "every vcpu has a
/// matching TSC" test it depends on could not hold - so it is false. The vcpu creations
/// since then made that test true (each one after the first takes the `matched` branch of
/// `__kvm_synchronize_tsc` and raises `nr_vcpus_matched_tsc`) but only ASKED for a
/// recompute, via `KVM_REQ_MASTERCLOCK_UPDATE`, which a vp has to RUN to service. None has
/// run yet at the point the partition is built. Measured: 4 of 4 clock reads there came
/// back with no host counter, and every read once the guest was running had one.
///
/// `KVM_SET_CLOCK` is the way out, because `kvm_vm_ioctl_set_clock` calls
/// `pvclock_update_vm_gtod_copy` on the calling thread. Writing the clock back at the value
/// just read therefore recomputes the flag without waiting for a vp.
///
/// It is not quite a no-op - the kernel rebases `kvmclock_offset` onto the value passed, so
/// the clock is rewound by the time between the read and the write, tens of microseconds -
/// and that is acceptable at exactly this point and nowhere else: no vp has run, so nothing
/// has observed the clock, and the counter is then put on whatever the clock reads
/// AFTERWARDS. On a machine reset the caller has just written the clock anyway, so the
/// first read here already carries the counter and this returns without a second write.
///
/// Best effort throughout: if the pairing is still absent the alignment brackets instead,
/// and its log says which it used.
fn prime_reference_clock_pairing(vm: &kvm::Partition) -> Result<(), KvmError> {
    let clock = vm.get_clock()?;
    if clock.host_tsc.is_some() {
        return Ok(());
    }
    vm.set_clock_ns(clock.clock_ns)?;
    Ok(())
}

/// Puts the guest timestamp counter on the partition reference clock's origin, a
/// deliberate hair ahead of it, on every vp.
///
/// The partition has two views of time and a guest hypervisor calibrates one against the
/// other, so they have to start together. They do not, on their own, at either of the two
/// points where the partition's clock starts:
///
/// * On a cold start the kernel zeroes the reference clock when the vm is created
///   (`kvm_arch_init_vm` sets `kvmclock_offset` to minus the current base time) but fixes
///   the guest counter's origin only when the bsp vcpu is created
///   (`kvm_arch_vcpu_postcreate` -> `kvm_synchronize_tsc(vcpu, NULL)`). Everything between
///   those two ioctls - the supported-cpuid query, the leaf build, capability derivation -
///   is a gap the guest then carries for its whole life. Measured on this host at 832 to
///   977 us across seven starts, landing 1:1 in the horizon a guest hypervisor computes
///   for a synthetic timer deadline, which is enough to make it arm ~1.9 M past-dated
///   timers a second.
/// * On a machine reset `reset_all` sets the reference clock to its at-reset value of zero
///   and asks the same of each vp's `Tsc`. That second half silently does not happen: the
///   write goes to `MSR_IA32_TSC`, and the kernel reads a host write of exactly zero as
///   "userspace is creating or synchronizing this vcpu" rather than as a value to store -
///   it sets `synchronizing` and substitutes `kvm->arch.cur_tsc_offset`
///   (`kvm_synchronize_tsc`, arch/x86/kvm/x86.c, the branch commented "Force
///   synchronization when creating a vCPU, or when userspace explicitly writes a zero
///   value"). So zero, the one value a reset needs to deliver, is the one value that path
///   cannot deliver. Measured on a live guest: TSC 5403506220, wrote 0, read back
///   5403522705, while a control write of 0x4000000000000000 landed.
///
/// Both are corrected the same way and in the same direction: the reference clock is the
/// partition's authority on time - every synthetic timer deadline is expressed in it, and
/// `GetReferenceTime` reads it directly - while the counter is a per-vp view of the same
/// instant. So this moves the counter to the clock. Moving the clock to the counter would
/// work arithmetically but is the wrong instrument: `KVM_SET_CLOCK` is a partition-wide
/// write that invalidates the reference page and kicks every vcpu, its effective origin is
/// a timestamp the kernel samples inside the ioctl and never reports, and on a cold start
/// it would have to be issued in the middle of building the partition.
///
/// The correction goes through the vcpu device attribute `KVM_VCPU_TSC_OFFSET`, not
/// through `MSR_IA32_TSC`. `kvm_arch_tsc_set_attr` hands the caller's value straight to
/// `__kvm_synchronize_tsc`, so it lands verbatim; the MSR path infers what userspace
/// "meant" from the value's distance to where the counter would be anyway, and both of the
/// writes this function makes fall inside the window where that inference discards them.
/// The attribute is an OFFSET, not a counter value - the guest reads
/// `scale(host TSC) + offset` - so a correction of N ticks to the counter is a correction
/// of N ticks to the offset, and deriving it from the pair the guest itself reads keeps it
/// exact under TSC scaling without having to know the ratio.
///
/// Every vp gets the SAME offset, computed once. The kernel takes unequal offsets as
/// unsynchronized vcpus: `kvm_arch_tsc_set_attr` marks a write `matched` only when it
/// equals the last offset written, so one pass with one value opens a single TSC
/// generation that every vp joins, whereas a separately sampled value per vp would open
/// one generation per write and drop the partition out of masterclock mode - which is what
/// the reference clock is built on.
///
/// The counter is left deliberately AHEAD of the clock, by [`guest_tsc_lead_ns`]. Landing
/// it exactly on the clock is not the safe target and landing it behind is the failure
/// itself; see [`GUEST_TSC_LEAD_FLOOR_NS`] for which direction is which and why.
///
/// The lead is then MEASURED rather than assumed. Everything above says what the write
/// should produce; only a fresh reading of the same pair afterwards says what it did, and
/// a host where that comes out wrong is exactly the host that would otherwise storm in
/// silence. So the reading is taken, compared to what was asked for, and reported at a
/// level that matches how bad the answer is.
fn align_guest_tsc_to_reference_clock(
    vm: &kvm::Partition,
    bsp_apic_id: u32,
    apic_ids: impl Iterator<Item = u32>,
) -> Result<Option<GuestTscAlignment>, KvmError> {
    let bsp = vm.vp(bsp_apic_id);
    if !bsp.supports_tsc_offset() {
        // The attribute landed in Linux 5.16. Older kernels have no way to express this,
        // and failing outright would be a worse outcome than a counter left on the wrong
        // origin. Say so loudly, so a later guest bugcheck is not unexplained.
        tracing::warn!(
            "kernel does not support KVM_VCPU_TSC_OFFSET; \
             the guest tsc will not be aligned to the partition reference clock"
        );
        return Ok(None);
    }

    // Every conversion below is scaled by this rate, so a rate that cannot be read does
    // not merely make the correction inaccurate: the target counter falls out as zero
    // ticks and the offset written to every vp puts the guest counter on an origin
    // unrelated to anything. That is strictly worse than the no-op of leaving the counter
    // alone, which is at least self-consistent, and the verification afterwards would
    // report the missing lead without being able to undo the write. So this degrades
    // exactly like the unsupported-attribute branch above does: the alignment is best
    // effort and must not fail the partition build over it.
    //
    // Whether a rate is usable at all is the wrapper's judgement, not this function's -
    // it rejects anything not strictly positive - so there is one place to look and no
    // second opinion to keep in step with it here.
    let guest_tsc_khz = match bsp.tsc_khz() {
        Ok(khz) => khz,
        Err(err) => {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "could not read a usable guest tsc rate; \
                 the guest tsc will not be aligned to the partition reference clock"
            );
            return Ok(None);
        }
    };
    let offset = bsp.tsc_offset()?;

    prime_reference_clock_pairing(vm)?;

    let sample = sample_reference_clock_against_counter(vm, &bsp, offset, guest_tsc_khz)?;
    if let CounterPairing::Disagrees(by_ticks) = sample.pairing {
        // Worth saying out loud rather than silently degrading: the kernel offered a
        // pairing and the guest's view of it is not where the bracket says the counter
        // was. On a host that scales the guest counter that is expected and the fallback
        // is correct; anything else means one of the two readings is not what it claims.
        tracing::warn!(
            by_ticks,
            "kvm reported a host counter for the reference clock that is not the guest's \
             view of it; falling back to bracketing"
        );
    }

    let before = sample.guest_tsc;
    let requested_lead_ns = guest_tsc_lead_ns(sample.error_ns);
    let new_offset = tsc_offset_aligned_to_reference_clock(
        offset,
        before,
        sample.reference_clock_ns,
        guest_tsc_khz,
        requested_lead_ns,
    );
    for apic_id in apic_ids {
        vm.vp(apic_id).set_tsc_offset(new_offset)?;
    }

    // Measure the result rather than assume it, on both counts. The write goes through the
    // kernel's TSC synchronization, which is entitled to adjust what it stores - a write
    // that reported success and changed nothing is exactly the defect this replaces - and
    // the lead the guest will actually see is the only thing that decides whether its
    // synthetic timers arm past-dated. So the pair is read again, under the offset just
    // written, and the lead it produced is computed from that reading.
    let verify = sample_reference_clock_against_counter(vm, &bsp, new_offset, guest_tsc_khz)?;
    let achieved_lead_ns = guest_tsc_lead_from_reference_clock(
        verify.guest_tsc,
        verify.reference_clock_ns,
        guest_tsc_khz,
    );
    // Both readings contribute: the first decides where the counter was put, the second
    // where it is seen to be, so the band the answer is allowed to land in is as wide as
    // the two of them together - plus what the units themselves cannot resolve, without
    // which an exact pairing leaves no band at all.
    let resolution_ns = lead_measurement_resolution_ns(guest_tsc_khz);
    let tolerances = LeadTolerances {
        requested_ns: requested_lead_ns,
        band_ns: sample
            .error_ns
            .saturating_add(verify.error_ns)
            .saturating_add(resolution_ns),
        resolution_ns,
    };
    let alignment = GuestTscAlignment {
        before,
        after: verify.guest_tsc,
        pairing: sample.pairing.source(),
        pairing_error_ns: sample.error_ns,
        requested_lead_ns,
        achieved_lead_ns,
    };
    match classify_achieved_lead(achieved_lead_ns, &tolerances) {
        LeadVerdict::AsIntended => tracing::info!(
            achieved_lead_ns,
            requested_lead_ns,
            tolerance_ns = tolerances.band_ns,
            pairing = alignment.pairing,
            "guest tsc aligned to the partition reference clock"
        ),
        LeadVerdict::OutsideBand => tracing::warn!(
            achieved_lead_ns,
            requested_lead_ns,
            tolerance_ns = tolerances.band_ns,
            pairing = alignment.pairing,
            "guest tsc lead landed outside the band its own measurement error allows"
        ),
        LeadVerdict::BelowFloor => tracing::error!(
            achieved_lead_ns,
            requested_lead_ns,
            floor_ns = GUEST_TSC_LEAD_FLOOR_NS,
            resolution_ns = tolerances.resolution_ns,
            pairing = alignment.pairing,
            "guest tsc lead is under its floor; the guest may arm past-dated synthetic timers"
        ),
    }
    Ok(Some(alignment))
}

impl Partition for KvmPartition {
    fn supports_reset(&self) -> Option<&dyn ResetPartition<Error = Self::Error>> {
        Some(self)
    }

    fn supports_initial_page_acceptance(
        &self,
    ) -> Option<&dyn virt::AcceptInitialPages<Error = <Self as Hv1>::Error>> {
        self.inner.sev.is_some().then_some(self)
    }

    fn doorbell_registration(
        self: &Arc<Self>,
        _minimum_vtl: Vtl,
    ) -> Option<Arc<dyn DoorbellRegistration>> {
        Some(self.clone())
    }

    fn as_signal_msi(&self, _vtl: Vtl) -> Option<Arc<dyn SignalMsi>> {
        Some(self.inner.clone())
    }

    fn irqfd(&self) -> Option<Arc<dyn virt::irqfd::IrqFd>> {
        Some(self.irqfd_state.clone())
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

    fn synic(&self) -> anyhow::Result<Arc<dyn vmcore::synic::SynicPortAccess>> {
        Ok(self.synic_ports.clone())
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
        let clock = self.kvm.get_clock().unwrap();
        ReferenceTimeResult {
            ref_time: clock.clock_ns / 100,
            system_time: clock
                .realtime_ns
                .map(|ns| jiff::Timestamp::from_nanosecond(ns as i128).unwrap()),
        }
    }
}

impl virt::BindProcessor for KvmProcessorBinder {
    type Processor<'a> = KvmProcessor<'a>;
    type Error = KvmError;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        let inner = &self.partition.vps[self.vpindex.index() as usize];
        let vp_info = inner.vp_info;
        let kvm = self.partition.kvm.vp(vp_info.apic_id);

        // Enable synic and set initial MSRs.
        if self.partition.hv1_enabled {
            kvm.enable_synic()?;

            // Set the VP index. Also, KVM incorrectly initializes
            // SCONTROL to 0. Set it to 1 on each processor.
            kvm.set_msrs(&[
                (
                    hvdef::HV_X64_MSR_VP_INDEX,
                    vp_info.base.vp_index.index().into(),
                ),
                (hvdef::HV_X64_MSR_SCONTROL, 1),
            ])?;
        }

        // Unlike the Microsoft hypervisor, KVM allows this MSR to be
        // set and defaults it to zero. Hard code the value here to the
        // same as the Microsoft hypervisor.
        kvm.set_msrs(&[(
            x86defs::X86X_IA32_MSR_MISC_ENABLE,
            hv1_emulator::x86::MISC_ENABLE.into(),
        )])?;

        // Set IA32_FEATURE_CONTROL on Intel processors. KVM initializes
        // this MSR to 0; set the lock bit (as Hyper-V does) so that the
        // MSR reads as locked, and enable VMX outside SMX if the guest
        // has VMX in CPUID.
        if self.partition.caps.vendor.is_intel_compatible() {
            let ecx = x86defs::cpuid::VersionAndFeaturesEcx::from(
                self.partition
                    .cpuid
                    .result(CpuidFunction::VersionAndFeatures.0, 0, &[0; 4])[2],
            );
            kvm.set_msrs(&[(
                x86defs::X86X_IA32_MSR_FEATURE_CONTROL,
                u64::from(
                    x86defs::vmx::Ia32FeatureControl::new()
                        .with_locked(true)
                        .with_vmx_enabled_outside_smx(ecx.vmx()),
                ),
            )])?;
        }

        // Advertise CMCI support (MCG_CMCI_P) in the guest's IA32_MCG_CAP when
        // the host permits it. KVM's in-kernel LAPIC only exposes the CMCI LVT
        // register (APIC offset 0x2F0) when MCG_CMCI_P is set, yet KVM defaults
        // MCG_CAP with it clear; without it, a guest that programs the CMCI LVT
        // via an x2APIC MSR takes a #GP. Preserve the default bank count and
        // other capability bits.
        if self.partition.mce_cmci_supported {
            let mut mcg_cap = [0u64];
            kvm.get_msrs(&[x86defs::X86X_MSR_MCG_CAP], &mut mcg_cap)?;
            let cap = x86defs::McgCap::from(mcg_cap[0]);
            if !cap.cmci_p() {
                kvm.setup_mce(cap.with_cmci_p(true).into())?;
            }
        }

        // Set per-VP CPUID entries, fixing up APIC ID fields.
        //
        // TODO: centralize this code, probably in the topology crate,
        // for use by other hypervisors.
        let reserved_vps_per_socket = self.partition.reserved_vps_per_socket;
        let cpuid_entries = self
            .partition
            .cpuid
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
                        let eax = x86defs::cpuid::ProcessorTopologyDefinitionEax::from(entry.eax);
                        entry.eax = eax.with_extended_apic_id(vp_info.apic_id).into();
                        let ebx = x86defs::cpuid::ProcessorTopologyDefinitionEbx::from(entry.ebx);
                        entry.ebx = ebx
                            .with_compute_unit_id(
                                (vp_info.apic_id % reserved_vps_per_socket
                                    / (ebx.threads_per_compute_unit() as u32 + 1))
                                    as u8,
                            )
                            .into();
                        let ecx = x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(entry.ecx);
                        entry.ecx = ecx
                            .with_node_id((vp_info.apic_id / reserved_vps_per_socket) as u8)
                            .into();
                    }
                    _ => (),
                }
                entry
            })
            .collect::<Vec<_>>();

        kvm.set_cpuid(&cpuid_entries)?;

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
            simp_overlay: OverlayPage::default(),
            siefp_overlay: OverlayPage::default(),
            vmtime: &mut self.vmtime,
        };

        // 1. Reset the APIC state to clear the directed EOI bit, which is
        //    set by KVM by default but our IO-APIC does not support.
        // 2. Enable x2apic if the partition needs it.
        // 3. Reset register state since KVM does not have the right
        //    architectural values.
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

        if self.partition.sev.is_some() && !vp_info.base.is_bsp() {
            // NOTE: SNP APs are started through the guest's GHCB AP creation
            // request. Keep them halted so KVM can wake them to install the
            // guest-provided VMSA instead of blocking in the uninitialized/APIC
            // startup path, which would return -EAGAIN from kvm_run to usermode
            // instead of making forward progress.
            //
            // The flow on KVM + QEMU + OVMF is that QEMU first programs a VMSA
            // for each AP pointing to QEMU's reset vector, then OVMF sends an
            // INIT_SIPI to each AP to then place it into the halted state. We
            // may need to change this depending on the contract with what we
            // expect to load (UEFI vs direct boot).
            vp.kvm.set_mp_state(kvm::KVM_MP_STATE_HALTED)?;
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
    /// Overlay backing the synic message page (SIMP).
    simp_overlay: OverlayPage,
    /// Overlay backing the synic event flags page (SIEFP).
    siefp_overlay: OverlayPage,
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
        if !(self.scontrol.enabled() && self.simp.enabled()) {
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

/// Maps, moves, or unmaps a synic overlay page (SIMP or SIEFP) to match `reg`.
///
/// KVM (with `KVM_CAP_HYPERV_SYNIC2`) keeps the live page in guest RAM and does
/// not zero it when the guest enables the overlay. But the overlay is logically
/// separate from guest RAM: it is zeroed once and thereafter follows the
/// overlay. Routing through [`OverlayPage`] preserves that, so a freshly enabled
/// page is zeroed rather than exposing stale guest data that the in-kernel synic
/// would treat as an occupied message slot and refuse to deliver into.
fn sync_synic_overlay(overlay: &mut OverlayPage, reg: HvSynicSimpSiefp, gm: &GuestMemory) {
    let mut prot = KvmNoVtlProtections(gm);
    if let Err(err) = overlay.sync(reg.enabled(), reg.base_gpn(), &mut prot) {
        tracelimit::warn_ratelimited!(
            error = &err as &dyn std::error::Error,
            gpn = reg.base_gpn(),
            "failed to map synic overlay page"
        );
    }
}

/// A no-op [`VtlProtectAccess`] implementation for use without VTL protections,
/// as is the case for KVM. Locking a page simply pins it in guest memory;
/// unlocking is a no-op.
struct KvmNoVtlProtections<'a>(&'a GuestMemory);

impl hv1_emulator::VtlProtectAccess for KvmNoVtlProtections<'_> {
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        _check_perms: hvdef::HvMapGpaFlags,
        _new_perms: Option<hvdef::HvMapGpaFlags>,
    ) -> Result<guestmem::LockedPages, HvError> {
        // Overlay pages are written through the returned locked pages, so lock
        // them for write.
        self.0
            .lock_gpns(guestmem::AccessType::Write, false, &[gpn])
            .map_err(|_| HvError::OperationDenied)
    }

    fn unlock_overlay_page(&mut self, _gpn: u64) -> Result<(), HvError> {
        Ok(())
    }
}

pub(crate) struct KvmMsi {
    pub(crate) address_lo: u32,
    pub(crate) address_hi: u32,
    pub(crate) data: u32,
}

impl KvmMsi {
    pub(crate) fn new(request: MsiRequest) -> Option<Self> {
        // TODO: validate the high bits of the request as well, across the codebase.
        let request_address = MsiAddress::from(request.address as u32);
        if request_address.address() != x86defs::msi::MSI_ADDRESS {
            return None;
        }
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

        Some(Self {
            address_lo,
            address_hi,
            data,
        })
    }
}

impl KvmPartitionInner {
    fn request_msi(&self, request: MsiRequest) {
        let Some(KvmMsi {
            address_lo,
            address_hi,
            data,
        }) = KvmMsi::new(request)
        else {
            tracelimit::warn_ratelimited!(
                address = request.address,
                data = request.data,
                "invalid MSI address"
            );
            return;
        };
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

struct KvmX86MsiRouteBuilder;

impl MsiRouteBuilder for KvmX86MsiRouteBuilder {
    fn routing_entry(
        &self,
        _partition: &KvmPartitionInner,
        address: u64,
        data: u32,
        _devid: Option<u32>,
    ) -> Option<kvm::RoutingEntry> {
        let KvmMsi {
            address_lo,
            address_hi,
            data,
        } = KvmMsi::new(MsiRequest { address, data })?;
        Some(kvm::RoutingEntry::Msi {
            address_lo,
            address_hi,
            data,
            devid: None,
        })
    }
}

impl virt::irqfd::IrqFd for KvmIrqFdState {
    fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn virt::irqfd::IrqFdRoute>> {
        Ok(Box::new(self.new_irqfd_route(KvmX86MsiRouteBuilder)?))
    }
}

impl IoApicRouting for KvmPartitionInner {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        let entry = match request {
            Some(request) => match KvmMsi::new(request) {
                Some(KvmMsi {
                    address_lo,
                    address_hi,
                    data,
                }) => Some(kvm::RoutingEntry::Msi {
                    address_lo,
                    address_hi,
                    data,
                    devid: None,
                }),
                None => {
                    tracelimit::warn_ratelimited!(
                        irq,
                        address = request.address,
                        data = request.data,
                        "invalid MSI address for IO-APIC route"
                    );
                    None
                }
            },
            None => None,
        };
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

struct KvmHypercallExit<'a> {
    partition: &'a KvmPartitionInner,
    registers: KvmHypercallRegisters,
}

struct KvmHypercallRegisters {
    input: u64,
    params: [u64; 2],
    result: u64,
}

impl KvmHypercallExit<'_> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

impl<'a> hv1_hypercall::AsHandler<KvmHypercallExit<'a>> for &mut KvmHypercallExit<'a> {
    fn as_handler(&mut self) -> &mut KvmHypercallExit<'a> {
        self
    }
}

impl hv1_hypercall::HypercallIo for KvmHypercallExit<'_> {
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

impl hv1_hypercall::PostMessage for KvmHypercallExit<'_> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_post_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl hv1_hypercall::SignalEvent for KvmHypercallExit<'_> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.partition
            .synic_ports
            .handle_signal_event(Vtl::Vtl0, connection_id, flag)
    }
}

impl<'p> Processor for KvmProcessor<'p> {
    type StateAccess<'a>
        = KvmVpStateAccess<'a, 'p>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> Result<(), <KvmVpStateAccess<'_, '_> as AccessVpState>::Error> {
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
                        // Bring the overlay pages into agreement with the new
                        // SIMP/SIEFP values the guest just programmed. The
                        // overlays are owned by this processor; the save/restore
                        // path reaches them through the bound processor.
                        sync_synic_overlay(&mut self.simp_overlay, simp.into(), &self.partition.gm);
                        sync_synic_overlay(
                            &mut self.siefp_overlay,
                            siefp.into(),
                            &self.partition.gm,
                        );
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
                            partition: self.partition,
                            registers: KvmHypercallRegisters {
                                input,
                                params,
                                result: 0,
                            },
                        };
                        KvmHypercallExit::DISPATCHER.dispatch(&self.partition.gm, &mut handler);
                        *result = handler.registers.result;
                    }
                    kvm::Exit::Hypercall {
                        nr,
                        args,
                        result,
                        flags,
                    } => {
                        if nr == kvm::KVM_HC_MAP_GPA_RANGE_UAPI {
                            let gpa = args[0];
                            let page_count = args[1];
                            let map_attributes = args[2];

                            tracing::debug!(
                                gpa,
                                page_count,
                                map_attributes,
                                flags,
                                "handling KVM_HC_MAP_GPA_RANGE"
                            );
                            match self.partition.set_map_gpa_range_attributes(
                                gpa,
                                page_count,
                                map_attributes,
                            ) {
                                Ok(()) => {
                                    *result = 0;
                                    tracing::debug!(
                                        gpa,
                                        page_count,
                                        map_attributes,
                                        "handled KVM_HC_MAP_GPA_RANGE"
                                    );
                                }
                                Err(err) => {
                                    tracelimit::error_ratelimited!(
                                        error = &err as &dyn std::error::Error,
                                        gpa,
                                        page_count,
                                        map_attributes,
                                        "failed KVM_HC_MAP_GPA_RANGE"
                                    );
                                    *result = 1;
                                }
                            }
                        } else {
                            *result = 1;
                            return Err(dev.fatal_error(
                                KvmRunVpError::UnhandledHypercall { nr, flags }.into(),
                            ));
                        }
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
                    kvm::Exit::SystemEvent {
                        event_type,
                        event_flags,
                    } => {
                        // KVM reports architectural shutdown/reset/crash
                        // notifications here; SNP adds SEV termination handling.
                        tracing::info!(event_type, event_flags, "system event");
                        match event_type {
                            kvm::KVM_SYSTEM_EVENT_SHUTDOWN => {
                                return Err(VpHaltReason::PowerOff);
                            }
                            kvm::KVM_SYSTEM_EVENT_RESET => {
                                return Err(VpHaltReason::Reset);
                            }
                            kvm::KVM_SYSTEM_EVENT_CRASH => {
                                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                            }
                            kvm::KVM_SYSTEM_EVENT_SEV_TERM => {
                                let ghcb_msr = event_flags;
                                return Err(dev.fatal_error(
                                    KvmRunVpError::SevTermination {
                                        ghcb_msr,
                                        reason_set: (ghcb_msr >> 12) & 0xf,
                                        reason: (ghcb_msr >> 16) & 0xff,
                                    }
                                    .into(),
                                ));
                            }
                            _ => {
                                return Err(dev.fatal_error(
                                    KvmRunVpError::UnhandledSystemEvent(event_type).into(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn flush_async_requests(&mut self) {}

    fn reset(&mut self) -> Result<(), impl std::error::Error + Send + Sync + 'static> {
        let vp_info = self.inner.vp_info;
        self.access_state(Vtl::Vtl0).reset_all(&vp_info)
    }

    fn access_state(&mut self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0);
        KvmVpStateAccess::new(self)
    }
}

impl virt::synic::Synic for KvmPartitionInner {
    fn port_map(&self) -> &virt::synic::SynicPortMap {
        &self.synic_ports
    }

    fn post_message(&self, _vtl: Vtl, vp_index: VpIndex, sint: u8, typ: u32, payload: &[u8]) {
        let Some(vp) = self.vp(vp_index) else {
            tracelimit::warn_ratelimited!(?vp_index, "post_message for invalid vp_index");
            return;
        };

        let wake = vp
            .synic_message_queue
            .enqueue_message(sint, &HvMessage::new(HvMessageType(typ), 0, payload));

        if wake {
            self.evaluate_vp(vp_index);
        }
    }

    fn new_guest_event_port(
        self: Arc<Self>,
        _vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> Box<dyn GuestEventPort> {
        Box::new(KvmGuestEventPort {
            partition: Arc::downgrade(&self),
            gm: self.gm.clone(),
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

impl SignalMsi for KvmPartitionInner {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        self.request_msi(MsiRequest { address, data });
    }
}

/// Converts a duration in nanoseconds to guest timestamp counter ticks.
///
/// Widened for the multiply alone: a long-running partition's reference clock times a
/// GHz-scale rate leaves 64 bits well before the counter it describes does.
fn guest_tsc_ticks_from_ns(ns: u64, guest_tsc_khz: u32) -> u64 {
    (ns as u128 * guest_tsc_khz as u128 / 1_000_000) as u64
}

/// Converts guest timestamp counter ticks to nanoseconds.
///
/// The inverse of [`guest_tsc_ticks_from_ns`], widened for the same reason: a counter that
/// has been running a while, times a nanosecond scale, leaves 64 bits long before the
/// counter itself does.
fn ns_from_guest_tsc_ticks(ticks: u64, guest_tsc_khz: u32) -> u64 {
    if guest_tsc_khz == 0 {
        // The kernel reports the rate of a vcpu it has already created, so this does not
        // happen. Answering zero rather than dividing by it keeps a kernel that surprises
        // us out of a panic in the middle of building a partition; the caller's own
        // verification then reports the lead as absent rather than as correct.
        return 0;
    }
    (ticks as u128 * 1_000_000 / guest_tsc_khz as u128) as u64
}

/// The guest counter value that sits at least `lead_ns` ahead of a reference clock
/// reading, in ticks, rounding UP.
///
/// ONE ceiling over the whole sum, not a truncated clock plus a separately rounded-up
/// lead. Rounding the lead alone is not enough and the difference is the whole defect this
/// replaces: [`guest_tsc_ticks_from_ns`] truncates, so converting the clock on its own
/// throws away its fractional tick, and the counter then lands that far under
/// `reference + lead` however carefully the lead itself was rounded. Whether it does is
/// decided by the fractional part of the clock's own tick equivalent, which is effectively
/// random per boot - measured on this host, a 20 us lead placed by truncate-then-add came
/// back under its floor on about a third of boots, always by 1 to 2 ns, and clean on the
/// rest.
///
/// Ceiling the sum instead puts the counter at or above `reference + lead` for every
/// clock value, with the overshoot bounded by a single tick.
fn guest_tsc_ticks_at_reference_clock_plus_lead(
    reference_clock_ns: u64,
    lead_ns: u64,
    guest_tsc_khz: u32,
) -> u64 {
    ((reference_clock_ns as u128 + lead_ns as u128) * guest_tsc_khz as u128).div_ceil(1_000_000)
        as u64
}

/// How far UNDER the true lead the verification's own arithmetic can read, in nanoseconds.
///
/// The verification is a measurement, so it has a resolution, and the floor has to be
/// judged at that resolution or the alarm reports the instrument rather than the counter.
/// Three steps in it discard something, and two discard it downward:
///
/// * the reference clock is reported in whole nanoseconds, and the reading the correction
///   was computed from and the reading the check takes are two such reports of one clock,
///   so the span between them can be a nanosecond short of the truth;
/// * the final ticks-to-nanoseconds conversion truncates, losing up to one more;
/// * the difference itself is carried in whole counter ticks, worth `ceil(1e6 / khz)`
///   nanoseconds once rendered.
///
/// Truncating the clock into ticks is the third step and is left out on purpose: it moves
/// the reference BACKWARDS, so it can only inflate the reported lead, and an allowance for
/// it would be an allowance against nothing.
///
/// Derived from the conversions rather than picked, so a slow counter - whose tick is
/// worth whole nanoseconds - gets the allowance it needs instead of the one that suited
/// this host.
fn lead_measurement_resolution_ns(guest_tsc_khz: u32) -> u64 {
    if guest_tsc_khz == 0 {
        return 2;
    }
    2 + 1_000_000u64.div_ceil(guest_tsc_khz as u64)
}

/// How far the guest counter leads the partition reference clock, in nanoseconds.
///
/// SIGNED, because the sign is the safety property rather than a presentational detail:
/// negative is the counter TRAILING the clock, which is the state that makes a guest
/// hypervisor arm past-dated synthetic timers. An unsigned difference would render exactly
/// the dangerous case as a very large safe-looking one.
///
/// Differenced in TICKS and converted once, not converted twice and differenced. The two
/// operands are whole counters and the answer is tens of microseconds, so converting each
/// to nanoseconds separately puts the full quantization error of both into the answer;
/// against a floor that is one part in fifty thousand of the operands, that is exactly
/// where a spurious shortfall comes from.
fn guest_tsc_lead_from_reference_clock(
    guest_tsc: u64,
    reference_clock_ns: u64,
    guest_tsc_khz: u32,
) -> i64 {
    if guest_tsc_khz == 0 {
        return 0;
    }
    // Modular, then read as signed: the counter and the clock's tick equivalent can
    // straddle a wrap, and the counter trailing the clock has to come out negative.
    let lead_ticks =
        guest_tsc.wrapping_sub(guest_tsc_ticks_from_ns(reference_clock_ns, guest_tsc_khz)) as i64;
    ((lead_ticks as i128 * 1_000_000) / guest_tsc_khz as i128)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// How the lead an alignment achieved compares to the one it asked for.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum LeadVerdict {
    /// Within the band the alignment's own measurement error allows.
    AsIntended,
    /// Outside that band, but still clear of the floor. The guest is safe and the
    /// correction was simply less accurate than its inputs claimed.
    OutsideBand,
    /// Under the floor, negative included. The counter is close enough to the clock, or
    /// behind it, that a short-horizon arm can read as already past.
    BelowFloor,
}

/// What an achieved lead is judged against.
///
/// The two allowances answer different questions and must not be collapsed into one
/// number. `band_ns` is how far the correction could have MISSED its request, so it
/// carries the pairing error of both readings. `resolution_ns` is what the verification
/// cannot SEE, and only that. Admitting the pairing error at the floor would excuse a
/// genuine shortfall the size of a bad measurement, which is the one thing the floor
/// exists to catch; leaving the resolution out of it reports rounding as a fault.
struct LeadTolerances {
    requested_ns: u64,
    band_ns: u64,
    resolution_ns: u64,
}

/// Judges the lead an alignment actually achieved.
///
/// Two questions, and the second is the one that matters. Whether the lead landed near the
/// request says how good the correction was; whether it clears [`GUEST_TSC_LEAD_FLOOR_NS`]
/// says whether the guest is safe. An overshoot beyond what the measurement allows is
/// worth a look and harms nothing, while a shortfall to the floor is the storm returning,
/// so the caller reports them at different levels instead of collapsing both into one
/// "unexpected" line that a reader has to decode.
fn classify_achieved_lead(achieved_ns: i64, against: &LeadTolerances) -> LeadVerdict {
    // The sign is settled first, on its own terms. A negative lead is the counter behind
    // the clock, which is the state the whole check exists to catch, and no allowance for
    // what the instrument cannot see can make it acceptable.
    //
    // Stating it here also removes a coupling the rest of the function would otherwise
    // depend on without saying so: the band comparison below casts to `u64`, which is
    // sound only while every negative value has already been rejected. Today that holds
    // by arithmetic rather than by intent - a resolution derived from any plausible
    // counter rate cannot reach a 20 us floor, so the floor test catches the negatives
    // first - and it stops holding the moment the floor is lowered towards a microsecond.
    // A small negative lead would then clear the floor test, widen into a huge positive
    // on the cast, and be reported as a harmless overshoot: the dangerous direction
    // rendered as the safe one.
    if achieved_ns < 0 {
        return LeadVerdict::BelowFloor;
    }
    // Judged at the resolution of the instrument. A reported shortfall smaller than what
    // the verification's own conversions discard says nothing about where the counter is,
    // and firing on it costs the alarm its meaning: measured on this host, the bare
    // comparison fired on about a third of boots at 1 to 2 ns under a 20 us floor.
    let resolution_ns = against.resolution_ns.min(i64::MAX as u64) as i64;
    if achieved_ns.saturating_add(resolution_ns) < GUEST_TSC_LEAD_FLOOR_NS as i64 {
        return LeadVerdict::BelowFloor;
    }
    let low = against.requested_ns.saturating_sub(against.band_ns);
    let high = against.requested_ns.saturating_add(against.band_ns);
    if (achieved_ns as u64) < low || achieved_ns as u64 > high {
        return LeadVerdict::OutsideBand;
    }
    LeadVerdict::AsIntended
}

/// The number of counter ticks a bracket spans, i.e. how long the read it encloses took.
///
/// Modular, because the counter is 64 bits and an alignment can be asked for either side
/// of a wrap.
fn bracket_width_ticks(before: u64, after: u64) -> u64 {
    after.wrapping_sub(before)
}

/// The counter's value at the middle of a bracket.
///
/// The reference clock is sampled at an unknown instant inside the bracket, so the
/// midpoint is the estimate whose worst-case error is smallest: half the width, rather
/// than the whole of it that either endpoint alone would carry. That is what takes the
/// cost of the `KVM_GET_CLOCK` ioctl itself out of the answer, instead of leaving it in
/// the residual as a systematic bias in whichever direction the reads were ordered.
fn counter_at_bracket_midpoint(before: u64, after: u64) -> u64 {
    before.wrapping_add(bracket_width_ticks(before, after) / 2)
}

/// Returns the L1 TSC offset that puts the guest timestamp counter at least `lead_ns`
/// AHEAD of the partition reference clock.
///
/// The guest reads `scale(host TSC) + offset`, so moving the counter by a known number of
/// ticks means moving the offset by the same number: the correction is the difference
/// between where the counter should read - [`guest_tsc_ticks_at_reference_clock_plus_lead`]
/// - and where it does read.
///
/// The target is computed from the clock and the lead TOGETHER rather than converted
/// piecewise, because the lead is a minimum and only a single rounding of the whole
/// quantity can guarantee one; see that function.
fn tsc_offset_aligned_to_reference_clock(
    current_offset: u64,
    guest_tsc: u64,
    reference_clock_ns: u64,
    guest_tsc_khz: u32,
    lead_ns: u64,
) -> u64 {
    let target =
        guest_tsc_ticks_at_reference_clock_plus_lead(reference_clock_ns, lead_ns, guest_tsc_khz);
    // Modular throughout: the counter is 64 bits and wraps, and so does what stands in
    // front of it, so a correction that carries either end past a boundary is ordinary.
    current_offset.wrapping_add(target).wrapping_sub(guest_tsc)
}

#[cfg(test)]
mod tests {
    use super::CounterPairing;
    use super::GUEST_TSC_LEAD_FLOOR_NS;
    use super::LeadTolerances;
    use super::LeadVerdict;
    use super::bracket_width_ticks;
    use super::classify_achieved_lead;
    use super::counter_at_bracket_midpoint;
    use super::guest_tsc_lead_from_reference_clock;
    use super::guest_tsc_lead_ns;
    use super::guest_tsc_ticks_at_reference_clock_plus_lead;
    use super::guest_tsc_ticks_from_ns;
    use super::lead_measurement_resolution_ns;
    use super::ns_from_guest_tsc_ticks;
    use super::pair_guest_tsc_with_reference_clock;
    use super::tsc_offset_aligned_to_reference_clock;

    /// A plausible guest TSC rate for the hosts this runs on, in kHz.
    const KHZ: u32 = 2_700_000;

    /// Rates to sweep anything quantization-sensitive over. A single rate makes one
    /// fractional part stand for all of them, which is how a rounding defect hides.
    const RATES_KHZ: [u32; 5] = [2_701_631, 2_701_609, 2_700_000, 1_999_999, 3_800_017];

    /// Converts nanoseconds to guest TSC ticks the way the caller's clock does.
    fn ticks(ns: u64) -> u64 {
        (ns as u128 * KHZ as u128 / 1_000_000) as u64
    }

    /// The lead the production caller asks for when its pairing is exact, in nanoseconds.
    fn lead_ns() -> u64 {
        guest_tsc_lead_ns(0)
    }

    /// The counter the correction leaves behind, at the instant it was computed from.
    ///
    /// The guest reads `scale(host tsc) + offset`, so the same host instant that read
    /// `guest_tsc` under `offset` reads this under the new one.
    fn corrected_counter(offset: u64, guest_tsc: u64, new_offset: u64) -> u64 {
        guest_tsc.wrapping_add(new_offset.wrapping_sub(offset))
    }

    /// Asserts a counter really is at or past `reference_clock_ns + lead_ns`, compared as
    /// exact rationals.
    ///
    /// Deliberately not expressed through either conversion. Checking a rounded counter
    /// against a rounded expectation lets the two roundings agree with each other and
    /// prove nothing, which is precisely how the shortfall this guards reached a live
    /// host; cross-multiplying leaves nothing to round.
    fn assert_counter_clears(counter: u64, reference_clock_ns: u64, lead_ns: u64, khz: u32) {
        let have = counter as u128 * 1_000_000;
        let want = (reference_clock_ns as u128 + lead_ns as u128) * khz as u128;
        assert!(
            have >= want,
            "khz={khz} clock_ns={reference_clock_ns} lead_ns={lead_ns}: \
             counter {counter} is short of the clock plus the lead by {} ticks",
            (want - have) as f64 / 1_000_000.0,
        );
    }

    /// The judgement the production caller makes, with its two allowances kept distinct.
    fn tolerances(requested_ns: u64, pairing_error_ns: u64, khz: u32) -> LeadTolerances {
        let resolution_ns = lead_measurement_resolution_ns(khz);
        LeadTolerances {
            requested_ns,
            band_ns: pairing_error_ns.saturating_add(resolution_ns),
            resolution_ns,
        }
    }

    #[test]
    fn a_counter_that_trails_the_reference_clock_is_advanced_to_it() {
        // Cold boot: the kernel zeroes the kvmclock when the vm is created and the guest
        // TSC only when the bsp vcpu is created, so the counter starts a creation's worth
        // of time behind the clock.
        let gap_ns = 900_000;
        let offset = 0x1234_5678_9abc_def0;
        let new = tsc_offset_aligned_to_reference_clock(offset, 0, gap_ns, KHZ, lead_ns());
        assert_eq!(
            new,
            offset.wrapping_add(guest_tsc_ticks_at_reference_clock_plus_lead(
                gap_ns,
                lead_ns(),
                KHZ
            ))
        );
        assert_counter_clears(corrected_counter(offset, 0, new), gap_ns, lead_ns(), KHZ);
    }

    #[test]
    fn a_counter_that_leads_the_reference_clock_is_retarded_to_it() {
        // Machine reset: the reference clock has just been returned to zero and the
        // counter still carries the previous boot's cycles.
        let elapsed_ns = 5_000;
        let offset = 0x1234_5678_9abc_def0;
        let guest_tsc = 5_403_506_220;
        let new =
            tsc_offset_aligned_to_reference_clock(offset, guest_tsc, elapsed_ns, KHZ, lead_ns());
        assert_eq!(
            new,
            offset.wrapping_sub(guest_tsc).wrapping_add(
                guest_tsc_ticks_at_reference_clock_plus_lead(elapsed_ns, lead_ns(), KHZ)
            )
        );
        assert_counter_clears(
            corrected_counter(offset, guest_tsc, new),
            elapsed_ns,
            lead_ns(),
            KHZ,
        );
    }

    #[test]
    fn a_counter_already_on_the_reference_clock_is_advanced_by_the_lead_alone() {
        // Zero is NOT the target. A deadline the guest computes from a counter sitting
        // exactly on the clock still satisfies stimer_start's `time_now >= count` at a
        // zero horizon, so the alignment has to leave the counter ahead.
        //
        // A clock whose tick equivalent is exact, so "advanced by the lead alone" is a
        // statement about the lead and not about where the clock's own fraction landed.
        let ns = 12_345_670;
        assert_eq!(
            ns as u128 * KHZ as u128 % 1_000_000,
            0,
            "clock must be exact"
        );
        let offset = 0x1234_5678_9abc_def0;
        let new = tsc_offset_aligned_to_reference_clock(offset, ticks(ns), ns, KHZ, lead_ns());
        assert_eq!(
            new,
            offset.wrapping_add((lead_ns() as u128 * KHZ as u128).div_ceil(1_000_000) as u64)
        );
        assert_counter_clears(
            corrected_counter(offset, ticks(ns), new),
            ns,
            lead_ns(),
            KHZ,
        );
    }

    #[test]
    fn the_corrected_counter_ends_ahead_of_the_clock_by_the_lead() {
        // The direction is the whole point of the fix, so assert it as the inequality a
        // reader cares about rather than only as an arithmetic identity: behind is the
        // storm, ahead is safe.
        //
        // Swept over the CLOCK as well as the starting gap, and compared as exact
        // rationals. The clock's own fractional tick is what the correction used to throw
        // away, so a single clock value - or a comparison made through the same truncating
        // conversion the correction uses - agrees with itself and misses it.
        let offset = 0x0000_0100_0000_0000u64;
        for khz in RATES_KHZ {
            for clock_ns in [
                4_000_000u64,
                4_000_001,
                17_807_900,
                123_456_789,
                999_999_999,
            ] {
                for behind_ns in [0, 1, 900_000] {
                    let guest_tsc = guest_tsc_ticks_from_ns(clock_ns, khz)
                        - guest_tsc_ticks_from_ns(behind_ns, khz);
                    let new = tsc_offset_aligned_to_reference_clock(
                        offset,
                        guest_tsc,
                        clock_ns,
                        khz,
                        lead_ns(),
                    );
                    let corrected = corrected_counter(offset, guest_tsc, new);
                    assert_counter_clears(corrected, clock_ns, lead_ns(), khz);
                    // And not by more than the single tick the rounding is allowed to add,
                    // so the guarantee is not bought with unbounded margin. Both sides
                    // scaled by 1e6, as in the clearance check, to keep it exact.
                    let overshoot = corrected as u128 * 1_000_000
                        - (clock_ns as u128 + lead_ns() as u128) * khz as u128;
                    assert!(
                        overshoot < 1_000_000,
                        "khz={khz} clock_ns={clock_ns} behind_ns={behind_ns}: \
                         overshoot of {} ticks exceeds one",
                        overshoot as f64 / 1_000_000.0,
                    );
                }
            }
        }
    }

    #[test]
    fn a_zero_lead_leaves_the_counter_exactly_on_the_clock() {
        // The lead is a parameter, not baked into the correction, so the arithmetic
        // without it is still the plain alignment. Asserted at a clock whose tick
        // equivalent is exact, where "on the clock" has an unambiguous answer.
        let ns = 12_345_670;
        assert_eq!(
            ns as u128 * KHZ as u128 % 1_000_000,
            0,
            "clock must be exact"
        );
        let offset = 0x1234_5678_9abc_def0;
        assert_eq!(
            tsc_offset_aligned_to_reference_clock(offset, ticks(ns), ns, KHZ, 0),
            offset
        );
    }

    #[test]
    fn a_long_uptime_does_not_overflow_the_conversion() {
        // A year of reference clock times a GHz-scale rate exceeds 64 bits as a product,
        // so the conversion has to be done wider than the values it converts.
        let ns = 365 * 24 * 60 * 60 * 1_000_000_000u64;
        let offset = 7;
        let expected = (ns as u128 * KHZ as u128 / 1_000_000) as u64;
        assert_eq!(guest_tsc_ticks_from_ns(ns, KHZ), expected);
        assert_eq!(
            tsc_offset_aligned_to_reference_clock(offset, 0, ns, KHZ, 0),
            offset.wrapping_add(expected)
        );
    }

    #[test]
    fn an_offset_correction_below_zero_wraps_rather_than_panicking() {
        // The offset is modular: the guest counter is 64 bits and so is what stands in
        // front of it. A correction that takes the offset below zero is ordinary.
        assert_eq!(
            tsc_offset_aligned_to_reference_clock(10, 100, 2, KHZ, 0),
            10u64
                .wrapping_add(guest_tsc_ticks_at_reference_clock_plus_lead(2, 0, KHZ))
                .wrapping_sub(100)
        );
    }

    #[test]
    fn a_bracket_midpoint_is_half_way_between_its_ends() {
        assert_eq!(bracket_width_ticks(1_000, 1_100), 100);
        assert_eq!(counter_at_bracket_midpoint(1_000, 1_100), 1_050);
        // Odd widths round toward the opening read, which is the conservative half: it
        // attributes the clock slightly early, leaving the counter slightly further
        // ahead rather than slightly behind.
        assert_eq!(counter_at_bracket_midpoint(1_000, 1_101), 1_050);
    }

    #[test]
    fn a_zero_width_bracket_is_its_own_midpoint() {
        assert_eq!(bracket_width_ticks(42, 42), 0);
        assert_eq!(counter_at_bracket_midpoint(42, 42), 42);
    }

    #[test]
    fn a_bracket_spanning_the_counter_wrap_stays_correct() {
        // The counter is 64 bits and an alignment can be asked for either side of a wrap,
        // so the width has to be modular rather than a subtraction that would panic in
        // debug and produce an absurd midpoint in release.
        let before = u64::MAX - 9;
        let after = before.wrapping_add(20);
        assert_eq!(bracket_width_ticks(before, after), 20);
        assert_eq!(
            counter_at_bracket_midpoint(before, after),
            before.wrapping_add(10)
        );
    }

    #[test]
    fn the_midpoint_halves_the_error_either_endpoint_would_carry() {
        // The reason for bracketing at all: the clock is sampled at an unknown instant
        // inside the bracket, so the worst case over that interval is what matters, and
        // the midpoint's is half of what either end alone would carry.
        let (before, after) = (10_000u64, 10_600u64);
        let mid = counter_at_bracket_midpoint(before, after);
        let width = bracket_width_ticks(before, after);
        let worst = |estimate: u64| {
            let lo = estimate.abs_diff(before);
            let hi = estimate.abs_diff(after);
            lo.max(hi)
        };
        assert_eq!(worst(mid), width / 2);
        assert_eq!(worst(before), width);
        assert_eq!(worst(after), width);
    }

    #[test]
    fn the_host_counter_kvm_reports_is_used_when_it_lands_inside_the_bracket() {
        // KVM hands over the pairing the bracket exists to estimate, so take it: the
        // guest's view of the reported host counter is `host tsc + offset`, exactly.
        let offset = 0x0000_0500_0000_0000u64;
        let host_tsc = 900_000_000_000u64;
        let exact = host_tsc.wrapping_add(offset);
        // A bracket whose midpoint is 400 ticks off the truth, half-width 1000: the exact
        // value lies inside it, which is what makes it usable.
        assert_eq!(
            pair_guest_tsc_with_reference_clock(Some(host_tsc), offset, exact - 400, 1_000),
            CounterPairing::Exact(exact),
        );
    }

    #[test]
    fn a_host_counter_outside_the_bracket_is_rejected_rather_than_trusted() {
        // `host tsc + offset` is the guest's view only while the counter is unscaled. A
        // host that scales it misses the bracket by orders of magnitude, so the bracket is
        // a real test of that assumption and not a formality - and the reported distance
        // is what tells a reader which of the two readings to doubt.
        let offset = 0x0000_0500_0000_0000u64;
        let host_tsc = 900_000_000_000u64;
        let bracketed = host_tsc.wrapping_add(offset).wrapping_add(5_000);
        assert_eq!(
            pair_guest_tsc_with_reference_clock(Some(host_tsc), offset, bracketed, 1_000),
            CounterPairing::Disagrees(5_000),
        );
    }

    #[test]
    fn an_absent_host_counter_falls_back_to_the_bracket() {
        // The cold-boot case when the partition is not on the masterclock: the field is
        // not reported, and "not reported" has to be distinguishable from "reported as
        // zero", which is why the input is an option rather than a counter plus a flag.
        assert_eq!(
            pair_guest_tsc_with_reference_clock(None, 7, 12_345, 1_000),
            CounterPairing::NotReported,
        );
    }

    #[test]
    fn a_pairing_that_straddles_the_counter_wrap_is_still_recognised() {
        // Both the counter and the offset are modular, so the exact value and the bracket
        // can sit on opposite sides of a wrap. A plain subtraction would call that pair
        // 2^64 apart and reject a perfectly good pairing.
        let host_tsc = u64::MAX - 100;
        let offset = 200u64;
        let exact = host_tsc.wrapping_add(offset);
        // The exact value has wrapped past zero and the bracket midpoint has not, so the
        // two really are on opposite sides. Stepping back by less than that would leave
        // both above the boundary and the test would pass without exercising anything.
        let bracketed = exact.wrapping_sub(150);
        assert!(bracketed > exact, "the pair must straddle the wrap");
        assert_eq!(
            pair_guest_tsc_with_reference_clock(Some(host_tsc), offset, bracketed, 1_000),
            CounterPairing::Exact(exact),
        );
    }

    #[test]
    fn the_requested_lead_is_the_floor_plus_the_error_the_alignment_actually_has() {
        // The point of the runtime term: a correction known to within e can land the
        // counter e short of what it asked for, so asking for the floor alone lets that
        // residual eat the whole margin. Asking for floor + e keeps the ACHIEVED lead at
        // the floor even at the worst case.
        assert_eq!(guest_tsc_lead_ns(0), GUEST_TSC_LEAD_FLOOR_NS);
        assert_eq!(guest_tsc_lead_ns(11_000), GUEST_TSC_LEAD_FLOOR_NS + 11_000);
        // And a pathological error does not wrap the request round to a tiny one.
        assert_eq!(guest_tsc_lead_ns(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_worst_case_residual_still_leaves_the_counter_at_the_floor() {
        // The property the runtime term exists for, stated end to end: ask for the lead
        // the alignment's own error justifies, let the correction land at its worst, and
        // the counter is still at least the floor ahead of the clock.
        let error_ns = 11_000;
        let requested = guest_tsc_lead_ns(error_ns);
        for residual in [-(error_ns as i64), 0, error_ns as i64] {
            let achieved = requested as i64 + residual;
            assert!(
                achieved >= GUEST_TSC_LEAD_FLOOR_NS as i64,
                "residual={residual} left the counter under the floor at {achieved}"
            );
        }
    }

    #[test]
    fn a_counter_behind_the_clock_measures_as_a_negative_lead() {
        // The dangerous direction has to be REPRESENTABLE. Measured as an unsigned
        // difference it would come back as an enormous safe-looking number, which is how a
        // host in the failing state would report itself as healthy.
        let clock_ns = 4_000_000;
        let behind = guest_tsc_lead_from_reference_clock(ticks(clock_ns - 22_080), clock_ns, KHZ);
        assert!(behind < 0, "counter behind the clock must read negative");
        assert_eq!(behind, -22_080);
        assert_eq!(
            guest_tsc_lead_from_reference_clock(ticks(clock_ns + 20_000), clock_ns, KHZ),
            20_000,
        );
        assert_eq!(
            guest_tsc_lead_from_reference_clock(ticks(clock_ns), clock_ns, KHZ),
            0,
        );
    }

    #[test]
    fn the_correction_and_the_verification_agree_on_the_lead() {
        // Ties the two halves together: what the offset arithmetic asks for is what a
        // fresh reading of the same pair, under the new offset, measures. If these ever
        // disagreed the verification would be checking a different quantity from the one
        // being set, and its silence would mean nothing.
        //
        // Driven through the PRODUCTION conversion, not this module's `ticks` helper. An
        // earlier form used the helper on both sides, so it agreed with itself and said
        // nothing about the quantization that the live run then found.
        let offset = 0x0000_0100_0000_0000u64;
        for khz in RATES_KHZ {
            for clock_ns in [4_000_000u64, 4_000_001, 17_807_900, 999_999_999] {
                for error_ns in [0, 11_000] {
                    let requested = guest_tsc_lead_ns(error_ns);
                    let guest_tsc = guest_tsc_ticks_from_ns(clock_ns, khz)
                        - guest_tsc_ticks_from_ns(900_000, khz);
                    let new = tsc_offset_aligned_to_reference_clock(
                        offset, guest_tsc, clock_ns, khz, requested,
                    );
                    let measured = guest_tsc_lead_from_reference_clock(
                        corrected_counter(offset, guest_tsc, new),
                        clock_ns,
                        khz,
                    );
                    // Not an equality: the correction rounds the target UP by up to a tick
                    // and the measurement renders the answer in whole nanoseconds, so the
                    // two agree to within what those steps can move, not exactly. What
                    // must hold is that the measurement never reports the correction as
                    // having undershot its request.
                    assert!(
                        measured >= requested as i64,
                        "khz={khz} clock_ns={clock_ns}: measured {measured} under \
                         requested {requested}",
                    );
                    assert_eq!(
                        classify_achieved_lead(measured, &tolerances(requested, error_ns, khz)),
                        LeadVerdict::AsIntended,
                        "khz={khz} clock_ns={clock_ns}: measured {measured}",
                    );
                }
            }
        }
    }

    #[test]
    fn the_target_counter_clears_the_clock_plus_the_lead_at_every_clock_value() {
        // The write half of the defect, isolated. Rounding the LEAD up is not enough: the
        // clock's own fractional tick is discarded by the same conversion, and the target
        // then sits under `clock + lead` by that fraction. Which clock values it happens
        // at depends on the rate, so the sweep is over both.
        //
        // Compared as exact rationals, never through either conversion.
        let mut piecewise_was_ever_short = false;
        for khz in RATES_KHZ {
            for clock_ns in [0u64, 1, 17_807_900, 4_000_000, 4_000_001, 999_999_999] {
                for lead in [0u64, 1, GUEST_TSC_LEAD_FLOOR_NS] {
                    let target = guest_tsc_ticks_at_reference_clock_plus_lead(clock_ns, lead, khz);
                    assert_counter_clears(target, clock_ns, lead, khz);

                    // The form this replaced, measured the same way. Recorded rather than
                    // asserted per case, because it is wrong only at some clock values -
                    // which is exactly why a single-value test passed while a third of
                    // live boots did not.
                    let piecewise = guest_tsc_ticks_from_ns(clock_ns, khz)
                        + (lead as u128 * khz as u128).div_ceil(1_000_000) as u64;
                    if (piecewise as u128) * 1_000_000
                        < (clock_ns as u128 + lead as u128) * khz as u128
                    {
                        piecewise_was_ever_short = true;
                    }
                }
            }
        }
        // The sweep's own negative control. If no case in it defeats the truncate-then-add
        // form, the cases are not exercising the quantization and everything above passes
        // vacuously.
        assert!(
            piecewise_was_ever_short,
            "the sweep contains no clock value the piecewise form gets wrong, so it \
             cannot show that rounding the sum is what fixes it",
        );
    }

    #[test]
    fn the_measured_lead_does_not_fall_under_the_floor_through_quantization() {
        // The defect the live run caught, as a test. The lead was converted to ticks by
        // truncation and measured back by converting two whole nanosecond values and
        // subtracting, so 20 us came back as 19999 ns on EVERY boot and the verification
        // correctly called it a shortfall - a false alarm, which is worse than no alarm
        // because it teaches a reader to ignore the real one. A minimum margin is
        // quantized upward, and the lead is measured in the counter's own units and
        // converted once.
        // Swept over the clock, not just the rate: whether the double conversion loses a
        // nanosecond depends on the fractional part of the clock's own tick equivalent, so
        // a single convenient clock value can make the two forms agree and prove nothing.
        // 17.8 ms is where the live partition sat when this was found.
        let cases = [2_701_609u32, 2_700_000, 1_999_999, 3_800_017]
            .into_iter()
            .flat_map(|khz| {
                [
                    17_807_900u64,
                    4_000_000,
                    4_000_001,
                    123_456_789,
                    999_999_999,
                ]
                .into_iter()
                .map(move |clock_ns| (khz, clock_ns))
            });
        for (khz, clock_ns) in cases {
            let offset = 0x0000_0100_0000_0000u64;
            let requested = guest_tsc_lead_ns(0);
            let guest_tsc = guest_tsc_ticks_from_ns(clock_ns, khz);
            let new =
                tsc_offset_aligned_to_reference_clock(offset, guest_tsc, clock_ns, khz, requested);
            let corrected = corrected_counter(offset, guest_tsc, new);
            let measured = guest_tsc_lead_from_reference_clock(corrected, clock_ns, khz);
            assert!(
                measured >= GUEST_TSC_LEAD_FLOOR_NS as i64,
                "khz={khz} clock_ns={clock_ns}: measured {measured} is under the floor",
            );
            assert_eq!(
                classify_achieved_lead(measured, &tolerances(requested, 0, khz)),
                LeadVerdict::AsIntended,
                "khz={khz} clock_ns={clock_ns}: measured {measured} vs requested {requested}",
            );
        }
    }

    #[test]
    fn the_floor_holds_when_the_verification_reads_the_clock_after_the_correction() {
        // The case the previous round missed, and the one the live host was failing on.
        // Every test above reads the SAME clock value the correction was computed from, so
        // the reference reading's own nanosecond quantization cancels and the verification
        // agrees with the write by construction. In the real sequence it does not: the
        // correction is computed from one `KVM_GET_CLOCK`, sixteen `KVM_VCPU_TSC_OFFSET`
        // writes follow, and the verification takes a SECOND reading tens of microseconds
        // later.
        //
        // Both readings are the same clock rendered in whole nanoseconds, so the span
        // between them is either whole nanosecond either side of the true one, depending
        // on where inside a nanosecond the first read fell. `extra_ns = 1` is that second
        // case, and it is the one that takes the measured lead under the floor - a
        // MEASUREMENT artifact, not a counter that moved, so it must not raise the alarm.
        let offset = 0x0000_0100_0000_0000u64;
        for khz in RATES_KHZ {
            for clock_ns in [17_807_900u64, 4_000_000, 4_000_001, 999_999_999] {
                let requested = guest_tsc_lead_ns(0);
                let guest_tsc = guest_tsc_ticks_from_ns(clock_ns, khz);
                let new = tsc_offset_aligned_to_reference_clock(
                    offset, guest_tsc, clock_ns, khz, requested,
                );
                let corrected = corrected_counter(offset, guest_tsc, new);
                // The counter advances by whole ticks; the clock reports the same span
                // rendered in whole nanoseconds. 270163 ticks is about 100 us at this
                // host's rate, the order of the real write-to-verify gap.
                for elapsed_ticks in [0u64, 1, 2, 3, 7, 100, 54_321, 270_163] {
                    for extra_ns in [0u64, 1] {
                        let seen_tsc = corrected.wrapping_add(elapsed_ticks);
                        let seen_clock =
                            clock_ns + ns_from_guest_tsc_ticks(elapsed_ticks, khz) + extra_ns;
                        let measured =
                            guest_tsc_lead_from_reference_clock(seen_tsc, seen_clock, khz);
                        assert_ne!(
                            classify_achieved_lead(measured, &tolerances(requested, 0, khz)),
                            LeadVerdict::BelowFloor,
                            "khz={khz} clock_ns={clock_ns} elapsed_ticks={elapsed_ticks} \
                             extra_ns={extra_ns}: measured {measured} raised the floor \
                             alarm, but the counter was never moved",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_real_shortfall_still_raises_the_floor_alarm() {
        // The other half, and the reason the allowance is derived rather than widened: it
        // must be small enough that the failure it exists to catch still fires. The storm
        // this whole alignment answers ran the counter 22 us BEHIND the clock, and a lead
        // one microsecond short of the floor is already a hundred times the resolution.
        for khz in RATES_KHZ {
            let against = tolerances(GUEST_TSC_LEAD_FLOOR_NS, 0, khz);
            assert!(
                against.resolution_ns < 1_000,
                "khz={khz}: a resolution of {} ns would swallow a real shortfall",
                against.resolution_ns,
            );
            for measured in [-22_080i64, 0, 1_000, GUEST_TSC_LEAD_FLOOR_NS as i64 - 1_000] {
                assert_eq!(
                    classify_achieved_lead(measured, &against),
                    LeadVerdict::BelowFloor,
                    "khz={khz}: {measured} ns must still be reported as under the floor",
                );
            }
        }
    }

    #[test]
    fn the_resolution_covers_both_whole_unit_conversions_and_the_tick() {
        // Derived, not picked, so it has to be checkable against the conversions it comes
        // from: one nanosecond for the reference clock being reported in whole
        // nanoseconds, one for the final ticks-to-nanoseconds truncation, and a whole
        // counter tick - which is a rounding at GHz rates and real nanoseconds below.
        for khz in RATES_KHZ {
            let resolution = lead_measurement_resolution_ns(khz);
            assert_eq!(
                resolution,
                2 + 1_000_000u64.div_ceil(khz as u64),
                "khz={khz}"
            );
            assert!(resolution >= 3, "khz={khz}");
        }
        // A counter slow enough for a tick to be worth real time gets a real allowance,
        // which a constant would not give it.
        assert_eq!(lead_measurement_resolution_ns(1_000), 2 + 1_000);
        // And a rate the kernel should never report does not divide by zero.
        assert_eq!(lead_measurement_resolution_ns(0), 2);
    }

    #[test]
    fn a_lead_within_its_measurement_error_is_as_intended() {
        let requested = guest_tsc_lead_ns(11_000);
        let against = tolerances(requested, 11_000, KHZ);
        assert_eq!(
            classify_achieved_lead(requested as i64, &against),
            LeadVerdict::AsIntended,
        );
        assert_eq!(
            classify_achieved_lead(requested as i64 + 11_000, &against),
            LeadVerdict::AsIntended,
        );
    }

    #[test]
    fn a_lead_past_its_measurement_error_is_reported_but_not_alarming() {
        // Overshoot costs late timers, nothing worse, so it is a different signal from a
        // shortfall - the caller logs it at a lower level, and it must not be collapsed
        // into the same verdict.
        let requested = guest_tsc_lead_ns(0);
        assert_eq!(
            classify_achieved_lead(requested as i64 + 5_000, &tolerances(requested, 0, KHZ)),
            LeadVerdict::OutsideBand,
        );
    }

    #[test]
    fn a_lead_under_the_floor_outranks_the_band() {
        // The verdict that matters. A counter under the floor can arm past-dated timers
        // whatever the request was, so it must not be reported as merely out-of-band even
        // when a generous band would cover it. The band is the wide allowance and the
        // resolution the narrow one; only the narrow one may reach the floor.
        let wide = LeadTolerances {
            requested_ns: 30_000,
            band_ns: 60_000,
            resolution_ns: lead_measurement_resolution_ns(KHZ),
        };
        assert_eq!(
            classify_achieved_lead(GUEST_TSC_LEAD_FLOOR_NS as i64 - 1_000, &wide),
            LeadVerdict::BelowFloor,
        );
        assert_eq!(
            classify_achieved_lead(-22_080, &wide),
            LeadVerdict::BelowFloor,
        );
        assert_eq!(
            classify_achieved_lead(
                GUEST_TSC_LEAD_FLOOR_NS as i64,
                &tolerances(GUEST_TSC_LEAD_FLOOR_NS, 0, KHZ)
            ),
            LeadVerdict::AsIntended,
        );
    }

    #[test]
    fn a_negative_lead_is_under_the_floor_however_coarse_the_instrument() {
        // The counter behind the clock is the failure the floor exists to catch, so the
        // sign has to decide the verdict on its own rather than have an allowance added
        // to it first. A resolution wider than the floor is what separates the two: added
        // to a small negative lead it clears the floor arithmetically, and the band
        // comparison that follows reads the negative through an unsigned cast, so the
        // dangerous direction would be reported as a harmless overshoot.
        let coarse = LeadTolerances {
            requested_ns: GUEST_TSC_LEAD_FLOOR_NS,
            band_ns: GUEST_TSC_LEAD_FLOOR_NS,
            resolution_ns: GUEST_TSC_LEAD_FLOOR_NS + 5_000,
        };
        for measured in [-1i64, -5_000, -22_080, i64::MIN] {
            assert_eq!(
                classify_achieved_lead(measured, &coarse),
                LeadVerdict::BelowFloor,
                "{measured} ns is the counter trailing the clock",
            );
        }
        // And at a resolution the caller would really derive, where the floor test
        // happens to catch the same values, so the two agree rather than one covering up
        // for the other.
        for khz in RATES_KHZ {
            assert_eq!(
                classify_achieved_lead(-1, &tolerances(GUEST_TSC_LEAD_FLOOR_NS, 0, khz)),
                LeadVerdict::BelowFloor,
                "khz={khz}",
            );
        }
    }

    #[test]
    fn a_zero_tsc_rate_would_put_the_counter_on_a_meaningless_origin() {
        // Why the caller refuses an unusable rate at the entry point instead of letting
        // the conversions absorb it. Each leaf special-cases zero so it cannot divide by
        // it, which keeps them all DEFINED - and none of that keeps the answer MEANINGFUL.
        // The rate is not reachable through the ioctl wrapper without a live vcpu, so what
        // is checkable here is the arithmetic the guard stands in front of.
        let clock_ns = 900_000;
        let offset = 0x1234_5678_9abc_def0;
        let guest_tsc = 0x0fed_cba9_8765_4321;
        // The target collapses to zero ticks whatever the clock reads, so the offset that
        // gets written to every vp carries no relation to the reference clock at all.
        assert_eq!(
            guest_tsc_ticks_at_reference_clock_plus_lead(clock_ns, lead_ns(), 0),
            0,
        );
        assert_eq!(
            tsc_offset_aligned_to_reference_clock(offset, guest_tsc, clock_ns, 0, lead_ns()),
            offset.wrapping_sub(guest_tsc),
        );
        // The verification does notice - it reads the lead as zero for any counter, and
        // zero is under the floor - but only after the write. Noticing is not undoing,
        // which is what makes this worse than not aligning at all.
        assert_eq!(
            guest_tsc_lead_from_reference_clock(guest_tsc, clock_ns, 0),
            0
        );
        assert_eq!(
            classify_achieved_lead(0, &tolerances(lead_ns(), 0, KHZ)),
            LeadVerdict::BelowFloor,
        );
    }

    #[test]
    fn nanoseconds_and_ticks_round_trip_at_a_long_uptime() {
        // The reverse conversion is on the verification path, where the input is a whole
        // counter rather than a short duration, so it is the one that meets the big
        // numbers first.
        let ns = 365 * 24 * 60 * 60 * 1_000_000_000u64;
        assert_eq!(
            ns_from_guest_tsc_ticks(guest_tsc_ticks_from_ns(ns, KHZ), KHZ),
            ns
        );
        // And a rate the kernel should never report does not divide by zero.
        assert_eq!(ns_from_guest_tsc_ticks(1_000, 0), 0);
    }
}
