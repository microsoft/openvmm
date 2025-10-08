// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common loader image loading traits and types used by all loaders.

#[cfg(guest_arch = "aarch64")]
pub use Aarch64Register as Register;
#[cfg(guest_arch = "x86_64")]
pub use X86Register as Register;

/// The page acceptance used for importing pages into the initial launch context
/// of the guest.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BootPageAcceptance {
    /// The page is accepted exclusive (no host visibility) and the page data is
    /// measured.
    Exclusive,
    /// The page is accepted exclusive (no host visibility) and the page data is
    /// unmeasured.
    ExclusiveUnmeasured,
    /// The page contains hardware-specific VP context information.
    VpContext,
    /// This page communicates error information to the host.
    ErrorPage,
    /// This page communicates hardware-specific secret information and the page
    /// data is unmeasured.
    SecretsPage,
    /// This page includes guest-specified CPUID information.
    CpuidPage,
    /// This page should include the enumeration of extended state CPUID leaves.
    CpuidExtendedStatePage,
    /// This page is host visible and contains valid data. The page has not been accepted.
    Shared,
}

/// The guest isolation type of the platform.
#[derive(Debug, PartialEq, Eq)]
pub enum IsolationType {
    /// No isolation is in use by this guest.
    None,
    /// This guest is isolated with VBS.
    Vbs,
    /// This guest is isolated with SNP (physical or emulated).
    Snp,
    /// This guest is isolated with TDX (physical or emulated).
    Tdx,
}

/// The startup memory type used to notify a well behaved host that memory
/// should be present before attempting to start the guest.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupMemoryType {
    /// The range is normal memory.
    Ram,
    /// The range is normal memory that additionally can have VTL2 protections
    /// applied by the guest.
    Vtl2ProtectableRam,
}

/// An x86 Table register, like GDTR.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TableRegister {
    pub base: u64,
    pub limit: u16,
}

impl From<igvm::registers::TableRegister> for TableRegister {
    fn from(value: igvm::registers::TableRegister) -> Self {
        Self {
            base: value.base,
            limit: value.limit,
        }
    }
}

impl From<TableRegister> for igvm::registers::TableRegister {
    fn from(value: TableRegister) -> Self {
        Self {
            base: value.base,
            limit: value.limit,
        }
    }
}

/// An x86 Segment Register, used for the segment selectors.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SegmentRegister {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attributes: u16,
}

impl From<igvm::registers::SegmentRegister> for SegmentRegister {
    fn from(value: igvm::registers::SegmentRegister) -> Self {
        Self {
            base: value.base,
            limit: value.limit,
            selector: value.selector,
            attributes: value.attributes,
        }
    }
}

impl From<SegmentRegister> for igvm::registers::SegmentRegister {
    fn from(value: SegmentRegister) -> Self {
        Self {
            base: value.base,
            limit: value.limit,
            selector: value.selector,
            attributes: value.attributes,
        }
    }
}

/// x86 registers that can be loaded via [ImageLoad::import_vp_register]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86Register {
    Gdtr(TableRegister),
    Idtr(TableRegister),
    Ds(SegmentRegister),
    Es(SegmentRegister),
    Fs(SegmentRegister),
    Gs(SegmentRegister),
    Ss(SegmentRegister),
    Cs(SegmentRegister),
    Tr(SegmentRegister),
    Cr0(u64),
    Cr3(u64),
    Cr4(u64),
    Efer(u64),
    Pat(u64),
    Rbp(u64),
    Rip(u64),
    Rsi(u64),
    Rsp(u64),
    R8(u64),
    R9(u64),
    R10(u64),
    R11(u64),
    R12(u64),
    Rflags(u64),
    MtrrDefType(u64),
    MtrrPhysBase0(u64),
    MtrrPhysMask0(u64),
    MtrrPhysBase1(u64),
    MtrrPhysMask1(u64),
    MtrrPhysBase2(u64),
    MtrrPhysMask2(u64),
    MtrrPhysBase3(u64),
    MtrrPhysMask3(u64),
    MtrrPhysBase4(u64),
    MtrrPhysMask4(u64),
    MtrrFix64k00000(u64),
    MtrrFix16k80000(u64),
    // We do not currently have a need for the middle fixed MTRRs.
    MtrrFix4kE0000(u64),
    MtrrFix4kE8000(u64),
    MtrrFix4kF0000(u64),
    MtrrFix4kF8000(u64),
}

