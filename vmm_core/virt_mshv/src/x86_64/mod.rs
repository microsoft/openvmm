// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific implementation of the mshv hypervisor backend.

mod vm_state;
mod vp_state;

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

use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use headervec::HeaderVec;
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
use memory_range::MemoryRange;
use mshv_ioctls::InterruptRequest;
use mshv_ioctls::VcpuFd;
use pal::unix::pthread::Pthread;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::os::fd::AsRawFd;
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
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const SNP_IMPORT_CHUNK_PAGES: usize = 256;

const SNP_HYPERV_CPUID_FUNCTIONS: [u32; 10] = [
    hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
    hvdef::HV_CPUID_FUNCTION_HV_INTERFACE,
    hvdef::HV_CPUID_FUNCTION_MS_HV_VERSION,
    hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES,
    hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION,
    hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS,
    hvdef::HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES,
    hvdef::HV_CPUID_FUNCTION_MS_HV_NESTED_FEATURES,
    hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION,
    hvdef::VIRTUALIZATION_STACK_CPUID_PROPERTIES,
];

#[derive(Debug, Copy, Clone, Eq, PartialEq, inspect::Inspect)]
pub(crate) enum SnpLaunchState {
    NotStarted,
    Started,
    Finished,
    Failed,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct ImportIsolatedPagesHeader {
    page_type: u8,
    rsvd: [u8; 7],
    page_count: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct ModifyGpaHostAccessHeader {
    flags: u8,
    rsvd: [u8; 7],
    page_count: u64,
}

const GHCB_RAX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rax) / size_of::<u64>());
const GHCB_RBX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rbx) / size_of::<u64>() - 64);
const GHCB_RCX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rcx) / size_of::<u64>() - 64);
const GHCB_RDX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rdx) / size_of::<u64>() - 64);
const GHCB_R8_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, r8) / size_of::<u64>() - 64);
const GHCB_SW_EXIT_CODE_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_code) / size_of::<u64>() - 64);
const GHCB_SW_EXIT_INFO1_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_info1) / size_of::<u64>() - 64);
const GHCB_SW_EXIT_INFO2_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_info2) / size_of::<u64>() - 64);
const GHCB_SW_SCRATCH_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_scratch) / size_of::<u64>() - 64);
const GHCB_XCR0_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, xcr0) / size_of::<u64>() - 64);
const GHCB_XSS_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, xss) / size_of::<u64>());
const SVM_EXITCODE_CPUID: u64 = 0x72;
const GHCB_SHARED_BUFFER_OFFSET: u64 =
    std::mem::offset_of!(x86defs::snp::GhcbPage, shared_buffer) as u64;
const SVM_NAE_SNP_AP_CREATE: u32 = 1;
const GHCB_ERROR_RESPONSE: u64 = 2;
const GHCB_ERROR_INVALID_INPUT: u64 = 5;
const SNP_UNSAFE_VMSA_ALIGNMENT: u64 = 2 * 1024 * 1024;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct SnpApCreateRequest {
    apic_id: u32,
    vmsa_gpa: u64,
    sev_features: u64,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum SnpApCreateRequestError {
    MissingInput,
    UnsupportedOperation(u16),
    UnsupportedVmpl(u16),
    InvalidSevFeatures(u64),
    InvalidVmsaGpa(u64),
}

fn set_ghcb_rax(ghcb: &mut x86defs::snp::GhcbPage, rax: u64) {
    ghcb.save.rax = rax;
    ghcb.save.valid_bitmap0 |= GHCB_RAX_VALID_BIT;
}

fn set_ghcb_gp(ghcb: &mut x86defs::snp::GhcbPage, index: usize, value: u64) -> bool {
    match index {
        index if index == x86emu::Gp::RAX as usize => set_ghcb_rax(ghcb, value),
        index if index == x86emu::Gp::RCX as usize => {
            ghcb.save.rcx = value;
            ghcb.save.valid_bitmap1 |= GHCB_RCX_VALID_BIT;
        }
        index if index == x86emu::Gp::RDX as usize => {
            ghcb.save.rdx = value;
            ghcb.save.valid_bitmap1 |= GHCB_RDX_VALID_BIT;
        }
        index if index == x86emu::Gp::R8 as usize => {
            ghcb.save.r8 = value;
            ghcb.save.valid_bitmap1 |= GHCB_R8_VALID_BIT;
        }
        _ => return false,
    }
    true
}

fn read_snp_start_vp_input(
    vcpufd: &VcpuFd,
    gpa: u64,
) -> Result<hvdef::hypercall::StartVirtualProcessorX64, mshv_ioctls::MshvError> {
    let mut data = [0; size_of::<hvdef::hypercall::StartVirtualProcessorX64>()];
    for (offset, chunk) in data.chunks_mut(16).enumerate() {
        let mut request = mshv_bindings::mshv_read_write_gpa {
            base_gpa: gpa + (offset * 16) as u64,
            byte_count: chunk.len() as u32,
            ..Default::default()
        };
        let result = vcpufd.gpa_read(&mut request)?;
        chunk.copy_from_slice(&result.data[..chunk.len()]);
    }
    Ok(
        hvdef::hypercall::StartVirtualProcessorX64::read_from_bytes(&data)
            .expect("buffer is exactly the StartVirtualProcessor input size"),
    )
}

fn ghcb_rax_is_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap0 & GHCB_RAX_VALID_BIT != 0
}

fn ghcb_exit_fields_are_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & (GHCB_SW_EXIT_CODE_VALID_BIT | GHCB_SW_EXIT_INFO1_VALID_BIT)
        == GHCB_SW_EXIT_CODE_VALID_BIT | GHCB_SW_EXIT_INFO1_VALID_BIT
}

fn ghcb_mmio_fields_are_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & (GHCB_SW_EXIT_INFO2_VALID_BIT | GHCB_SW_SCRATCH_VALID_BIT)
        == GHCB_SW_EXIT_INFO2_VALID_BIT | GHCB_SW_SCRATCH_VALID_BIT
}

fn ghcb_exit_info2_is_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & GHCB_SW_EXIT_INFO2_VALID_BIT != 0
}

fn set_ghcb_error(ghcb: &mut x86defs::snp::GhcbPage, error: u64) {
    ghcb.save.sw_exit_info1 = GHCB_ERROR_RESPONSE;
    ghcb.save.sw_exit_info2 = error;
    ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO1_VALID_BIT | GHCB_SW_EXIT_INFO2_VALID_BIT;
}

fn parse_snp_ap_create_request(
    ghcb: &x86defs::snp::GhcbPage,
) -> Result<SnpApCreateRequest, SnpApCreateRequestError> {
    let operation = ghcb.save.sw_exit_info1 as u16;
    if operation != SVM_NAE_SNP_AP_CREATE as u16 {
        // TODO: Implement CREATE_ON_INIT and DESTROY once the MSHV kernel ABI
        // provides the target-VP lifecycle operations needed for them.
        return Err(SnpApCreateRequestError::UnsupportedOperation(operation));
    }

    let vmpl = (ghcb.save.sw_exit_info1 >> 16) as u16;
    if vmpl != 0 {
        return Err(SnpApCreateRequestError::UnsupportedVmpl(vmpl));
    }

    if !ghcb_rax_is_valid(ghcb) || !ghcb_exit_info2_is_valid(ghcb) {
        return Err(SnpApCreateRequestError::MissingInput);
    }

    let sev_features = ghcb.save.rax;
    if sev_features & 1 == 0 {
        return Err(SnpApCreateRequestError::InvalidSevFeatures(sev_features));
    }

    let vmsa_gpa = ghcb.save.sw_exit_info2;
    // Conservatively mirror KVM's workaround for the SNP erratum where a
    // hugepage can collide with a 2 MiB-aligned VMSA RMP entry.
    if !vmsa_gpa.is_multiple_of(hvdef::HV_PAGE_SIZE)
        || vmsa_gpa.is_multiple_of(SNP_UNSAFE_VMSA_ALIGNMENT)
    {
        return Err(SnpApCreateRequestError::InvalidVmsaGpa(vmsa_gpa));
    }

    Ok(SnpApCreateRequest {
        apic_id: (ghcb.save.sw_exit_info1 >> 32) as u32,
        vmsa_gpa,
        sev_features,
    })
}

fn vp_index_for_apic_id(
    apic_id: u32,
    vps: impl IntoIterator<Item = (VpIndex, u32)>,
) -> Option<VpIndex> {
    vps.into_iter()
        .find_map(|(vp_index, candidate)| (candidate == apic_id).then_some(vp_index))
}

fn snp_host_access_flags(visibility: u32) -> Option<u8> {
    let acquire = 1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_ACQUIRE;
    let readable = 1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_READABLE;
    let writable = 1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_WRITABLE;
    match visibility {
        0 => Some(0),
        // The current MSHV kernel tests the readable flag when setting
        // writable access, so a read-only request would become read-write.
        1 => None,
        3 => Some(acquire | readable | writable),
        _ => None,
    }
}

fn parse_snp_gpa_range(range: hvdef::hypercall::HvGpaRange) -> Result<(u64, u64), VpHaltReason> {
    const PAGES_PER_2MB: u64 = 512;
    const PAGES_PER_1GB: u64 = 512 * PAGES_PER_2MB;

    let page = range.as_extended();
    let unit_count = page
        .additional_pages()
        .checked_add(1)
        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
    if !page.large_page() {
        return Ok((page.gpa_page_number(), unit_count));
    }

    let page = range.as_extended_large_page();
    let pages_per_unit = if page.page_size() {
        PAGES_PER_1GB
    } else {
        PAGES_PER_2MB
    };
    if page.page_size() && !page.gpa_large_page_number().is_multiple_of(PAGES_PER_2MB) {
        return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
    }
    let start_pfn = page
        .gpa_large_page_number()
        .checked_mul(PAGES_PER_2MB)
        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
    let page_count = unit_count
        .checked_mul(pages_per_unit)
        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
    Ok((start_pfn, page_count))
}

