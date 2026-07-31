// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AMD SEV-SNP support for the x86_64 MSHV backend.

use super::*;

pub(super) const SNP_IMPORT_CHUNK_PAGES: usize = 256;

pub(super) const SNP_HYPERV_CPUID_FUNCTIONS: [u32; 10] = [
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

#[derive(inspect::Inspect)]
pub(crate) struct SnpPartitionState {
    /// Launch progress is synchronized because loading and memory mapping use
    /// shared partition references.
    pub(crate) launch_state: Mutex<SnpLaunchState>,
    /// BSP SEV features used to validate GHCB AP-creation requests.
    ///
    /// AMD GHCB specification 56421, "SNP AP Creation", supplies the AP VMSA's
    /// SEV_FEATURES in RAX and requires them to match the requesting vCPU. Each
    /// created AP is validated against this BSP value, making it the canonical
    /// feature set for subsequent requests.
    pub(super) sev_features: Mutex<Option<u64>>,
    pub(super) cpuid_offloads_enabled: bool,
}

impl SnpPartitionState {
    pub(super) fn new(disable_cpuid_offload: bool) -> Self {
        Self {
            launch_state: Mutex::new(SnpLaunchState::NotStarted),
            sev_features: Mutex::new(None),
            cpuid_offloads_enabled: !disable_cpuid_offload,
        }
    }
}

pub(crate) struct SnpVpState {
    ghcb_page: MshvGhcbPage,
}

impl SnpVpState {
    pub(super) fn new(vcpufd: &VcpuFd) -> Result<Self, Error> {
        // SAFETY: The VP fd owns a kernel GHCB state page at this documented
        // mmap offset for encrypted VPs when the target kernel advertises support.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                hvdef::HV_PAGE_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                vcpufd.as_raw_fd(),
                i64::from(mshv_bindings::MSHV_VP_MMAP_OFFSET_GHCB)
                    * libc::sysconf(libc::_SC_PAGE_SIZE),
            )
        };
        if page == libc::MAP_FAILED {
            return Err(ErrorInner::MapGhcbPage(std::io::Error::last_os_error()).into());
        }
        Ok(Self {
            ghcb_page: MshvGhcbPage(page.cast()),
        })
    }

    pub(super) fn page_ptr(&mut self) -> *mut x86defs::snp::GhcbPage {
        self.ghcb_page.0
    }
}

struct MshvGhcbPage(*mut x86defs::snp::GhcbPage);

// SAFETY: The mapping is uniquely owned by the processor binder and is only
// accessed while the binder is mutably borrowed by a bound processor.
unsafe impl Send for MshvGhcbPage {}

impl Drop for MshvGhcbPage {
    fn drop(&mut self) {
        // SAFETY: The pointer was returned by mmap for exactly one page and is
        // unmapped exactly once here.
        unsafe {
            libc::munmap(self.0.cast(), hvdef::HV_PAGE_SIZE as usize);
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub(super) struct ImportIsolatedPagesHeader {
    pub(super) page_type: u8,
    pub(super) rsvd: [u8; 7],
    pub(super) page_count: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub(super) struct ModifyGpaHostAccessHeader {
    pub(super) flags: u8,
    pub(super) rsvd: [u8; 7],
    pub(super) page_count: u64,
}

pub(super) const GHCB_RAX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rax) / size_of::<u64>());
pub(super) const GHCB_RBX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rbx) / size_of::<u64>() - 64);
pub(super) const GHCB_RCX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rcx) / size_of::<u64>() - 64);
pub(super) const GHCB_RDX_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, rdx) / size_of::<u64>() - 64);
pub(super) const GHCB_R8_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, r8) / size_of::<u64>() - 64);
pub(super) const GHCB_SW_EXIT_CODE_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_code) / size_of::<u64>() - 64);
pub(super) const GHCB_SW_EXIT_INFO1_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_info1) / size_of::<u64>() - 64);
pub(super) const GHCB_SW_EXIT_INFO2_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_exit_info2) / size_of::<u64>() - 64);
pub(super) const GHCB_SW_SCRATCH_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, sw_scratch) / size_of::<u64>() - 64);
pub(super) const GHCB_XCR0_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, xcr0) / size_of::<u64>() - 64);
pub(super) const GHCB_XSS_VALID_BIT: u64 =
    1 << (std::mem::offset_of!(x86defs::snp::GhcbSaveArea, xss) / size_of::<u64>());
pub(super) const SVM_EXITCODE_CPUID: u64 = 0x72;
pub(super) const GHCB_SHARED_BUFFER_OFFSET: u64 =
    std::mem::offset_of!(x86defs::snp::GhcbPage, shared_buffer) as u64;