impl From<igvm::registers::X86Register> for X86Register {
    fn from(value: igvm::registers::X86Register) -> Self {
        use igvm::registers::X86Register as igvm_reg;
        match value {
            igvm_reg::Gdtr(v) => X86Register::Gdtr(v.into()),
            igvm_reg::Idtr(v) => X86Register::Idtr(v.into()),
            igvm_reg::Ds(v) => X86Register::Ds(v.into()),
            igvm_reg::Es(v) => X86Register::Es(v.into()),
            igvm_reg::Fs(v) => X86Register::Fs(v.into()),
            igvm_reg::Gs(v) => X86Register::Gs(v.into()),
            igvm_reg::Ss(v) => X86Register::Ss(v.into()),
            igvm_reg::Cs(v) => X86Register::Cs(v.into()),
            igvm_reg::Tr(v) => X86Register::Tr(v.into()),
            igvm_reg::Cr0(v) => X86Register::Cr0(v),
            igvm_reg::Cr3(v) => X86Register::Cr3(v),
            igvm_reg::Cr4(v) => X86Register::Cr4(v),
            igvm_reg::Efer(v) => X86Register::Efer(v),
            igvm_reg::Pat(v) => X86Register::Pat(v),
            igvm_reg::Rbp(v) => X86Register::Rbp(v),
            igvm_reg::Rip(v) => X86Register::Rip(v),
            igvm_reg::Rsi(v) => X86Register::Rsi(v),
            igvm_reg::Rsp(v) => X86Register::Rsp(v),
            igvm_reg::R8(v) => X86Register::R8(v),
            igvm_reg::R9(v) => X86Register::R9(v),
            igvm_reg::R10(v) => X86Register::R10(v),
            igvm_reg::R11(v) => X86Register::R11(v),
            igvm_reg::R12(v) => X86Register::R12(v),
            igvm_reg::Rflags(v) => X86Register::Rflags(v),
            igvm_reg::MtrrDefType(v) => X86Register::MtrrDefType(v),
            igvm_reg::MtrrPhysBase0(v) => X86Register::MtrrPhysBase0(v),
            igvm_reg::MtrrPhysMask0(v) => X86Register::MtrrPhysMask0(v),
            igvm_reg::MtrrPhysBase1(v) => X86Register::MtrrPhysBase1(v),
            igvm_reg::MtrrPhysMask1(v) => X86Register::MtrrPhysMask1(v),
            igvm_reg::MtrrPhysBase2(v) => X86Register::MtrrPhysBase2(v),
            igvm_reg::MtrrPhysMask2(v) => X86Register::MtrrPhysMask2(v),
            igvm_reg::MtrrPhysBase3(v) => X86Register::MtrrPhysBase3(v),
            igvm_reg::MtrrPhysMask3(v) => X86Register::MtrrPhysMask3(v),
            igvm_reg::MtrrPhysBase4(v) => X86Register::MtrrPhysBase4(v),
            igvm_reg::MtrrPhysMask4(v) => X86Register::MtrrPhysMask4(v),
            igvm_reg::MtrrFix64k00000(v) => X86Register::MtrrFix64k00000(v),
            igvm_reg::MtrrFix16k80000(v) => X86Register::MtrrFix16k80000(v),
            igvm_reg::MtrrFix4kE0000(v) => X86Register::MtrrFix4kE0000(v),
            igvm_reg::MtrrFix4kE8000(v) => X86Register::MtrrFix4kE8000(v),
            igvm_reg::MtrrFix4kF0000(v) => X86Register::MtrrFix4kF0000(v),
            igvm_reg::MtrrFix4kF8000(v) => X86Register::MtrrFix4kF8000(v),
        }
    }
}