fn sanitize_snp_cpuid(function: u32, index: u32, expose_hypervisor: bool, values: &mut [u32; 4]) {
    if function == x86defs::cpuid::CpuidFunction::VersionAndFeatures.0 && !expose_hypervisor {
        values[2] &= !(1 << 31);
    }
    if function == x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0 && index == 1 {
        // TODO: Import a CPUID extended-state page and preserve supported XSS
        // components. The normal SNP CPUID page does not contain the required
        // component subleaves, so exposing this bitmap makes Linux consume
        // missing entries as zero-offset user state.
        values[2] = 0;
        values[3] = 0;
    }
}

fn get_snp_cpuid_values(
    vcpufd: &VcpuFd,
    function: u32,
    index: u32,
    xfem: u64,
    xss: u64,
    expose_hypervisor: bool,
) -> Result<[u32; 4], mshv_ioctls::MshvError> {
    let mut values = vcpufd.get_cpuid_values(function, index, xfem, xss)?;
    sanitize_snp_cpuid(function, index, expose_hypervisor, &mut values);
    Ok(values)
}

fn snp_cpuid_overrides(expose_hypervisor: bool) -> [virt::CpuidLeaf; 2] {
    [
        // Make the hypervisor-present bit match the guest contract.
        virt::CpuidLeaf::new(
            x86defs::cpuid::CpuidFunction::VersionAndFeatures.0,
            [0, 0, u32::from(expose_hypervisor) << 31, 0],
        )
        .masked([0, 0, 1 << 31, 0]),
        // Do not expose supervisor state without an SNP extended-state page.
        virt::CpuidLeaf::new(
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0,
            [0; 4],
        )
        .indexed(1)
        .masked([0, 0, u32::MAX, u32::MAX]),
    ]
}

fn add_snp_hyperv_cpuid_leaves(page: &mut x86defs::snp::HvPspCpuidPage) -> Result<(), usize> {
    let mut count = page.count as usize;
    if count > x86defs::snp::HV_PSP_CPUID_LEAF_COUNT_MAX {
        return Err(count);
    }

    for function in SNP_HYPERV_CPUID_FUNCTIONS {
        if page.cpuid_leaf_info[..count]
            .iter()
            .any(|leaf| leaf.eax_in == function && leaf.ecx_in == 0)
        {
            continue;
        }
        if count == x86defs::snp::HV_PSP_CPUID_LEAF_COUNT_MAX {
            return Err(count + 1);
        }

        page.cpuid_leaf_info[count].eax_in = function;
        count += 1;
    }
    page.count = count as u32;
    Ok(())
}

fn snp_hv_cpuid_overrides(native_max_leaf: u32) -> [virt::CpuidLeaf; 3] {
    const HV_ISOLATION_TYPE_SNP: u32 = 2;
    let privileges = hvdef::HvPartitionPrivilege::new()
        .with_start_virtual_processor(true)
        .with_isolation(true)
        .into_bits();
    let privilege_high = (privileges >> 32) as u32;

    // TODO: Investigate why this MSHV environment does not derive the Hyper-V
    // isolation CPUID contract from MSHV_PT_ISOLATION_SNP. Cloud Hypervisor
    // does not install an equivalent override, but we have not confirmed
    // whether its environment receives the isolation leaves correctly from
    // MSHV. Without these overrides, ACI Linux does not recognize a
    // non-paravisor Hyper-V SNP guest and uses the hypercall-page overlay
    // instead of direct VMMCALL hypercalls. Correctly describing isolation
    // also keeps ACI's restricted-injection doorbell EOI path active, making
    // the previous APIC-access recommendation mask unnecessary.
    [
        // Make the isolation configuration leaf discoverable.
        virt::CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
            [
                native_max_leaf.max(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION),
                0,
                0,
                0,
            ],
        )
        .masked([u32::MAX, 0, 0, 0]),
        // Tell the guest that this is an isolated Hyper-V partition.
        virt::CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES,
            [0, privilege_high, 0, 0],
        )
        .masked([0, privilege_high, 0, 0]),
        // Describe physical SNP without a paravisor or vTOM boundary.
        virt::CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION,
            [0, HV_ISOLATION_TYPE_SNP, 0, 0],
        )
        .masked([u32::MAX; 4]),
    ]
}

fn snp_start_vp_vmsa_gpa(context: &hvdef::hypercall::InitialVpContextX64) -> Option<u64> {
    let encoded = context.rip;
    // TODO: Confirm this ACI-specific overload is the intended Microsoft
    // Hypervisor SNP contract. ACI zeroes the nominal register context and
    // stores `vmsa_gpa | 1` in its first eight bytes for HvCallStartVP.
    if encoded & 1 == 0
        || context.as_bytes()[size_of::<u64>()..]
            .iter()
            .any(|&x| x != 0)
    {
        return None;
    }
    let vmsa_gpa = encoded & !1;
    (vmsa_gpa.is_multiple_of(hvdef::HV_PAGE_SIZE)
        && !vmsa_gpa.is_multiple_of(SNP_UNSAFE_VMSA_ALIGNMENT))
    .then_some(vmsa_gpa)
}

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
        let snp = match config.isolation.isolation_type() {
            virt::IsolationType::None => false,
            virt::IsolationType::Snp => true,
            _ => return Err(ErrorInner::IsolationNotSupported.into()),
        };
        let x2apic = matches!(
            config.processor_topology.apic_mode(),
            vm_topology::processor::x86::ApicMode::X2ApicSupported
                | vm_topology::processor::x86::ApicMode::X2ApicEnabled
        );
        let create_args =
            partition_create_args(snp, x2apic, config.processor_topology.smt_enabled());

        let vmfd = create_vm_with_retry(&self.mshv, &create_args)?;

        // Set synthetic processor features before initialization when the
        // guest interface is configured. SNP partitions require the smaller
        // early-property feature set accepted by the hypervisor.
        if config.hv_config.is_some() || snp {
            let synthetic_features = if snp {
                snp_synthetic_features()
            } else {
                common_synthetic_features()
                    .with_access_partition_reference_tsc(true)
                    .with_access_guest_idle_reg(true)
                    .with_access_frequency_regs(true)
                    .with_enable_extended_gva_ranges_for_flush_virtual_address_list(true)
            };

            vmfd.set_partition_property(
                HvPartitionPropertyCode::SyntheticProcFeatures.0,
                u64::from(synthetic_features),
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
        }

        vmfd.initialize()
            .map_err(|e| ErrorInner::CreateVMInitFailed(e.into()))?;

        if snp {
            let snp_policy = mshv_bindings::snp::get_default_snp_guest_policy();
            let vmgexit_offloads = snp_vmgexit_offloads(config.snp_disable_cpuid_offload);
            // SAFETY: These generated C unions always contain a valid u64 view.
            let (snp_policy, vmgexit_offloads) =
                unsafe { (snp_policy.as_uint64, vmgexit_offloads.as_uint64) };

            for (code, value) in [
                (HvPartitionPropertyCode::IsolationPolicy, snp_policy),
                (
                    HvPartitionPropertyCode::SevVmgexitOffloads,
                    vmgexit_offloads,
                ),
                (
                    HvPartitionPropertyCode::UnimplementedMsrAction,
                    mshv_bindings::hv_unimplemented_msr_action_HV_UNIMPLEMENTED_MSR_ACTION_IGNORE_WRITE_READ_ZERO
                        as u64,
                ),
                (HvPartitionPropertyCode::TimeFreeze, 1),
            ] {
                vmfd.set_partition_property(code.0, value)
                    .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;
            }
        }

        // Tell the hypervisor how many VPs are in each socket.
        vmfd.set_partition_property(
            HvPartitionPropertyCode::ProcessorsPerSocket.0,
            config.processor_topology.reserved_vps_per_socket() as u64,
        )
        .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        MshvProtoPartition::new(config, vmfd)
    }
}

fn partition_create_args(
    snp: bool,
    x2apic: bool,
    smt: bool,
) -> mshv_bindings::mshv_create_partition_v2 {
    let mut pt_flags =
        1 << mshv_bindings::MSHV_PT_BIT_LAPIC | 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES;

    if snp || x2apic {
        pt_flags |= 1 << mshv_bindings::MSHV_PT_BIT_X2APIC;
    }
    if smt {
        pt_flags |= 1 << mshv_bindings::MSHV_PT_BIT_SMT_ENABLED_GUEST;
    }

    mshv_bindings::mshv_create_partition_v2 {
        pt_flags: pt_flags | 1 << mshv_bindings::MSHV_PT_BIT_CPU_AND_XSAVE_FEATURES,
        pt_isolation: if snp {
            mshv_bindings::MSHV_PT_ISOLATION_SNP as u64
        } else {
            mshv_bindings::MSHV_PT_ISOLATION_NONE as u64
        },
        pt_num_cpu_fbanks: mshv_bindings::MSHV_NUM_CPU_FEATURES_BANKS as u16,
        pt_cpu_fbanks: [
            !u64::from(supported_processor_features()),
            !u64::from(supported_processor_features1()),
        ],
        pt_disabled_xsave: !u64::from(supported_xsave_features()),
        ..Default::default()
    }
}

fn snp_synthetic_features() -> hvdef::HvPartitionSyntheticProcessorFeatures {
    hvdef::HvPartitionSyntheticProcessorFeatures::new()
        .with_hypervisor_present(true)
        .with_hv1(true)
        .with_access_partition_reference_counter(true)
        .with_access_synic_regs(true)
        .with_access_synthetic_timer_regs(true)
        .with_access_partition_reference_tsc(true)
        .with_access_frequency_regs(true)
        .with_access_intr_ctrl_regs(true)
        .with_access_vp_index(true)
        .with_access_hypercall_regs(true)
        .with_access_guest_idle_reg(true)
        .with_tb_flush_hypercalls(true)
        .with_synthetic_cluster_ipi(true)
        .with_direct_synthetic_timers(true)
}