pub(super) const SVM_NAE_SNP_AP_CREATE: u32 = 1;
pub(super) const GHCB_ERROR_RESPONSE: u64 = 2;
pub(super) const GHCB_ERROR_INVALID_INPUT: u64 = 5;
pub(super) const SNP_UNSAFE_VMSA_ALIGNMENT: u64 = 2 * 1024 * 1024;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) struct SnpApCreateRequest {
    pub(super) apic_id: u32,
    pub(super) vmsa_gpa: u64,
    pub(super) sev_features: u64,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum SnpApCreateRequestError {
    MissingInput,
    UnsupportedOperation(u16),
    UnsupportedVmpl(u16),
    InvalidSevFeatures(u64),
    InvalidVmsaGpa(u64),
}

pub(super) fn set_ghcb_rax(ghcb: &mut x86defs::snp::GhcbPage, rax: u64) {
    ghcb.save.rax = rax;
    ghcb.save.valid_bitmap0 |= GHCB_RAX_VALID_BIT;
}

pub(super) fn set_ghcb_gp(ghcb: &mut x86defs::snp::GhcbPage, index: usize, value: u64) -> bool {
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

pub(super) fn read_snp_start_vp_input(
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

pub(super) fn ghcb_rax_is_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap0 & GHCB_RAX_VALID_BIT != 0
}

pub(super) fn ghcb_exit_fields_are_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & (GHCB_SW_EXIT_CODE_VALID_BIT | GHCB_SW_EXIT_INFO1_VALID_BIT)
        == GHCB_SW_EXIT_CODE_VALID_BIT | GHCB_SW_EXIT_INFO1_VALID_BIT
}

pub(super) fn ghcb_mmio_fields_are_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & (GHCB_SW_EXIT_INFO2_VALID_BIT | GHCB_SW_SCRATCH_VALID_BIT)
        == GHCB_SW_EXIT_INFO2_VALID_BIT | GHCB_SW_SCRATCH_VALID_BIT
}

pub(super) fn ghcb_exit_info2_is_valid(ghcb: &x86defs::snp::GhcbPage) -> bool {
    ghcb.save.valid_bitmap1 & GHCB_SW_EXIT_INFO2_VALID_BIT != 0
}

pub(super) fn set_ghcb_error(ghcb: &mut x86defs::snp::GhcbPage, error: u64) {
    ghcb.save.sw_exit_info1 = GHCB_ERROR_RESPONSE;
    ghcb.save.sw_exit_info2 = error;
    ghcb.save.valid_bitmap1 |= GHCB_SW_EXIT_INFO1_VALID_BIT | GHCB_SW_EXIT_INFO2_VALID_BIT;
}