impl From<X86Register> for igvm::registers::X86Register {
    fn from(value: X86Register) -> Self {
        use igvm::registers::X86Register as igvm_reg;
        match value {
            X86Register::Gdtr(v) => igvm_reg::Gdtr(v.into()),
            X86Register::Idtr(v) => igvm_reg::Idtr(v.into()),
            X86Register::Ds(v) => igvm_reg::Ds(v.into()),
            X86Register::Es(v) => igvm_reg::Es(v.into()),
            X86Register::Fs(v) => igvm_reg::Fs(v.into()),
            X86Register::Gs(v) => igvm_reg::Gs(v.into()),
            X86Register::Ss(v) => igvm_reg::Ss(v.into()),
            X86Register::Cs(v) => igvm_reg::Cs(v.into()),
            X86Register::Tr(v) => igvm_reg::Tr(v.into()),
            X86Register::Cr0(v) => igvm_reg::Cr0(v),
            X86Register::Cr3(v) => igvm_reg::Cr3(v),
            X86Register::Cr4(v) => igvm_reg::Cr4(v),
            X86Register::Efer(v) => igvm_reg::Efer(v),
            X86Register::Pat(v) => igvm_reg::Pat(v),
            X86Register::Rbp(v) => igvm_reg::Rbp(v),
            X86Register::Rip(v) => igvm_reg::Rip(v),
            X86Register::Rsi(v) => igvm_reg::Rsi(v),
            X86Register::Rsp(v) => igvm_reg::Rsp(v),
            X86Register::R8(v) => igvm_reg::R8(v),
            X86Register::R9(v) => igvm_reg::R9(v),
            X86Register::R10(v) => igvm_reg::R10(v),
            X86Register::R11(v) => igvm_reg::R11(v),
            X86Register::R12(v) => igvm_reg::R12(v),
            X86Register::Rflags(v) => igvm_reg::Rflags(v),
            X86Register::MtrrDefType(v) => igvm_reg::MtrrDefType(v),
            X86Register::MtrrPhysBase0(v) => igvm_reg::MtrrPhysBase0(v),
            X86Register::MtrrPhysMask0(v) => igvm_reg::MtrrPhysMask0(v),
            X86Register::MtrrPhysBase1(v) => igvm_reg::MtrrPhysBase1(v),
            X86Register::MtrrPhysMask1(v) => igvm_reg::MtrrPhysMask1(v),
            X86Register::MtrrPhysBase2(v) => igvm_reg::MtrrPhysBase2(v),
            X86Register::MtrrPhysMask2(v) => igvm_reg::MtrrPhysMask2(v),
            X86Register::MtrrPhysBase3(v) => igvm_reg::MtrrPhysBase3(v),
            X86Register::MtrrPhysMask3(v) => igvm_reg::MtrrPhysMask3(v),
            X86Register::MtrrPhysBase4(v) => igvm_reg::MtrrPhysBase4(v),
            X86Register::MtrrPhysMask4(v) => igvm_reg::MtrrPhysMask4(v),
            X86Register::MtrrFix64k00000(v) => igvm_reg::MtrrFix64k00000(v),
            X86Register::MtrrFix16k80000(v) => igvm_reg::MtrrFix16k80000(v),
            X86Register::MtrrFix4kE0000(v) => igvm_reg::MtrrFix4kE0000(v),
            X86Register::MtrrFix4kE8000(v) => igvm_reg::MtrrFix4kE8000(v),
            X86Register::MtrrFix4kF0000(v) => igvm_reg::MtrrFix4kF0000(v),
            X86Register::MtrrFix4kF8000(v) => igvm_reg::MtrrFix4kF8000(v),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aarch64Register {
    Pc(u64),
    X0(u64),
    X1(u64),
    Cpsr(u64),
    VbarEl1(u64),
    Ttbr0El1(u64),
    Ttbr1El1(u64),
    MairEl1(u64),
    SctlrEl1(u64),
    TcrEl1(u64),
}

impl From<igvm::registers::AArch64Register> for Aarch64Register {
    fn from(value: igvm::registers::AArch64Register) -> Self {
        use igvm::registers::AArch64Register as igvm_reg;
        match value {
            igvm_reg::Pc(v) => Aarch64Register::Pc(v),
            igvm_reg::X0(v) => Aarch64Register::X0(v),
            igvm_reg::X1(v) => Aarch64Register::X1(v),
            igvm_reg::Cpsr(v) => Aarch64Register::Cpsr(v),
            igvm_reg::SctlrEl1(v) => Aarch64Register::SctlrEl1(v),
            igvm_reg::TcrEl1(v) => Aarch64Register::TcrEl1(v),
            igvm_reg::MairEl1(v) => Aarch64Register::MairEl1(v),
            igvm_reg::VbarEl1(v) => Aarch64Register::VbarEl1(v),
            igvm_reg::Ttbr0El1(v) => Aarch64Register::Ttbr0El1(v),
            igvm_reg::Ttbr1El1(v) => Aarch64Register::Ttbr1El1(v),
        }
    }
}

impl From<Aarch64Register> for igvm::registers::AArch64Register {
    fn from(value: Aarch64Register) -> Self {
        use igvm::registers::AArch64Register as igvm_reg;
        match value {
            Aarch64Register::Pc(v) => igvm_reg::Pc(v),
            Aarch64Register::X0(v) => igvm_reg::X0(v),
            Aarch64Register::X1(v) => igvm_reg::X1(v),
            Aarch64Register::Cpsr(v) => igvm_reg::Cpsr(v),
            Aarch64Register::SctlrEl1(v) => igvm_reg::SctlrEl1(v),
            Aarch64Register::TcrEl1(v) => igvm_reg::TcrEl1(v),
            Aarch64Register::MairEl1(v) => igvm_reg::MairEl1(v),
            Aarch64Register::VbarEl1(v) => igvm_reg::VbarEl1(v),
            Aarch64Register::Ttbr0El1(v) => igvm_reg::Ttbr0El1(v),
            Aarch64Register::Ttbr1El1(v) => igvm_reg::Ttbr1El1(v),
        }
    }
}

/// Isolation information returned by the importer to loaders.
#[derive(Debug)]
pub struct IsolationConfig {
    /// True if VTL2 is enabled, representing a paravisor present.
    pub paravisor_present: bool,

    /// The isolation type of the platform.
    pub isolation_type: IsolationType,

    /// If there is a shared gpa boundary, the number of bits.
    pub shared_gpa_boundary_bits: Option<u8>,
}

#[derive(Debug, Default)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl IsolationConfig {
    /// Get the isolation config in the format of a CPUID result.
    pub fn get_cpuid(&self) -> CpuidResult {
        // See HV_HYPERVISOR_ISOLATION_CONFIGURATION for format info.
        let eax = if self.paravisor_present { 1 } else { 0 };

        let mut ebx = 0;
        match self.isolation_type {
            IsolationType::None => {}
            IsolationType::Vbs => ebx = hvdef::HvPartitionIsolationType::VBS.0 as _,
            IsolationType::Snp => ebx = hvdef::HvPartitionIsolationType::SNP.0 as _,
            IsolationType::Tdx => ebx = hvdef::HvPartitionIsolationType::TDX.0 as _,
        }

        match self.shared_gpa_boundary_bits {
            None => {}
            Some(bits) => {
                ebx |= 1 << 5;
                ebx |= ((bits & 0x3F) as u32) << 6;
            }
        }

        CpuidResult {
            eax,
            ebx,
            ecx: 0,
            edx: 0,
        }
    }
}

#[derive(Debug)]
pub struct ImportRegion {
    pub page_base: u64,
    pub page_count: u64,
    pub acceptance: BootPageAcceptance,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PageRegion {
    pub page_base: u64,
    pub page_count: u64,
}

#[derive(Debug)]
pub enum IgvmParameterType {
    VpCount,
    Srat,
    Madt,
    MmioRanges,
    MemoryMap,
    CommandLine,
    Slit,
    Pptt,
    DeviceTree,
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct ParameterAreaIndex(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestArchKind {
    X86_64,
    Aarch64,
}

pub trait GuestArch {
    fn arch() -> GuestArchKind;
}

impl GuestArch for X86Register {
    fn arch() -> GuestArchKind {
        GuestArchKind::X86_64
    }
}

impl GuestArch for Aarch64Register {
    fn arch() -> GuestArchKind {
        GuestArchKind::Aarch64
    }
}

pub trait ImageLoad<R>
where
    R: GuestArch,
{
    /// Get the isolation configuration for this loader. This can be used by loaders
    /// to load different state depending on the platform.
    fn isolation_config(&self) -> IsolationConfig;

    /// Create a parameter area for the given page_base and page_count, which
    /// can be used to import parameters.
    ///
    /// `debug_tag` is a human readable string used by the loader to identify
    /// this region for debugging and reporting.
    fn create_parameter_area(
        &mut self,
        page_base: u64,
        page_count: u32,
        debug_tag: &str,
    ) -> anyhow::Result<ParameterAreaIndex>;

    /// Create a parameter area for the given page_base, page_count, and initial_data
    /// which can be used to import parameters.
    ///
    /// `debug_tag` is a human readable string used by the loader to identify
    /// this region for debugging and reporting.
    fn create_parameter_area_with_data(
        &mut self,
        page_base: u64,
        page_count: u32,
        debug_tag: &str,
        initial_data: &[u8],
    ) -> anyhow::Result<ParameterAreaIndex>;

    /// Import an IGVM parameter into the given parameter area index at the given offset.
    ///
    /// IGVM Parameters are used to specify where OS agnostic runtime dynamic information
    /// should be loaded into the guest memory space. This allows loaders to load a base IGVM
    /// file with a given measurement that can be specialized with runtime unmeasured parameters.
    fn import_parameter(
        &mut self,
        parameter_area: ParameterAreaIndex,
        byte_offset: u32,
        parameter_type: IgvmParameterType,
    ) -> anyhow::Result<()>;

    /// Import data into the guest address space with the given acceptance type.
    /// data.len() must be smaller than or equal to the number of pages being imported.
    ///
    /// `debug_tag` is a human readable string used by the loader to identify
    /// this region for debugging and reporting.
    fn import_pages(
        &mut self,
        page_base: u64,
        page_count: u64,
        debug_tag: &str,
        acceptance: BootPageAcceptance,
        data: &[u8],
    ) -> anyhow::Result<()>;

    /// Import a register into the BSP.
    fn import_vp_register(&mut self, register: R) -> anyhow::Result<()>;

    /// Verify with the loader that memory is available in guest address space with the given type.
    fn verify_startup_memory_available(
        &mut self,
        page_base: u64,
        page_count: u64,
        memory_type: StartupMemoryType,
    ) -> anyhow::Result<()>;

    /// Notify the loader to deposit architecture specific VP context information at the given page.
    ///
    /// TODO: It probably makes sense to use a different acceptance type than the default one?
    fn set_vp_context_page(&mut self, page_base: u64) -> anyhow::Result<()>;

    /// Specify this region as relocatable.
    fn relocation_region(
        &mut self,
        gpa: u64,
        size_bytes: u64,
        relocation_alignment: u64,
        minimum_relocation_gpa: u64,
        maximum_relocation_gpa: u64,
        apply_rip_offset: bool,
        apply_gdtr_offset: bool,
        vp_index: u16,
    ) -> anyhow::Result<()>;

    /// Specify a region as relocatable page table memory.
    fn page_table_relocation(
        &mut self,
        page_table_gpa: u64,
        size_pages: u64,
        used_pages: u64,
        vp_index: u16,
    ) -> anyhow::Result<()>;

    /// Lets the loader know what the base page of where the config page
    /// containing list of accepted regions should be. This list should contain
    /// the pages that will be accepted by the loader and therefore should not
    /// be accepted again by either the boot shim or the vtl 2 firmware. The
    /// list will be sorted in ascending order (on the base page) and be an
    /// array of non-overlapping
    /// [`loader_defs::paravisor::ImportedRegionDescriptor`]. A
    /// [`loader_defs::paravisor::ImportedRegionDescriptor`] with a page count
    /// of 0 indicates the end of the list.
    fn set_imported_regions_config_page(&mut self, page_base: u64);
}