fn snp_vmgexit_offloads(disable_cpuid: bool) -> mshv_bindings::hv_sev_vmgexit_offload {
    let mut offloads = mshv_bindings::snp::get_default_vmgexit_offload_features();
    if disable_cpuid {
        // SAFETY: The generated accessors correspond to fields in the active
        // bitfield member of this C union.
        unsafe {
            offloads.__bindgen_anon_1.set_nae_cpuid(0);
            offloads.__bindgen_anon_1.set_msr_cpuid(0);
        }
    }
    offloads
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

        let reset_rdx = if self.config.isolation.isolation_type() == virt::IsolationType::Snp {
            0
        } else {
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
            physical_address_width: self
                .vmfd
                .get_partition_property(HvPartitionPropertyCode::PhysicalAddressWidth.0)
                .map_err(|e| ErrorInner::GetPartitionProperty(e.into()))?
                as u8,
            snp_c_bit: None,
            can_freeze_time: false,
            xsaves_state_bv_broken: false,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
            nested_virt: false,
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
        let isolation = self.config.isolation.isolation_type();
        let mut cpuid = config.cpuid.to_vec();
        if isolation == virt::IsolationType::Snp {
            let expose_hypervisor = self.config.hv_config.is_some();
            cpuid.extend(snp_cpuid_overrides(expose_hypervisor));
            if expose_hypervisor {
                let native_max_leaf = self
                    .bsp
                    .get_cpuid_values(hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION, 0, 0, 0)
                    .map(|values| values[0])
                    .unwrap_or(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION);
                cpuid.extend(snp_hv_cpuid_overrides(native_max_leaf));
            }
        }
        let cpuid = virt::CpuidLeafSet::new(cpuid);

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
            let mut cpuid_error = None;
            let cpuid_caps = virt::PartitionCapabilities::from_cpuid(
                self.config.processor_topology,
                &mut |function, index| {
                    self.bsp
                        .get_cpuid_values(function, index, 0, 0)
                        .unwrap_or_else(|err| {
                            cpuid_error.get_or_insert(err);
                            [0; 4]
                        })
                },
            );
            let mut caps = match (cpuid_caps, cpuid_error) {
                (Ok(caps), None) => caps,
                (result, error) => {
                    tracing::warn!(
                        error = error.as_ref().map(|err| err as &dyn std::error::Error),
                        capabilities_error = result
                            .err()
                            .as_ref()
                            .map(|err| err as &dyn std::error::Error),
                        "failed to query CPUID capabilities, falling back to partition properties; some features may be unavailable"
                    );
                    self.caps_from_properties()?
                }
            };
            caps.hv1 = self.config.hv_config.is_some();
            caps.hv1_reference_tsc_page = self.config.hv_config.is_some();
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
            software_devices: ApicSoftwareDevices::new(apic_id_map),
            snp_launch_state: Mutex::new(SnpLaunchState::NotStarted),
            snp_sev_features: Mutex::new(None),
            snp_cpuid_offloads_enabled: !self.config.snp_disable_cpuid_offload,
            isolation,
            // SNP partition creation set TimeFreeze=1 before this object was built.
            time_frozen: Mutex::new(isolation.is_isolated()),
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
                ghcb_page: None,
            })
            .collect();

        Ok((partition, vps))
    }
}