pub(super) fn parse_snp_ap_create_request(
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

pub(super) fn vp_index_for_apic_id(
    apic_id: u32,
    vps: impl IntoIterator<Item = (VpIndex, u32)>,
) -> Option<VpIndex> {
    vps.into_iter()
        .find_map(|(vp_index, candidate)| (candidate == apic_id).then_some(vp_index))
}

pub(super) fn snp_host_access_flags(visibility: u32) -> Option<u8> {
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

pub(crate) fn acquire_snp_host_access(
    partition: &MshvPartitionInner,
    addr: u64,
    size: u64,
) -> anyhow::Result<()> {
    // TODO: The current prototype implementation does not coordinate
    // acquisition with guest visibility changes. In particular, there is no
    // per-page state preventing a fault on another thread from acquiring
    // access while a GPA attribute intercept is revoking it. A complete
    // implementation must serialize acquisition with revocation and block or
    // fail this request if the guest is making the page private.
    anyhow::ensure!(
        addr.is_multiple_of(hvdef::HV_PAGE_SIZE)
            && size.is_multiple_of(hvdef::HV_PAGE_SIZE)
            && size != 0,
        "host-access range must be page aligned and nonempty"
    );
    let page_count = size / hvdef::HV_PAGE_SIZE;
    let flags = (1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_ACQUIRE)
        | (1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_READABLE)
        | (1 << mshv_bindings::MSHV_GPA_HOST_ACCESS_BIT_WRITABLE);
    let mut buf = HeaderVec::<ModifyGpaHostAccessHeader, u64, 0>::new(ModifyGpaHostAccessHeader {
        flags,
        rsvd: [0; 7],
        page_count,
    });
    let gpas = (0..page_count)
        .map(|page| addr + page * hvdef::HV_PAGE_SIZE)
        .collect::<Vec<_>>();
    buf.extend_tail_from_slice(&gpas);
    // SAFETY: The custom header matches `mshv_modify_gpa_host_access`,
    // followed by `page_count` contiguous GPA values.
    let args = unsafe {
        &*buf
            .as_ptr()
            .cast::<mshv_bindings::mshv_modify_gpa_host_access>()
    };
    partition.vmfd.modify_gpa_host_access(args)?;
    Ok(())
}

pub(super) fn parse_snp_gpa_range(
    range: hvdef::hypercall::HvGpaRange,
) -> Result<(u64, u64), VpHaltReason> {
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

pub(super) fn sanitize_snp_cpuid(
    function: u32,
    index: u32,
    expose_hypervisor: bool,
    values: &mut [u32; 4],
) {
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

pub(super) fn get_snp_cpuid_values(
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

pub(super) fn snp_cpuid_overrides(expose_hypervisor: bool) -> [virt::CpuidLeaf; 2] {
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

pub(super) fn add_snp_hyperv_cpuid_leaves(
    page: &mut x86defs::snp::HvPspCpuidPage,
) -> Result<(), usize> {
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

pub(super) fn snp_hv_cpuid_overrides(native_max_leaf: u32) -> [virt::CpuidLeaf; 3] {
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

pub(super) fn snp_start_vp_vmsa_gpa(
    context: &hvdef::hypercall::InitialVpContextX64,
) -> Option<u64> {
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

impl virt::AcceptInitialPages for MshvPartition {
    type Error = Error;

    fn accept_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
    }
}

impl MshvPartitionInner {
    fn snp_launch_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Error> {
        let Some(snp) = self.snp.as_ref() else {
            return Err(ErrorInner::IsolationNotSupported.into());
        };
        {
            let mut state = snp.launch_state.lock();
            match *state {
                SnpLaunchState::NotStarted => *state = SnpLaunchState::Started,
                SnpLaunchState::Started => return Err(ErrorInner::SnpLaunchInProgress.into()),
                SnpLaunchState::Finished => {
                    return Err(ErrorInner::SnpLaunchAlreadyFinished.into());
                }
                SnpLaunchState::Failed => return Err(ErrorInner::SnpLaunchFailed.into()),
            }
        }

        match self.snp_launch_initial_pages_inner(snp, pages) {
            Ok(()) => {
                *snp.launch_state.lock() = SnpLaunchState::Finished;
                Ok(())
            }
            Err(err) => {
                *snp.launch_state.lock() = SnpLaunchState::Failed;
                Err(err)
            }
        }
    }

    fn snp_launch_initial_pages_inner(
        &self,
        snp: &SnpPartitionState,
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
        *snp.sev_features.lock() = Some(vmsa.sev_features.into_bits());
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

impl MshvProcessor<'_> {
    /// Dispatches SNP exits that can be handled without a VP register page.
    pub(super) async fn handle_snp_exit(
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

    pub(super) fn modify_gpa_host_access(
        &self,
        gpas: &[u64],
        flags: u8,
    ) -> Result<(), VpHaltReason> {
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

    pub(super) fn handle_snp_gpa_attribute_intercept(
        &self,
        message: &HvMessage,
    ) -> Result<(), VpHaltReason> {
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

        // TODO: The current prototype implementation assumes that no
        // virtstack component is using these pages. Before revoking host
        // access, mark the ranges as revoking so that new GuestMemory faults
        // cannot reacquire them, then drain active GuestMemory accesses,
        // acquisitions already in progress, locked ranges, and device/DMA
        // users. Only after the release ioctl succeeds should the ranges be
        // marked private and the VP resumed, allowing the pending guest
        // visibility hypercall to be re-executed. If the accesses cannot be
        // drained, deny the intercept instead of reporting success.
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

    pub(super) fn sev_set_reg(
        &self,
        name: HvX64RegisterName,
        value: u64,
    ) -> Result<(), VpHaltReason> {
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

    pub(super) fn sev_get_reg(&self, name: HvX64RegisterName) -> Result<u64, VpHaltReason> {
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

    pub(super) async fn handle_sev_vmgexit_intercept(
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
                    self.partition
                        .snp
                        .as_ref()
                        .is_some_and(|snp| snp.cpuid_offloads_enabled),
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
    pub(super) async fn handle_sev_nae(
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
                let cpuid_offloads_enabled = self
                    .partition
                    .snp
                    .as_ref()
                    .is_some_and(|snp| snp.cpuid_offloads_enabled);
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

    pub(super) fn handle_snp_ap_create(
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
        let Some(snp) = self.partition.snp.as_ref() else {
            return Err(VpHaltReason::TripleFault { vtl: Vtl::Vtl0 });
        };
        let launch_sev_features = *snp.sev_features.lock();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

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