#[cfg(test)]
#[expect(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    #[test]
    fn can_disable_snp_cpuid_offloads() {
        let enabled = snp_vmgexit_offloads(false);
        let disabled = snp_vmgexit_offloads(true);

        // SAFETY: These generated C unions contain the bitfield member used by
        // get_default_vmgexit_offload_features() and snp_vmgexit_offloads().
        unsafe {
            assert_eq!(enabled.__bindgen_anon_1.nae_cpuid(), 1);
            assert_eq!(enabled.__bindgen_anon_1.msr_cpuid(), 1);
            assert_eq!(disabled.__bindgen_anon_1.nae_cpuid(), 0);
            assert_eq!(disabled.__bindgen_anon_1.msr_cpuid(), 0);
            assert_eq!(
                disabled.__bindgen_anon_1.nae_rdmsr(),
                enabled.__bindgen_anon_1.nae_rdmsr()
            );
        }
    }

    #[test]
    fn snp_partition_creation_uses_isolation_flags() {
        let args = partition_create_args(true, false, false);
        let pt_isolation = args.pt_isolation;
        let pt_num_cpu_fbanks = args.pt_num_cpu_fbanks;
        let pt_cpu_fbanks = args.pt_cpu_fbanks;
        let pt_disabled_xsave = args.pt_disabled_xsave;

        assert_eq!(pt_isolation, mshv_bindings::MSHV_PT_ISOLATION_SNP as u64);
        assert_ne!(args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_LAPIC, 0);
        assert_ne!(
            args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES,
            0
        );
        assert_ne!(args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_X2APIC, 0);
        assert_ne!(
            args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_CPU_AND_XSAVE_FEATURES,
            0
        );
        assert_eq!(
            pt_num_cpu_fbanks,
            mshv_bindings::MSHV_NUM_CPU_FEATURES_BANKS as u16
        );
        assert_eq!(
            pt_cpu_fbanks,
            [
                !u64::from(supported_processor_features()),
                !u64::from(supported_processor_features1()),
            ]
        );
        assert_eq!(pt_disabled_xsave, !u64::from(supported_xsave_features()));
    }

    #[test]
    fn snp_hv_cpuid_exposes_isolation() {
        let leaves = snp_hv_cpuid_overrides(0x40000010);
        assert_eq!(leaves[0].result[0], 0x40000010);
        let privileges = hvdef::HvPartitionPrivilege::from(
            u64::from(leaves[1].result[0]) | (u64::from(leaves[1].result[1]) << 32),
        );
        assert!(privileges.start_virtual_processor());
        assert!(privileges.isolation());
        assert_eq!(leaves[2].result, [0, 2, 0, 0]);
    }

    #[test]
    fn snp_cpuid_overrides_match_sanitization() {
        let hidden = snp_cpuid_overrides(false);
        assert_eq!(hidden[0].result[2], 0);
        assert_eq!(hidden[0].mask[2], 1 << 31);

        let exposed = snp_cpuid_overrides(true);
        assert_eq!(exposed[0].result[2], 1 << 31);
        assert_eq!(exposed[0].mask[2], 1 << 31);

        assert_eq!(
            exposed[1].function,
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
        );
        assert_eq!(exposed[1].index, Some(1));
        assert_eq!(exposed[1].result[2..], [0, 0]);
        assert_eq!(exposed[1].mask[2..], [u32::MAX, u32::MAX]);
    }

    #[test]
    fn adds_hyperv_leaves_to_snp_cpuid_page() {
        let mut page = x86defs::snp::HvPspCpuidPage::new_zeroed();
        page.count = 1;
        page.cpuid_leaf_info[0].eax_in = hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION;

        add_snp_hyperv_cpuid_leaves(&mut page).unwrap();

        assert_eq!(page.count as usize, SNP_HYPERV_CPUID_FUNCTIONS.len());
        for function in SNP_HYPERV_CPUID_FUNCTIONS {
            assert_eq!(
                page.cpuid_leaf_info[..page.count as usize]
                    .iter()
                    .filter(|leaf| leaf.eax_in == function && leaf.ecx_in == 0)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn parses_aci_snp_start_vp_context() {
        let mut context = hvdef::hypercall::InitialVpContextX64::new_zeroed();
        context.rip = 0x517001;
        assert_eq!(snp_start_vp_vmsa_gpa(&context), Some(0x517000));

        context.rflags = 2;
        assert_eq!(snp_start_vp_vmsa_gpa(&context), None);
        context.rflags = 0;
        context.rip = 0x517000;
        assert_eq!(snp_start_vp_vmsa_gpa(&context), None);
        context.rip = SNP_UNSAFE_VMSA_ALIGNMENT | 1;
        assert_eq!(snp_start_vp_vmsa_gpa(&context), None);
    }

    #[test]
    fn ordinary_partition_creation_keeps_feature_banks() {
        let args = partition_create_args(false, false, true);
        let pt_isolation = args.pt_isolation;
        let pt_num_cpu_fbanks = args.pt_num_cpu_fbanks;

        assert_eq!(pt_isolation, mshv_bindings::MSHV_PT_ISOLATION_NONE as u64);
        assert_ne!(
            args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_CPU_AND_XSAVE_FEATURES,
            0
        );
        assert_ne!(
            args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_SMT_ENABLED_GUEST,
            0
        );
        assert_eq!(args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_X2APIC, 0);
        assert_ne!(
            args.pt_flags & 1 << mshv_bindings::MSHV_PT_BIT_GPA_SUPER_PAGES,
            0
        );
        assert_eq!(
            pt_num_cpu_fbanks,
            mshv_bindings::MSHV_NUM_CPU_FEATURES_BANKS as u16
        );
    }

    #[test]
    fn maps_supported_snp_import_types() {
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::Normal).unwrap(),
            Some(mshv_bindings::MSHV_ISOLATED_PAGE_NORMAL as u8)
        );
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::NormalUnmeasured).unwrap(),
            Some(mshv_bindings::MSHV_ISOLATED_PAGE_UNMEASURED as u8)
        );
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::VpContext).unwrap(),
            Some(mshv_bindings::MSHV_ISOLATED_PAGE_VMSA as u8)
        );
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::Secrets).unwrap(),
            Some(mshv_bindings::MSHV_ISOLATED_PAGE_SECRETS as u8)
        );
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::Cpuid).unwrap(),
            Some(mshv_bindings::MSHV_ISOLATED_PAGE_CPUID as u8)
        );
        assert_eq!(
            snp_isolated_page_type(virt::InitialPageImportType::Shared).unwrap(),
            None
        );
        assert!(matches!(
            snp_isolated_page_type(virt::InitialPageImportType::CpuidExtendedState),
            Err(Error(ErrorInner::UnsupportedSnpPageImportType(
                virt::InitialPageImportType::CpuidExtendedState
            )))
        ));
    }

    #[test]
    fn validates_unique_snp_pages() {
        let pages = [
            virt::InitialPageImport {
                range: MemoryRange::new(0x1000..0x2000),
                import_type: virt::InitialPageImportType::VpContext,
                tag: "vmsa",
            },
            virt::InitialPageImport {
                range: MemoryRange::new(0x2000..0x3000),
                import_type: virt::InitialPageImportType::Cpuid,
                tag: "cpuid",
            },
        ];

        assert_eq!(snp_launch_pages(&pages).unwrap(), (0x1000, 0x2000));

        let duplicate = [pages[0].clone(), pages[0].clone(), pages[1].clone()];
        assert!(matches!(
            snp_launch_pages(&duplicate),
            Err(Error(ErrorInner::MultipleSnpVmsa))
        ));
    }

    #[test]
    fn rejects_invalid_snp_page_ranges() {
        assert!(matches!(
            validate_snp_page_range(MemoryRange::EMPTY),
            Err(Error(ErrorInner::InvalidSnpPageRange))
        ));

        let pages = [
            virt::InitialPageImport {
                range: MemoryRange::new(0x1000..0x3000),
                import_type: virt::InitialPageImportType::VpContext,
                tag: "vmsa",
            },
            virt::InitialPageImport {
                range: MemoryRange::new(0x3000..0x4000),
                import_type: virt::InitialPageImportType::Cpuid,
                tag: "cpuid",
            },
        ];
        assert!(matches!(
            snp_launch_pages(&pages),
            Err(Error(ErrorInner::InvalidSnpPageRange))
        ));
    }

    #[test]
    fn marks_ghcb_rax_valid() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();

        assert!(!ghcb_rax_is_valid(&ghcb));
        set_ghcb_rax(&mut ghcb, 0x1234_5678);

        assert_eq!(ghcb.save.rax, 0x1234_5678);
        assert!(ghcb_rax_is_valid(&ghcb));
    }

    #[test]
    fn validates_ghcb_exit_fields() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();

        assert!(!ghcb_exit_fields_are_valid(&ghcb));
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_CODE_VALID_BIT;
        assert!(!ghcb_exit_fields_are_valid(&ghcb));
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO1_VALID_BIT;
        assert!(ghcb_exit_fields_are_valid(&ghcb));
    }

    #[test]
    fn validates_ghcb_mmio_fields() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();

        assert!(!ghcb_mmio_fields_are_valid(&ghcb));
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO2_VALID_BIT;
        assert!(!ghcb_mmio_fields_are_valid(&ghcb));
        ghcb.save.valid_bitmap1 |= GHCB_SW_SCRATCH_VALID_BIT;
        assert!(ghcb_mmio_fields_are_valid(&ghcb));
    }

    #[test]
    fn validates_ghcb_exit_info2() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();

        assert!(!ghcb_exit_info2_is_valid(&ghcb));
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO2_VALID_BIT;
        assert!(ghcb_exit_info2_is_valid(&ghcb));
    }

    #[test]
    fn parses_snp_ap_create_requests() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();
        ghcb.save.sw_exit_info1 = (7u64 << 32) | u64::from(SVM_NAE_SNP_AP_CREATE);
        ghcb.save.sw_exit_info2 = 0x20_000;
        set_ghcb_rax(&mut ghcb, 9);
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO1_VALID_BIT | GHCB_SW_EXIT_INFO2_VALID_BIT;

        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Ok(SnpApCreateRequest {
                apic_id: 7,
                vmsa_gpa: 0x20_000,
                sev_features: 9,
            })
        );

        ghcb.save.sw_exit_info1 = 2;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::UnsupportedOperation(2))
        );

        ghcb.save.sw_exit_info1 = 0;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::UnsupportedOperation(0))
        );

        ghcb.save.sw_exit_info1 = (7u64 << 32) | (1 << 16) | u64::from(SVM_NAE_SNP_AP_CREATE);
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::UnsupportedVmpl(1))
        );

        ghcb.save.sw_exit_info1 = (7u64 << 32) | u64::from(SVM_NAE_SNP_AP_CREATE);
        ghcb.save.sw_exit_info2 = 0x20_001;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::InvalidVmsaGpa(0x20_001))
        );

        ghcb.save.sw_exit_info2 = SNP_UNSAFE_VMSA_ALIGNMENT;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::InvalidVmsaGpa(
                SNP_UNSAFE_VMSA_ALIGNMENT
            ))
        );

        ghcb.save.sw_exit_info2 = 0x20_000;
        ghcb.save.rax = 0;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::InvalidSevFeatures(0))
        );

        set_ghcb_rax(&mut ghcb, 9);
        ghcb.save.valid_bitmap1 &= !GHCB_SW_EXIT_INFO2_VALID_BIT;
        assert_eq!(
            parse_snp_ap_create_request(&ghcb),
            Err(SnpApCreateRequestError::MissingInput)
        );
    }

    #[test]
    fn maps_snp_apic_ids_to_vp_indices() {
        let vps = [(VpIndex::new(0), 0), (VpIndex::new(1), 4)];
        assert_eq!(vp_index_for_apic_id(4, vps), Some(VpIndex::new(1)));
        assert_eq!(vp_index_for_apic_id(3, vps), None);
    }

    #[test]
    fn encodes_ghcb_errors() {
        let mut ghcb = x86defs::snp::GhcbPage::new_zeroed();
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_CODE_VALID_BIT;
        set_ghcb_error(&mut ghcb, GHCB_ERROR_INVALID_INPUT);

        assert_eq!(ghcb.save.sw_exit_info1, GHCB_ERROR_RESPONSE);
        assert_eq!(ghcb.save.sw_exit_info2, GHCB_ERROR_INVALID_INPUT);
        assert!(ghcb_exit_fields_are_valid(&ghcb));
        assert!(ghcb_exit_info2_is_valid(&ghcb));
    }

    #[test]
    fn builds_snp_host_access_flags() {
        assert_eq!(snp_host_access_flags(0), Some(0));
        assert_eq!(snp_host_access_flags(1), None);
        assert_eq!(
            snp_host_access_flags(3),
            Some(
                1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_ACQUIRE
                    | 1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_READABLE
                    | 1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_WRITABLE
            )
        );
        assert_eq!(snp_host_access_flags(2), None);
    }

    #[test]
    fn parses_snp_gpa_ranges() {
        let mut range = hvdef::hypercall::HvGpaRange(
            hvdef::hypercall::HvGpaRangeExtended::new()
                .with_additional_pages(2)
                .with_gpa_page_number(0x1234)
                .into_bits(),
        );
        assert_eq!(parse_snp_gpa_range(range).unwrap(), (0x1234, 3));

        range = hvdef::hypercall::HvGpaRange(
            hvdef::hypercall::HvGpaRangeExtendedLargePage::new()
                .with_additional_pages(1)
                .with_large_page(true)
                .with_gpa_large_page_number(1)
                .into_bits(),
        );
        assert_eq!(parse_snp_gpa_range(range).unwrap(), (0x200, 1024));

        range = hvdef::hypercall::HvGpaRange(
            hvdef::hypercall::HvGpaRangeExtendedLargePage::new()
                .with_large_page(true)
                .with_page_size(true)
                .with_gpa_large_page_number(512)
                .into_bits(),
        );
        assert_eq!(parse_snp_gpa_range(range).unwrap(), (512 * 512, 512 * 512));

        range = hvdef::hypercall::HvGpaRange(
            hvdef::hypercall::HvGpaRangeExtendedLargePage::new()
                .with_large_page(true)
                .with_page_size(true)
                .with_gpa_large_page_number(1)
                .into_bits(),
        );
        assert!(parse_snp_gpa_range(range).is_err());
    }

    #[test]
    fn sanitizes_snp_cpuid() {
        let mut values = [0, 0, 1 << 31, 0];
        sanitize_snp_cpuid(
            x86defs::cpuid::CpuidFunction::VersionAndFeatures.0,
            0,
            false,
            &mut values,
        );
        assert_eq!(values[2], 0);

        let mut values = [0, 0, 1 << 31, 0];
        sanitize_snp_cpuid(
            x86defs::cpuid::CpuidFunction::VersionAndFeatures.0,
            0,
            true,
            &mut values,
        );
        assert_eq!(values[2], 1 << 31);

        let mut values = [0xb, 0x240, 0x1800, 1];
        sanitize_snp_cpuid(
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0,
            1,
            false,
            &mut values,
        );
        assert_eq!(values, [0xb, 0x240, 0, 0]);
    }
}

// ---------------------------------------------------------------------------
// Partition trait impls
// ---------------------------------------------------------------------------

impl virt::AcceptInitialPages for MshvPartition {
    type Error = Error;

    fn accept_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
    }
}

impl MshvPartitionInner {
    fn snp_launch_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Error> {
        {
            let mut state = self.snp_launch_state.lock();
            match *state {
                SnpLaunchState::NotStarted => *state = SnpLaunchState::Started,
                SnpLaunchState::Started => return Err(ErrorInner::SnpLaunchInProgress.into()),
                SnpLaunchState::Finished => {
                    return Err(ErrorInner::SnpLaunchAlreadyFinished.into());
                }
                SnpLaunchState::Failed => return Err(ErrorInner::SnpLaunchFailed.into()),
            }
        }

        match self.snp_launch_initial_pages_inner(pages) {
            Ok(()) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Finished;
                Ok(())
            }
            Err(err) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Failed;
                Err(err)
            }
        }
    }

    fn snp_launch_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), Error> {
        let (vmsa_gpa, cpuid_gpa) = snp_launch_pages(pages)?;
        let vmsa = self
            .gm
            .read_plain::<x86defs::snp::SevVmsa>(vmsa_gpa)
            .map_err(ErrorInner::SnpGuestMemory)?;
        // GHCB SNP AP-creation requests supply a new VMSA. Save the BSP launch
        // features so handle_snp_ap_create can require each AP VMSA to use the
        // same guest-visible SEV feature set.
        *self.snp_sev_features.lock() = Some(vmsa.sev_features.into_bits());
        self.write_snp_cpuid_page(cpuid_gpa)?;

        self.vmfd
            .set_partition_property(
                HvPartitionPropertyCode::IsolationState.0,
                mshv_bindings::hv_partition_isolation_state_HV_PARTITION_ISOLATION_SECURE as u64,
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        {
            let mut memory = self.memory.lock();
            for range in memory.ranges.iter_mut().flatten() {
                if !range.mapped {
                    self.vmfd
                        .map_user_memory(range.region)
                        .map_err(|e| ErrorInner::SnpMapGuestMemory(e.into()))?;
                    range.mapped = true;
                }
            }
        }

        let mut batch_type = None;
        let mut batch = Vec::with_capacity(SNP_IMPORT_CHUNK_PAGES);
        let mut sorted_pages = pages.to_vec();
        sorted_pages.sort_by_key(|page| page.range.start());
        for page in sorted_pages {
            let Some(page_type) = snp_isolated_page_type(page.import_type)? else {
                continue;
            };
            validate_snp_page_range(page.range)?;
            for pfn in
                page.range.start() / hvdef::HV_PAGE_SIZE..page.range.end() / hvdef::HV_PAGE_SIZE
            {
                if batch_type != Some(page_type) || batch.len() == SNP_IMPORT_CHUNK_PAGES {
                    if let Some(current_type) = batch_type {
                        self.import_isolated_pages(current_type, &batch)?;
                        batch.clear();
                    }
                    batch_type = Some(page_type);
                }
                batch.push(pfn);
            }
        }
        if let Some(page_type) = batch_type {
            self.import_isolated_pages(page_type, &batch)?;
        }

        self.complete_isolated_import()?;

        let sev_control =
            mshv_bindings::snp::get_sev_control_register(vmsa_gpa / hvdef::HV_PAGE_SIZE);
        self.bsp_vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((
                HvX64RegisterName::SevControl,
                sev_control,
            ))])
            .map_err(ErrorInner::Register)?;
        Ok(())
    }

    fn write_snp_cpuid_page(&self, cpuid_gpa: u64) -> Result<(), Error> {
        let mut page = self
            .gm
            .read_plain::<x86defs::snp::HvPspCpuidPage>(cpuid_gpa)
            .map_err(ErrorInner::SnpGuestMemory)?;
        let count = page.count as usize;
        if count > x86defs::snp::HV_PSP_CPUID_LEAF_COUNT_MAX {
            return Err(ErrorInner::TooManySnpCpuidEntries(count).into());
        }
        if self.caps.hv1 {
            // TODO: Determine the correct long-term strategy for exposing
            // synthetic Hyper-V CPUID leaves to direct-boot SNP guests: include
            // them in the measured CPUID page, rely on GHCB CPUID fallback, or
            // support both based on the guest contract.
            //
            // The loader creates this page with the architectural x86 and AMD
            // leaves needed by an SNP guest. Add the synthetic Hyper-V leaf
            // requests here because their values depend on the MSHV partition
            // configuration. The loop below queries MSHV for every requested
            // leaf and writes the results into this page before it is imported
            // through the SNP launch API. The PSP consequently measures these
            // synthetic values along with the rest of the CPUID page.
            //
            // This is not inherently required by the SNP or GHCB protocols. A
            // guest can treat a synthetic leaf absent from the measured table
            // as unsupported by that table and retry it through the GHCB CPUID
            // protocol. With CPUID VMGEXIT offloading enabled, the hypervisor
            // can answer that request using the CPUID intercept results
            // registered above. With offloading disabled, OpenVMM answers the
            // forwarded MSR or page-protocol request.
            //
            // Some Linux versions instead treat an absent, out-of-range
            // synthetic leaf as a successful all-zero result. In particular,
            // returning zeros for 0x40000000 prevents Hyper-V vendor detection
            // and therefore disables the entire Hyper-V guest interface. Keep
            // appending the leaves for compatibility with those guests. A
            // guest that returns -EOPNOTSUPP for missing synthetic leaves does
            // not require this augmentation.
            add_snp_hyperv_cpuid_leaves(&mut page).map_err(ErrorInner::TooManySnpCpuidEntries)?;
        }
        let count = page.count as usize;

        for leaf in &mut page.cpuid_leaf_info[..count] {
            let values = get_snp_cpuid_values(
                &self.bsp_vcpufd,
                leaf.eax_in,
                leaf.ecx_in,
                leaf.xfem_in,
                leaf.xss_in,
                self.caps.hv1,
            )
            .map_err(|e| ErrorInner::SnpCpuid(e.into()))?;
            leaf.eax_out = values[0];
            leaf.ebx_out = values[1];
            leaf.ecx_out = values[2];
            leaf.edx_out = values[3];
            leaf.reserved_z = 0;
        }
        self.gm
            .write_plain(cpuid_gpa, &page)
            .map_err(ErrorInner::SnpGuestMemory)?;
        Ok(())
    }

    fn import_isolated_pages(&self, page_type: u8, pfns: &[u64]) -> Result<(), Error> {
        if pfns.is_empty() {
            return Ok(());
        }

        let mut buf =
            HeaderVec::<ImportIsolatedPagesHeader, u64, 0>::new(ImportIsolatedPagesHeader {
                page_type,
                rsvd: [0; 7],
                page_count: pfns.len() as u64,
            });
        buf.extend_tail_from_slice(pfns);
        // SAFETY: The custom header has the same C layout as
        // `mshv_import_isolated_pages`, followed by `page_count` u64 PFNs.
        let args = unsafe {
            &*buf
                .as_ptr()
                .cast::<mshv_bindings::mshv_import_isolated_pages>()
        };
        self.vmfd
            .import_isolated_pages(args)
            .map_err(|e| ErrorInner::ImportIsolatedPages(e.into()))?;
        Ok(())
    }

    fn complete_isolated_import(&self) -> Result<(), Error> {
        let mut data = mshv_bindings::mshv_complete_isolated_import::default();
        data.import_data.psp_parameters.id_block.policy =
            mshv_bindings::snp::get_default_snp_guest_policy();
        data.import_data.psp_parameters.id_block_enabled = 0;
        data.import_data.psp_parameters.author_key_enabled = 0;
        self.vmfd
            .complete_isolated_import(&data)
            .map_err(|e| ErrorInner::CompleteIsolatedImport(e.into()))?;
        Ok(())
    }
}

/// Finds and validates the unique VMSA and CPUID pages needed for SNP launch.
fn snp_launch_pages(pages: &[virt::InitialPageImport]) -> Result<(u64, u64), Error> {
    let mut vmsa = None;
    let mut cpuid = None;
    for page in pages {
        let slot = match page.import_type {
            virt::InitialPageImportType::VpContext => &mut vmsa,
            virt::InitialPageImportType::Cpuid => &mut cpuid,
            _ => continue,
        };
        if slot.is_some() {
            return Err(match page.import_type {
                virt::InitialPageImportType::VpContext => ErrorInner::MultipleSnpVmsa,
                virt::InitialPageImportType::Cpuid => ErrorInner::MultipleSnpCpuid,
                _ => unreachable!(),
            }
            .into());
        }
        validate_snp_page_range(page.range)?;
        if page.range.len() != hvdef::HV_PAGE_SIZE {
            return Err(ErrorInner::InvalidSnpPageRange.into());
        }
        *slot = Some(page.range.start());
    }
    Ok((
        vmsa.ok_or(ErrorInner::MissingSnpVmsa)?,
        cpuid.ok_or(ErrorInner::MissingSnpCpuid)?,
    ))
}

fn validate_snp_page_range(range: MemoryRange) -> Result<(), Error> {
    if range.is_empty()
        || !range.start().is_multiple_of(hvdef::HV_PAGE_SIZE)
        || !range.end().is_multiple_of(hvdef::HV_PAGE_SIZE)
    {
        return Err(ErrorInner::InvalidSnpPageRange.into());
    }
    Ok(())
}

fn snp_isolated_page_type(import_type: virt::InitialPageImportType) -> Result<Option<u8>, Error> {
    Ok(Some(match import_type {
        virt::InitialPageImportType::Normal => mshv_bindings::MSHV_ISOLATED_PAGE_NORMAL as u8,
        virt::InitialPageImportType::NormalUnmeasured => {
            mshv_bindings::MSHV_ISOLATED_PAGE_UNMEASURED as u8
        }
        virt::InitialPageImportType::VpContext => mshv_bindings::MSHV_ISOLATED_PAGE_VMSA as u8,
        virt::InitialPageImportType::Secrets => mshv_bindings::MSHV_ISOLATED_PAGE_SECRETS as u8,
        virt::InitialPageImportType::Cpuid => mshv_bindings::MSHV_ISOLATED_PAGE_CPUID as u8,
        virt::InitialPageImportType::Shared => return Ok(None),
        virt::InitialPageImportType::CpuidExtendedState => {
            return Err(ErrorInner::UnsupportedSnpPageImportType(import_type).into());
        }
    }))
}

impl virt::Partition for MshvPartition {
    fn supports_initial_page_acceptance(
        &self,
    ) -> Option<&dyn virt::AcceptInitialPages<Error = Error>> {
        (self.inner.isolation == virt::IsolationType::Snp).then_some(self)
    }

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
        // TODO: Implement LINT injection for non-isolated MSHV partitions.
        //
        // The legacy PIC/PIT are temporarily attached for direct-boot TSC
        // calibration, but MSHV isolated VPs cannot receive PIC ExtINT through
        // LINT0. The guest must route runtime interrupts through the IOAPIC/MSI
        // path; PIC-dependent isolated guests are not supported.
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

    fn synic(&self) -> anyhow::Result<Arc<dyn vmcore::synic::SynicPortAccess>> {
        Ok(self.synic_ports.clone())
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

fn map_ghcb_page(vcpufd: &VcpuFd) -> Result<crate::MshvGhcbPage, Error> {
    // SAFETY: The VP fd owns a kernel GHCB state page at this documented mmap
    // offset for encrypted VPs when the target kernel advertises support.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            hvdef::HV_PAGE_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            vcpufd.as_raw_fd(),
            i64::from(mshv_bindings::MSHV_VP_MMAP_OFFSET_GHCB) * libc::sysconf(libc::_SC_PAGE_SIZE),
        )
    };
    if page == libc::MAP_FAILED {
        return Err(ErrorInner::MapGhcbPage(std::io::Error::last_os_error()).into());
    }
    Ok(crate::MshvGhcbPage(page.cast()))
}

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

        let reg_page_ptr = if self.partition.isolation == virt::IsolationType::Snp {
            None
        } else {
            Some(
                vcpufd
                    .get_vp_reg_page()
                    .ok_or(ErrorInner::MissingRegisterPage)?
                    .0
                    .cast::<HvX64RegisterPage>(),
            )
        };

        let runner = MshvVpRunner {
            vcpufd,
            reg_page: reg_page_ptr,
            ghcb_page: if self.partition.isolation == virt::IsolationType::Snp {
                if self.ghcb_page.is_none() {
                    self.ghcb_page = Some(map_ghcb_page(vcpufd)?);
                }
                self.ghcb_page.as_mut().map(|page| page.0)
            } else {
                None
            },
        };

        let this = MshvProcessor {
            partition: &self.partition,
            inner,
            vpindex: self.vpindex,
            runner,
            deliverability_notifications: HvDeliverabilityNotificationsRegister::new(),
        };

        if this.partition.isolation != virt::IsolationType::Snp {
            // Set the APIC state.
            let apic_base =
                virt::vp::Apic::at_reset(&this.partition.caps, &this.inner.vp_info).apic_base;

            let regs = &[
                HvRegisterAssoc::from((
                    HvX64RegisterName::InitialApicId,
                    u64::from(inner.vp_info.apic_id),
                )),
                HvRegisterAssoc::from((HvX64RegisterName::ApicBase, apic_base)),
                HvRegisterAssoc::from((
                    HvX64RegisterName::ApicId,
                    u64::from(inner.vp_info.apic_id),
                )),
            ];

            let reg_count = if this.partition.caps.x2apic { 2 } else { 3 };

            vcpufd
                .set_hvdef_regs(&regs[..reg_count])
                .map_err(ErrorInner::Register)?;
        }

        Ok(this)
    }
}

impl MshvProcessor<'_> {
    fn ensure_cpuid_intercept_expected(
        cpuid_offloads_enabled: bool,
        protocol: &'static str,
        function: u32,
        index: u32,
    ) -> Result<(), VpHaltReason> {
        if cpuid_offloads_enabled {
            tracelimit::warn_ratelimited!(
                protocol,
                function,
                index,
                "unexpected SNP CPUID intercept while CPUID offloads are enabled"
            );
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }
        Ok(())
    }

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
        if self.partition.isolation == virt::IsolationType::Snp {
            return self.handle_snp_exit(exit, dev).await;
        }

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
                self.handle_hypercall_intercept(exit, dev)?;
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

    /// Dispatches SNP exits that can be handled without a VP register page.
    async fn handle_snp_exit(
        &mut self,
        exit: &HvMessage,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        match exit.header.typ {
            HvMessageType::HvMessageTypeUnrecoverableException => {
                let info = exit.as_message::<hvdef::HvX64UnrecoverableExceptionMessage>();
                tracelimit::warn_ratelimited!(
                    rip = info.header.rip,
                    "SNP VP reported an unrecoverable exception"
                );
                Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })
            }
            HvMessageType::HvMessageTypeGpaAttributeIntercept => {
                self.handle_snp_gpa_attribute_intercept(exit)
            }
            HvMessageType::HvMessageTypeHypercallIntercept => {
                tracing::trace!("HYPERCALL_INTERCEPT");
                self.handle_hypercall_intercept(exit, dev)
            }
            HvMessageType::HvMessageTypeSynicSintDeliverable => {
                let info = exit.as_message::<hvdef::HvX64SynicSintDeliverableMessage>();
                self.handle_sint_deliverable(info.deliverable_sints);
                Ok(())
            }
            HvMessageType::HvMessageTypeX64ApicEoi => {
                let info = exit.as_message::<hvdef::HvX64ApicEoiMessage>();
                dev.handle_eoi(info.interrupt_vector);
                Ok(())
            }
            HvMessageType::HvMessageTypeX64SevVmgexitIntercept => {
                self.handle_sev_vmgexit_intercept(exit, dev).await
            }
            HvMessageType::HvMessageTypeUnacceptedGpa
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeGpaIntercept => {
                let info = exit.as_message::<hvdef::HvX64MemoryInterceptMessage>();
                let instruction = info
                    .instruction_bytes
                    .get(..info.instruction_byte_count as usize);
                tracelimit::warn_ratelimited!(
                    gpa = info.guest_physical_address,
                    gva = info.guest_virtual_address,
                    rip = info.header.rip,
                    access = ?info.header.intercept_access_type,
                    instruction_count = info.instruction_byte_count,
                    ?instruction,
                    exit_type = ?exit.header.typ,
                    "unexpected memory intercept for SNP VP"
                );
                Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })
            }
            exit_type => {
                tracelimit::warn_ratelimited!(
                    ?exit_type,
                    "unexpected non-VMGEXIT message for SNP VP"
                );
                Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })
            }
        }
    }

    fn modify_gpa_host_access(&self, gpas: &[u64], flags: u8) -> Result<(), VpHaltReason> {
        if gpas.is_empty() {
            return Ok(());
        }

        let mut buf =
            HeaderVec::<ModifyGpaHostAccessHeader, u64, 0>::new(ModifyGpaHostAccessHeader {
                flags,
                rsvd: [0; 7],
                page_count: gpas.len() as u64,
            });
        buf.extend_tail_from_slice(gpas);
        // SAFETY: The custom header matches `mshv_modify_gpa_host_access`
        // followed by `page_count` contiguous GPA values. Despite the UAPI
        // field name `guest_pfns`, the kernel converts each entry with
        // `HVPFN_DOWN`, so the variable array contains byte GPAs.
        let args = unsafe {
            &*buf
                .as_ptr()
                .cast::<mshv_bindings::mshv_modify_gpa_host_access>()
        };
        self.partition
            .vmfd
            .modify_gpa_host_access(args)
            .map_err(|err| {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    first_gpa = gpas[0],
                    page_count = gpas.len(),
                    flags,
                    "failed to modify SNP GPA host access"
                );
                VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
            })
    }

    fn handle_snp_gpa_attribute_intercept(&self, message: &HvMessage) -> Result<(), VpHaltReason> {
        const BATCH_PAGES: usize = 256;
        let info = message.as_message::<hvdef::HvX64GpaAttributeInterceptMessage>();
        let range_count = info.flags.range_count() as usize;
        let ranges = &info.ranges;
        if range_count == 0 || range_count > ranges.len() {
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }

        let flags = snp_host_access_flags(info.flags.host_visibility())
            .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
        if info.flags.adjust() || info.flags.memory_type() != 0 {
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }

        // TODO: Before revoking host access, block new GuestMemory accesses
        // to these pages and drain concurrent accesses and locked ranges.
        let mut gpas = Vec::with_capacity(BATCH_PAGES);
        for range in &ranges[..range_count] {
            let (start_pfn, page_count) = parse_snp_gpa_range(*range)?;
            let end_pfn = start_pfn
                .checked_add(page_count)
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;

            for pfn in start_pfn..end_pfn {
                gpas.push(
                    pfn.checked_mul(hvdef::HV_PAGE_SIZE)
                        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?,
                );
                if gpas.len() == BATCH_PAGES {
                    self.modify_gpa_host_access(&gpas, flags)?;
                    gpas.clear();
                }
            }
        }
        self.modify_gpa_host_access(&gpas, flags)
    }

    fn sev_set_reg(&self, name: HvX64RegisterName, value: u64) -> Result<(), VpHaltReason> {
        self.runner
            .vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((name, value))])
            .map_err(|err| {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    ?name,
                    "failed to set SNP VP register"
                );
                VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
            })
    }

    fn sev_get_reg(&self, name: HvX64RegisterName) -> Result<u64, VpHaltReason> {
        let mut assoc = [HvRegisterAssoc::from((name, 0u64))];
        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc)
            .map_err(|err| {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    ?name,
                    "failed to get SNP VP register"
                );
                VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
            })?;
        Ok(assoc[0].value.as_u64())
    }

    async fn handle_sev_vmgexit_intercept(
        &mut self,
        message: &HvMessage,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        use mshv_bindings::snp::*;

        let info = message.as_message::<hvdef::HvX64VmgexitInterceptMessage>();
        let ghcb_op = (info.ghcb_msr & GHCB_INFO_MASK as u64) as u32;
        let ghcb_data = info.ghcb_msr >> GHCB_INFO_BIT_WIDTH;
        tracing::trace!(
            ghcb_op,
            ghcb_data,
            ghcb_page_valid = info.flags.ghcb_page_valid(),
            "SNP VMGEXIT"
        );

        // CPUID requests reach userspace only when the corresponding VMGEXIT
        // offloads are disabled. The negotiation, registration, shutdown, and
        // normal page-protocol operations are always handled here.
        match ghcb_op {
            GHCB_INFO_SPECIAL_DBGPRINT => {}
            GHCB_INFO_HYP_FEATURE_REQUEST if ghcb_data == 0 => {
                let features = GHCB_HYP_FEATURE_SEV_SNP | GHCB_HYP_FEATURE_SEV_SNP_AP_CREATION;
                let response = GHCB_INFO_HYP_FEATURE_RESPONSE as u64
                    | u64::from(features) << GHCB_INFO_BIT_WIDTH;
                self.sev_set_reg(HvX64RegisterName::Ghcb, response)?;
            }
            GHCB_INFO_CPUID_REQUEST => {
                let function = (info.ghcb_msr >> 32) as u32;
                let register = ((info.ghcb_msr >> 30) & 3) as usize;
                Self::ensure_cpuid_intercept_expected(
                    self.partition.snp_cpuid_offloads_enabled,
                    "MSR",
                    function,
                    0,
                )?;
                let values = get_snp_cpuid_values(
                    self.runner.vcpufd,
                    function,
                    0,
                    0,
                    0,
                    self.partition.caps.hv1,
                )
                .map_err(|err| {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        function,
                        "failed to service SNP MSR CPUID request"
                    );
                    VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
                })?;
                let response = GHCB_INFO_CPUID_RESPONSE as u64 | u64::from(values[register]) << 32;
                self.sev_set_reg(HvX64RegisterName::Ghcb, response)?;
            }
            GHCB_INFO_SEV_INFO_REQUEST => {
                let values = self
                    .runner
                    .vcpufd
                    .get_cpuid_values(0x8000_001f, 0, 0, 0)
                    .map_err(|err| {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "failed to query SNP CPUID leaf"
                        );
                        VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
                    })?;
                let c_bit = u64::from(values[1] & 0x3f);
                let response = GHCB_INFO_SEV_INFO_RESPONSE as u64
                    | u64::from(GHCB_PROTOCOL_VERSION_MAX) << 48
                    | u64::from(GHCB_PROTOCOL_VERSION_MIN) << 32
                    | c_bit << 24;
                self.sev_set_reg(HvX64RegisterName::Ghcb, response)?;
            }
            GHCB_INFO_REGISTER_REQUEST => {
                let previous = self.sev_get_reg(HvX64RegisterName::SevGhcbGpa)?;
                let page_number = ghcb_data;
                let ghcb_gpa = page_number << GHCB_INFO_BIT_WIDTH;
                self.sev_set_reg(HvX64RegisterName::SevGhcbGpa, previous & !1)?;
                self.sev_set_reg(HvX64RegisterName::SevGhcbGpa, ghcb_gpa | 1)?;
                self.sev_set_reg(
                    HvX64RegisterName::Ghcb,
                    GHCB_INFO_REGISTER_RESPONSE as u64 | page_number << GHCB_INFO_BIT_WIDTH,
                )?;
            }
            GHCB_INFO_SHUTDOWN_REQUEST => {
                tracing::error!(ghcb_data, "SNP guest requested shutdown");
                return Err(VpHaltReason::PowerOff);
            }
            GHCB_INFO_NORMAL => {
                self.handle_sev_nae(info, ghcb_data, dev).await?;
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    ghcb_op,
                    ghcb_data,
                    "unsupported SNP GHCB MSR operation"
                );
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
        }

        Ok(())
    }

    /// Handles GHCB page-protocol non-automatic exits forwarded by MSHV.
    ///
    /// The GHCB is guest-writable shared memory, so validate its fields against
    /// the hypervisor-supplied intercept snapshot before using them.
    async fn handle_sev_nae(
        &mut self,
        info: &hvdef::HvX64VmgexitInterceptMessage,
        ghcb_pfn: u64,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        use mshv_bindings::snp::*;

        if !info.flags.ghcb_page_valid()
            || x86defs::snp::GhcbUsage(info.ghcb_page.ghcb_usage) != x86defs::snp::GhcbUsage::BASE
            || !(GHCB_PROTOCOL_VERSION_MIN..=GHCB_PROTOCOL_VERSION_MAX)
                .contains(&u32::from(info.ghcb_page.standard.ghcb_protocol_version))
        {
            tracelimit::warn_ratelimited!(
                ghcb_page_valid = info.flags.ghcb_page_valid(),
                ghcb_usage = info.ghcb_page.ghcb_usage,
                "invalid SNP GHCB page"
            );
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }

        let ghcb = self
            .runner
            .ghcb_page()
            .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
        if !ghcb_exit_fields_are_valid(ghcb)
            || ghcb.save.sw_exit_code != info.ghcb_page.standard.sw_exit_code
            || ghcb.save.sw_exit_info1 != info.ghcb_page.standard.sw_exit_info1
        {
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }

        let ghcb_gpa = ghcb_pfn
            .checked_mul(hvdef::HV_PAGE_SIZE)
            .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
        let registered = self.sev_get_reg(HvX64RegisterName::SevGhcbGpa)?;
        if registered & 1 == 0 || registered & !0xfff != ghcb_gpa {
            tracelimit::warn_ratelimited!(
                ghcb_gpa,
                registered,
                "SNP VMGEXIT used an unregistered GHCB page"
            );
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        }

        match info.ghcb_page.standard.sw_exit_code {
            SVM_EXITCODE_CPUID => {
                let cpuid_offloads_enabled = self.partition.snp_cpuid_offloads_enabled;
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if !ghcb_rax_is_valid(ghcb) || ghcb.save.valid_bitmap1 & GHCB_RCX_VALID_BIT == 0 {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }

                let function = ghcb.save.rax as u32;
                let index = ghcb.save.rcx as u32;
                Self::ensure_cpuid_intercept_expected(
                    cpuid_offloads_enabled,
                    "NAE",
                    function,
                    index,
                )?;
                let xfem = if ghcb.save.valid_bitmap1 & GHCB_XCR0_VALID_BIT != 0 {
                    ghcb.save.xcr0
                } else {
                    1
                };
                let xss = if ghcb.save.valid_bitmap0 & GHCB_XSS_VALID_BIT != 0 {
                    ghcb.save.xss
                } else {
                    0
                };
                let values = get_snp_cpuid_values(
                    self.runner.vcpufd,
                    function,
                    index,
                    xfem,
                    xss,
                    self.partition.caps.hv1,
                )
                .map_err(|err| {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        function,
                        index,
                        "failed to service SNP GHCB CPUID request"
                    );
                    VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }
                })?;

                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                set_ghcb_rax(ghcb, u64::from(values[0]));
                ghcb.save.rbx = u64::from(values[1]);
                ghcb.save.rcx = u64::from(values[2]);
                ghcb.save.rdx = u64::from(values[3]);
                ghcb.save.valid_bitmap1 |=
                    GHCB_RBX_VALID_BIT | GHCB_RCX_VALID_BIT | GHCB_RDX_VALID_BIT;
                ghcb.save.sw_exit_info1 = 0;
            }
            exit_code if exit_code == u64::from(SVM_EXITCODE_IOIO_PROT) => {
                let exit_info = u32::try_from(info.ghcb_page.standard.sw_exit_info1)
                    .map_err(|_| VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if exit_info & ((1 << 1) | (1 << 2) | (1 << 3) | (0x7 << 13)) != 0 {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }
                let len = match exit_info & 0x70 {
                    0x10 => 1,
                    0x20 => 2,
                    0x40 => 4,
                    _ => return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 }),
                };
                let port = (exit_info >> 16) as u16;
                let is_write = exit_info & 1 == 0;
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if is_write && !ghcb_rax_is_valid(ghcb) {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }
                let mut rax = ghcb.save.rax;
                virt_support_x86emu::emulate::emulate_io(
                    self.vpindex,
                    is_write,
                    port,
                    &mut rax,
                    len,
                    dev,
                )
                .await;
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if !is_write {
                    set_ghcb_rax(ghcb, rax);
                }
                ghcb.save.sw_exit_info1 = 0;
            }
            exit_code
                if exit_code == u64::from(SVM_EXITCODE_MMIO_READ)
                    || exit_code == u64::from(SVM_EXITCODE_MMIO_WRITE) =>
            {
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                let len = usize::try_from(info.ghcb_page.standard.sw_exit_info2)
                    .ok()
                    .filter(|len| matches!(len, 1 | 2 | 4 | 8))
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                let expected_scratch = ghcb_gpa
                    .checked_add(GHCB_SHARED_BUFFER_OFFSET)
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if !ghcb_mmio_fields_are_valid(ghcb)
                    || ghcb.save.sw_exit_info2 != info.ghcb_page.standard.sw_exit_info2
                    || ghcb.save.sw_scratch != info.ghcb_page.standard.sw_scratch
                    || info.ghcb_page.standard.sw_scratch != expected_scratch
                {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }

                let address = info.ghcb_page.standard.sw_exit_info1;
                address
                    .checked_add((len - 1) as u64)
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if exit_code == u64::from(SVM_EXITCODE_MMIO_READ) {
                    let mut data = [0; 8];
                    dev.read_mmio(self.vpindex, address, &mut data[..len]).await;
                    let ghcb = self
                        .runner
                        .ghcb_page()
                        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                    ghcb.shared_buffer[..len].copy_from_slice(&data[..len]);
                    ghcb.save.sw_exit_info1 = 0;
                } else {
                    let mut data = [0; 8];
                    data[..len].copy_from_slice(&ghcb.shared_buffer[..len]);
                    dev.write_mmio(self.vpindex, address, &data[..len]).await;
                    let ghcb = self
                        .runner
                        .ghcb_page()
                        .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                    ghcb.save.sw_exit_info1 = 0;
                }
            }
            exit_code if exit_code == u64::from(SVM_EXITCODE_HV_DOORBELL_PAGE) => {
                if info.ghcb_page.standard.sw_exit_info1 != u64::from(SVM_NAE_HV_DOORBELL_PAGE_SET)
                {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }

                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                let doorbell_gpa = info.ghcb_page.standard.sw_exit_info2;
                let doorbell_end = doorbell_gpa
                    .checked_add(hvdef::HV_PAGE_SIZE)
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                if !ghcb_exit_info2_is_valid(ghcb)
                    || ghcb.save.sw_exit_info2 != doorbell_gpa
                    || !doorbell_gpa.is_multiple_of(hvdef::HV_PAGE_SIZE)
                    || !self.partition.mem_layout.ram().iter().any(|range| {
                        range
                            .range
                            .contains(&MemoryRange::new(doorbell_gpa..doorbell_end))
                    })
                {
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }

                // Userspace does not maintain SNP page-visibility state yet.
                // Hyper-V validates that the GPA is suitable for use as a
                // doorbell page; propagate a rejected register write as a
                // fatal guest error.
                self.sev_set_reg(HvX64RegisterName::SevDoorbellGpa, doorbell_gpa | 1)?;
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                ghcb.save.sw_exit_info1 = 0;
            }
            exit_code if exit_code == u64::from(SVM_EXITCODE_SNP_AP_CREATION) => {
                self.handle_snp_ap_create(info, ghcb_gpa)?;
            }
            exit_code => {
                tracelimit::warn_ratelimited!(
                    exit_code,
                    sw_exit_info1 = info.ghcb_page.standard.sw_exit_info1,
                    sw_exit_info2 = info.ghcb_page.standard.sw_exit_info2,
                    sw_scratch = info.ghcb_page.standard.sw_scratch,
                    "unhandled SNP GHCB NAE"
                );
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
        }

        Ok(())
    }

    fn handle_snp_ap_create(
        &mut self,
        info: &hvdef::HvX64VmgexitInterceptMessage,
        ghcb_gpa: u64,
    ) -> Result<(), VpHaltReason> {
        let request = {
            let ghcb = self
                .runner
                .ghcb_page()
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
            if ghcb.save.sw_exit_info2 != info.ghcb_page.standard.sw_exit_info2 {
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
            match parse_snp_ap_create_request(ghcb) {
                Ok(request) => request,
                Err(error) => {
                    tracelimit::warn_ratelimited!(
                        ?error,
                        "rejected invalid SNP AP creation request"
                    );
                    set_ghcb_error(ghcb, GHCB_ERROR_INVALID_INPUT);
                    return Ok(());
                }
            }
        };
        let launch_sev_features = *self.partition.snp_sev_features.lock();
        if launch_sev_features != Some(request.sev_features) {
            tracelimit::warn_ratelimited!(
                sev_features = request.sev_features,
                ?launch_sev_features,
                "rejected SNP AP creation with mismatched SEV features"
            );
            let ghcb = self
                .runner
                .ghcb_page()
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
            set_ghcb_error(ghcb, GHCB_ERROR_INVALID_INPUT);
            return Ok(());
        }

        let target_vp = vp_index_for_apic_id(
            request.apic_id,
            self.partition
                .vps
                .iter()
                .map(|vp| (vp.vp_info.base.vp_index, vp.vp_info.apic_id)),
        );
        let vmsa_end = request.vmsa_gpa.checked_add(hvdef::HV_PAGE_SIZE);
        let valid_vmsa = vmsa_end.is_some_and(|end| {
            request.vmsa_gpa != ghcb_gpa
                && self.partition.mem_layout.ram().iter().any(|range| {
                    range
                        .range
                        .contains(&MemoryRange::new(request.vmsa_gpa..end))
                })
        });
        let target_vp = match target_vp {
            Some(target_vp) if valid_vmsa && !target_vp.is_bsp() && target_vp != self.vpindex => {
                target_vp
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    apic_id = request.apic_id,
                    vmsa_gpa = request.vmsa_gpa,
                    sev_features = request.sev_features,
                    ?target_vp,
                    "rejected invalid SNP AP creation target"
                );
                let ghcb = self
                    .runner
                    .ghcb_page()
                    .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
                set_ghcb_error(ghcb, GHCB_ERROR_INVALID_INPUT);
                return Ok(());
            }
        };
        tracing::trace!(
            target_vp = target_vp.index(),
            apic_id = request.apic_id,
            vmsa_gpa = request.vmsa_gpa,
            sev_features = request.sev_features,
            "creating SNP AP"
        );
        let request = mshv_bindings::mshv_sev_snp_ap_create {
            vp_id: u64::from(target_vp.index()),
            vmsa_gpa: request.vmsa_gpa,
        };
        if let Err(error) = self.partition.vmfd.sev_snp_ap_create(&request) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                target_vp = target_vp.index(),
                vmsa_gpa = request.vmsa_gpa,
                "failed to create SNP AP"
            );
            let ghcb = self
                .runner
                .ghcb_page()
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
            set_ghcb_error(ghcb, GHCB_ERROR_INVALID_INPUT);
            return Ok(());
        }

        let ghcb = self
            .runner
            .ghcb_page()
            .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
        ghcb.save.sw_exit_info1 = 0;
        ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO1_VALID_BIT;
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

    fn handle_hypercall_intercept(
        &mut self,
        message: &HvMessage,
        _devices: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let info = message.as_message::<hvdef::HvX64HypercallInterceptMessage>();
        let isolated = self.partition.isolation == virt::IsolationType::Snp;
        // SEV-SNP guests use the 64-bit direct VMMCALL ABI. MSHV redacts the
        // execution-state bits in the isolated hypercall intercept.
        let is_64bit = isolated
            || info.header.execution_state.cr0_pe() && info.header.execution_state.efer_lma();
        let vcpufd = self.runner.vcpufd;
        let mut isolated_regs = HvX64RegisterPage::new_zeroed();
        let reg_page = if isolated {
            let ghcb = self
                .runner
                .ghcb_page()
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
            if !ghcb_rax_is_valid(ghcb)
                || ghcb.save.valid_bitmap1 & GHCB_RCX_VALID_BIT == 0
                || ghcb.save.rax != info.rax
                || ghcb.save.rcx != info.rcx
                || ghcb.save.rdx != info.rdx
                || ghcb.save.r8 != info.r8
            {
                tracelimit::warn_ratelimited!("inconsistent SNP hypercall registers");
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
            isolated_regs.gp_registers[x86emu::Gp::RAX as usize] = info.rax;
            isolated_regs.gp_registers[x86emu::Gp::RBX as usize] = info.rbx;
            isolated_regs.gp_registers[x86emu::Gp::RCX as usize] = info.rcx;
            isolated_regs.gp_registers[x86emu::Gp::RDX as usize] = info.rdx;
            isolated_regs.gp_registers[x86emu::Gp::RSI as usize] = info.rsi;
            isolated_regs.gp_registers[x86emu::Gp::RDI as usize] = info.rdi;
            isolated_regs.gp_registers[x86emu::Gp::R8 as usize] = info.r8;
            for (dst, src) in isolated_regs.xmm.iter_mut().zip(&info.xmm_registers) {
                *dst = u128::from(*src);
            }
            &mut isolated_regs
        } else {
            self.runner.reg_page()
        };

        let (modified_gp, modified_xmm) = {
            let mut handler = MshvHypercallHandler {
                partition: self.partition,
                reg_page,
                caller_vp: self.vpindex,
                isolated,
                modified_gp: 0,
                modified_xmm: 0,
            };

            if isolated && info.rcx as u16 == hvdef::HypercallCode::HvCallStartVirtualProcessor.0 {
                let input_end = info
                    .rdx
                    .checked_add(size_of::<hvdef::hypercall::StartVirtualProcessorX64>() as u64);
                let result = input_end
                    .ok_or(hvdef::HvError::InvalidParameter)
                    .and_then(|_| {
                        read_snp_start_vp_input(vcpufd, info.rdx).map_err(|err| {
                            tracelimit::warn_ratelimited!(
                                error = &err as &dyn std::error::Error,
                                input_gpa = info.rdx,
                                "failed to read SNP StartVirtualProcessor input"
                            );
                            hvdef::HvError::InvalidParameter
                        })
                    })
                    .and_then(|input| {
                        if input.rsvd0 != 0 || input.rsvd1 != 0 {
                            return Err(hvdef::HvError::InvalidParameter);
                        }
                        hv1_hypercall::StartVirtualProcessor::start_virtual_processor(
                            &mut handler,
                            input.partition_id,
                            input.vp_index,
                            Vtl::try_from(input.target_vtl)?,
                            &input.vp_context,
                        )
                    });
                let output = match result {
                    Ok(()) => hvdef::hypercall::HypercallOutput::SUCCESS,
                    Err(err) => err.into(),
                };
                hv1_hypercall::X64RegisterState::set_gp(
                    &mut handler,
                    hv1_hypercall::X64HypercallRegister::Rax,
                    output.into(),
                );
            } else {
                MshvHypercallHandler::DISPATCHER.dispatch(
                    &self.partition.gm,
                    if isolated {
                        X64RegisterIo::new_without_ip_advance(&mut handler, is_64bit)
                    } else {
                        X64RegisterIo::new(&mut handler, is_64bit)
                    },
                );
            }
            (handler.modified_gp, handler.modified_xmm)
        };

        if isolated {
            if modified_xmm != 0 {
                tracelimit::warn_ratelimited!(modified_xmm, "unsupported SNP hypercall XMM output");
                return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
            }
            let ghcb = self
                .runner
                .ghcb_page()
                .ok_or(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 })?;
            for index in 0..isolated_regs.gp_registers.len() {
                if modified_gp & (1 << index) != 0
                    && !set_ghcb_gp(ghcb, index, isolated_regs.gp_registers[index])
                {
                    tracelimit::warn_ratelimited!(
                        index,
                        "unsupported SNP hypercall GP-register output"
                    );
                    return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
                }
            }
        }
        Ok(())
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
        let index = n as usize;
        self.reg_page.gp_registers[index] = value;
        if self.isolated {
            self.modified_gp |= 1 << index;
        } else {
            self.reg_page.dirty.set_general_purpose(true);
        }
    }

    fn xmm(&mut self, n: usize) -> u128 {
        self.reg_page.xmm[n]
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.reg_page.xmm[n] = value;
        if self.isolated {
            self.modified_xmm |= 1 << n;
        } else {
            self.reg_page.dirty.set_xmm(true);
        }
    }
}

pub(crate) struct MshvHypercallHandler<'a> {
    pub(crate) partition: &'a MshvPartitionInner,
    pub(crate) reg_page: &'a mut HvX64RegisterPage,
    pub(crate) caller_vp: VpIndex,
    isolated: bool,
    modified_gp: u16,
    modified_xmm: u8,
}

impl MshvHypercallHandler<'_> {
    const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvX64StartVirtualProcessor,
        ],
    );
}

impl hv1_hypercall::StartVirtualProcessor<hvdef::hypercall::InitialVpContextX64>
    for MshvHypercallHandler<'_>
{
    fn start_virtual_processor(
        &mut self,
        partition_id: u64,
        target_vp: u32,
        target_vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> hvdef::HvResult<()> {
        if self.partition.isolation != virt::IsolationType::Snp
            || partition_id != hvdef::HV_PARTITION_ID_SELF
        {
            return Err(hvdef::HvError::InvalidPartitionId);
        }
        if target_vtl != Vtl::Vtl0 {
            return Err(hvdef::HvError::InvalidParameter);
        }

        let target_vp = VpIndex::new(target_vp);
        if target_vp.is_bsp()
            || target_vp == self.caller_vp
            || target_vp.index() as usize >= self.partition.vps.len()
        {
            return Err(hvdef::HvError::InvalidVpIndex);
        }

        let vmsa_gpa = snp_start_vp_vmsa_gpa(vp_context).ok_or(hvdef::HvError::InvalidParameter)?;
        let vmsa_end = vmsa_gpa
            .checked_add(hvdef::HV_PAGE_SIZE)
            .ok_or(hvdef::HvError::InvalidParameter)?;
        if !self
            .partition
            .mem_layout
            .ram()
            .iter()
            .any(|range| range.range.contains(&MemoryRange::new(vmsa_gpa..vmsa_end)))
        {
            return Err(hvdef::HvError::InvalidParameter);
        }

        let request = mshv_bindings::mshv_sev_snp_ap_create {
            vp_id: u64::from(target_vp.index()),
            vmsa_gpa,
        };
        self.partition
            .vmfd
            .sev_snp_ap_create(&request)
            .map_err(|err| {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    target_vp = target_vp.index(),
                    vmsa_gpa,
                    "failed to handle SNP StartVirtualProcessor"
                );
                hvdef::HvError::InvalidVpState
            })
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
