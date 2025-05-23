// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SNP support for the bootshim.

use super::address_space::LocalMap;
use super::address_space::PAGE_TABLE_ENTRY_COUNT;
use super::address_space::X64_PAGE_SHIFT;
use super::address_space::X64_PTE_ACCESSED;
use super::address_space::X64_PTE_PRESENT;
use super::address_space::X64_PTE_READ_WRITE;
use crate::arch::x86_64::address_space::X64_PTE_CONFIDENTIAL;
use crate::single_threaded::SingleThreaded;
use bitfield_struct::bitfield;
use core::arch::asm;
use core::cell::Cell;
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;
use core::sync::atomic::fence;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HypercallOutput;
use memory_range::MemoryRange;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use x86defs::X64_PAGE_SIZE;
use x86defs::X86X_AMD_MSR_GHCB;
use x86defs::snp::GhcbInfo;
use x86defs::snp::GhcbMsr;
use x86defs::snp::GhcbProtocolVersion;
use x86defs::snp::GhcbUsage;
use x86defs::snp::SevExitCode;
use x86defs::snp::SevIoAccessInfo;
use zerocopy::IntoBytes;

static GHCB_PREVIOUS: SingleThreaded<Cell<u64>> = SingleThreaded(Cell::new(0));

pub struct Ghcb;

#[derive(Debug)]
pub enum AcceptGpaStatus {
    Success,
    Retry,
}

#[expect(dead_code)] // Printed via Debug in the error case.
#[derive(Debug)]
pub enum AcceptGpaError {
    MemorySecurityViolation {
        error_code: u32,
        carry_flag: u32,
        page_number: u64,
        large_page: bool,
        validate: bool,
    },
    Unknown,
}

struct GhcbCall {
    extra_data: u64,
    page_number: u64,
    info: GhcbInfo,
}

// The memory mapping bits likely don't belong to this module, but
// no centralized facility seems to exist for them yet.

/// 4-level virtual address. The number of bits used in the VA
/// ought to be requested through CPUID. Here it is "hardcoded"
/// to 48 bits, which is the most common case.
#[bitfield(u64)]
struct VirtAddr4Level {
    /// Offset inside the page.
    #[bits(12)]
    offset: usize,
    /// PT index.
    #[bits(9)]
    pt_index: usize,
    /// PD index.
    #[bits(9)]
    pd_index: usize,
    /// PDP index.
    #[bits(9)]
    pdp_index: usize,
    /// PML4 index.
    #[bits(9)]
    pml4_index: usize,
    /// Reserved bits.
    #[bits(16)]
    reserved: usize,
}

impl VirtAddr4Level {
    const fn canonicalize(&self) -> VirtAddr4Level {
        // If PML4 is greater than 255, make it upper-half canonical
        // by sign extending the PML4 index.
        Self::from_bits((self.into_bits().wrapping_shl(16) as i64).wrapping_shr(16) as u64)
    }
}

/// Page table.
#[repr(C, align(4096))]
struct PageTable {
    entries: [u64; PAGE_TABLE_ENTRY_COUNT],
}

// Would be great to allocate this pages dynamically as otherwise they go
// into the IGVM file and require measurement through the PSP.

/// PDP table to map the GHCB
static PDP_TABLE: SingleThreaded<UnsafeCell<PageTable>> =
    SingleThreaded(UnsafeCell::new(PageTable {
        entries: [0; PAGE_TABLE_ENTRY_COUNT],
    }));

/// PD table to map the GHCB
static PD_TABLE: SingleThreaded<UnsafeCell<PageTable>> =
    SingleThreaded(UnsafeCell::new(PageTable {
        entries: [0; PAGE_TABLE_ENTRY_COUNT],
    }));

/// Page table to map the GHCB
static PAGE_TABLE: SingleThreaded<UnsafeCell<PageTable>> =
    SingleThreaded(UnsafeCell::new(PageTable {
        entries: [0; PAGE_TABLE_ENTRY_COUNT],
    }));

const PML4_INDEX: usize = 0x1d0; // upper half mapping
const PDP_INDEX: usize = 0;
const PD_INDEX: usize = 0;
const PT_INDEX: usize = 0;
const GHCB_GVA: VirtAddr4Level = VirtAddr4Level::new()
    .with_pt_index(PT_INDEX)
    .with_pd_index(PD_INDEX)
    .with_pdp_index(PDP_INDEX)
    .with_pml4_index(PML4_INDEX)
    .canonicalize();

fn get_cr3() -> u64 {
    let mut cr3: u64;

    // SAFETY: No access to the memory.
    unsafe {
        asm!("mov {0}, cr3", out(reg) cr3, options(nostack));
    }
    cr3
}

fn cache_lines_flush_page(addr: u64) {
    const FLUSH_SIZE: u64 = 64; // NOTE: hardcoded cache line size.
    let start = addr & !(X64_PAGE_SIZE - 1);
    let end = start + X64_PAGE_SIZE;

    // Make sure there are no pending writes on the cache lines.
    fence(Ordering::SeqCst);

    for addr in (start..end).step_by(FLUSH_SIZE as usize) {
        // SAFETY: No concurrency issues.
        unsafe {
            asm!("clflush [{0}]", in(reg) addr, options(nostack));
        }
    }
}

fn flush_tlb() {
    fence(Ordering::SeqCst);
    // NOTE: no flush for the global pages.
    // SAFETY: No concurrency issues.
    unsafe {
        asm!("mov cr3, {0}", in(reg) get_cr3(), options(nostack));
    }
    compiler_fence(Ordering::SeqCst);
}

fn page_table(pfn: u64) -> &'static mut [u64] {
    // SAFETY: The next page address must be set, identical mapping.
    unsafe {
        core::slice::from_raw_parts_mut((pfn << X64_PAGE_SHIFT) as *mut u64, PAGE_TABLE_ENTRY_COUNT)
    }
}

fn pte_for_pfn(pfn: u64, confidential: bool) -> u64 {
    let common = X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE | (pfn << X64_PAGE_SHIFT);
    if confidential {
        common | X64_PTE_CONFIDENTIAL
    } else {
        common
    }
}

/// GHCB page access. The GHCB page is statically allocated and
/// initialized. The GHCB page might be accessed and modified
/// concurrently by the (malicious) host, and the atomic accesses
/// mitigate the possibility of torn reads/writes.
mod ghcb_access {
    use super::GHCB_GVA;
    use crate::PageAlign;
    use crate::arch::x86_64::address_space::X64_PAGE_SHIFT;
    use crate::zeroed;
    use core::mem::offset_of;
    use core::sync::atomic::AtomicU8;
    use core::sync::atomic::AtomicU16;
    use core::sync::atomic::AtomicU32;
    use core::sync::atomic::AtomicU64;
    use core::sync::atomic::Ordering;
    use x86defs::snp::GHCB_PAGE_HV_HYPERCALL_DATA_SIZE;
    use x86defs::snp::GhcbPage;
    use x86defs::snp::GhcbPageHvHypercall;
    use x86defs::snp::GhcbProtocolVersion;
    use x86defs::snp::GhcbSaveArea;
    use x86defs::snp::GhcbUsage;

    /// The GHCB page itself. Must not be *ever* accessed directly
    /// using the static. It might be unaccepted at any time, and
    /// the VA below is mapped with the C-bit set.
    ///
    /// The declaration is just a means to get the page statically
    /// allocated and aligned.
    static GHCB: PageAlign<[u8; size_of::<GhcbPage>()]> = zeroed();

    pub fn page_number() -> u64 {
        // Identical mapping, the GVA is the same as the GPA.
        let gva = GHCB.0.as_ptr() as u64;
        gva >> X64_PAGE_SHIFT
    }

    /// # Safety
    ///
    /// The caller must ensure that the GHCB page is properly mapped
    /// and can be accessed in a memory-safe manner.
    unsafe fn ghcb_data<T>() -> &'static mut [T] {
        // SAFETY: The GHCB page is statically allocated and initialized.
        // It is either mapped by the time of access, or the code won't
        // be executed at all due to the hardware fault.
        unsafe {
            core::slice::from_raw_parts_mut(
                GHCB_GVA.into_bits() as *mut T,
                size_of::<GhcbPage>() / size_of::<T>(),
            )
        }
    }

    macro_rules! ghcb_field_set {
        ($field:ident, $type:ty, $val:expr) => {{
            // SAFETY: Atomic access to the GHCB page.
            let ghcb_data = unsafe { ghcb_data::<$type>() };
            let pos = offset_of!(GhcbPage, $field) / size_of::<$type>();
            ghcb_data[pos].store($val, Ordering::SeqCst);
        }};
    }

    macro_rules! ghcb_save_field_set {
        ($field:ident, $type:ty, $func:ident, $val:expr) => {{
            // SAFETY: Atomic access to the GHCB page.
            let ghcb_data = unsafe { ghcb_data::<$type>() };
            // Save area is at the beginning of the GHCB page.
            let pos = offset_of!(GhcbSaveArea, $field) / size_of::<$type>();
            ghcb_data[pos].$func($val, Ordering::SeqCst);
        }};
    }

    macro_rules! ghcb_save_field_get {
        ($field:ident, $type:ty) => {{
            // SAFETY: Atomic access to the GHCB page.
            let ghcb_data = unsafe { ghcb_data::<$type>() };
            // Save area is at the beginning of the GHCB page.
            let pos = offset_of!(GhcbSaveArea, $field) / size_of::<$type>();
            ghcb_data[pos].load(Ordering::SeqCst)
        }};
    }

    pub fn clear_page() {
        // SAFETY: Atomic access to the GHCB page.
        unsafe { ghcb_data::<AtomicU64>() }
            .iter()
            .for_each(|x| x.store(0, Ordering::SeqCst));
    }

    pub fn clear_bitmaps() {
        ghcb_save_field_set!(valid_bitmap0, AtomicU64, store, 0);
        ghcb_save_field_set!(valid_bitmap1, AtomicU64, store, 0);
    }

    macro_rules! ghcb_save_set_valid_bitmap0 {
        ($save_field:ident) => {{
            let mask = 1u64 << (offset_of!(GhcbSaveArea, $save_field) / 8);
            ghcb_save_field_set!(valid_bitmap0, AtomicU64, fetch_or, mask);
        }};
    }

    macro_rules! ghcb_save_set_valid_bitmap1 {
        ($save_field:ident) => {{
            let mask = 1u64 << (offset_of!(GhcbSaveArea, $save_field) / 8 - 64);
            ghcb_save_field_set!(valid_bitmap1, AtomicU64, fetch_or, mask);
        }};
    }

    macro_rules! ghcb_save_assert_valid_bitmap0 {
        ($save_field:ident) => {{
            let mask = 1u64 << (offset_of!(GhcbSaveArea, $save_field) / 8);
            assert_eq!(ghcb_save_field_get!(valid_bitmap0, AtomicU64) & mask, mask);
        }};
    }

    macro_rules! ghcb_save_assert_valid_bitmap1 {
        ($save_field:ident) => {{
            let mask = 1u64 << (offset_of!(GhcbSaveArea, $save_field) / 8 - 64);
            assert_eq!(ghcb_save_field_get!(valid_bitmap1, AtomicU64) & mask, mask);
        }};
    }

    pub fn set_usage(usage: GhcbUsage) {
        ghcb_field_set!(ghcb_usage, AtomicU32, usage.into_bits());
    }

    pub fn set_protocol_version(version: GhcbProtocolVersion) {
        ghcb_field_set!(protocol_version, AtomicU16, version.into_bits());
    }

    pub fn set_sw_exit_code(code: u64) {
        ghcb_save_field_set!(sw_exit_code, AtomicU64, store, code);
        ghcb_save_set_valid_bitmap1!(sw_exit_code);
    }

    pub fn set_sw_exit_info1(info: u64) {
        ghcb_save_field_set!(sw_exit_info1, AtomicU64, store, info);
        ghcb_save_set_valid_bitmap1!(sw_exit_info1);
    }

    pub fn sw_exit_info1() -> u64 {
        ghcb_save_assert_valid_bitmap1!(sw_exit_info1);
        ghcb_save_field_get!(sw_exit_info1, AtomicU64)
    }

    pub fn set_sw_exit_info2(info: u64) {
        ghcb_save_field_set!(sw_exit_info2, AtomicU64, store, info);
        ghcb_save_set_valid_bitmap1!(sw_exit_info2);
    }

    pub fn set_rax(rax: u64) {
        ghcb_save_field_set!(rax, AtomicU64, store, rax);
        ghcb_save_set_valid_bitmap0!(rax);
    }

    pub fn rax() -> u64 {
        ghcb_save_assert_valid_bitmap0!(rax);
        ghcb_save_field_get!(rax, AtomicU64)
    }

    pub fn set_rcx(rcx: u64) {
        ghcb_save_field_set!(rcx, AtomicU64, store, rcx);
        ghcb_save_set_valid_bitmap1!(rcx);
    }

    pub fn set_rdx(rdx: u64) {
        ghcb_save_field_set!(rdx, AtomicU64, store, rdx);
        ghcb_save_set_valid_bitmap1!(rdx);
    }

    pub fn rdx() -> u64 {
        ghcb_save_assert_valid_bitmap1!(rdx);
        ghcb_save_field_get!(rdx, AtomicU64)
    }

    /// # Safety
    ///
    /// The caller must ensure that the GHCB page is properly mapped
    /// and can be accessed in a memory-safe manner.
    unsafe fn ghcb_hv_hypercall<T>() -> &'static mut [T] {
        // SAFETY: The GHCB page is statically allocated and initialized.
        // It is either mapped by the time of access, or the code won't
        // be executed at all due to the hardware fault.
        unsafe {
            core::slice::from_raw_parts_mut(
                GHCB_GVA.into_bits() as *mut T,
                size_of::<GhcbPageHvHypercall>() / size_of::<T>(),
            )
        }
    }

    macro_rules! ghcb_hv_hypercall_field_set {
        ($field:ident, $type:ty, $val:expr) => {{
            // SAFETY: Atomic access to the GHCB page.
            let ghcb_data = unsafe { ghcb_hv_hypercall::<$type>() };
            let pos = offset_of!(GhcbPageHvHypercall, $field) / size_of::<$type>();
            ghcb_data[pos].store($val, Ordering::SeqCst);
        }};
    }

    macro_rules! ghcb_hv_hypercall_field_get {
        ($field:ident, $type:ty) => {{
            // SAFETY: Atomic access to the GHCB page.
            let ghcb_data = unsafe { ghcb_hv_hypercall::<$type>() };
            let pos = offset_of!(GhcbPageHvHypercall, $field) / size_of::<$type>();
            ghcb_data[pos].load(Ordering::SeqCst)
        }};
    }

    pub fn set_hypercall_data(data: &[u8], start: usize) {
        // SAFETY: Atomic access to the GHCB page.
        let ghcb_data = unsafe { ghcb_hv_hypercall::<AtomicU8>() };
        assert!(data.len() <= GHCB_PAGE_HV_HYPERCALL_DATA_SIZE);

        ghcb_data[start..start + data.len()]
            .iter()
            .zip(data.iter())
            .for_each(|(x, y)| x.store(*y, Ordering::SeqCst));
    }

    pub fn set_hypercall_input(input: u64) {
        ghcb_hv_hypercall_field_set!(io, AtomicU64, input);
    }

    pub fn hypercall_output() -> u64 {
        ghcb_hv_hypercall_field_get!(io, AtomicU64)
    }

    pub fn set_hypercall_output_gpa(gpa: u64) {
        ghcb_hv_hypercall_field_set!(output_gpa, AtomicU64, gpa);
    }
}

#[allow(dead_code)]
enum IoAccessSize {
    Byte = 1,
    Word = 2,
    Dword = 4,
}

impl Ghcb {
    pub fn initialize() {
        // Make sure page alignment.
        assert!((PAGE_TABLE.get() as u64) & (X64_PAGE_SIZE - 1) == 0);
        assert!((PD_TABLE.get() as u64) & (X64_PAGE_SIZE - 1) == 0);
        assert!((PDP_TABLE.get() as u64) & (X64_PAGE_SIZE - 1) == 0);

        // Map the GHCB page in the guest as non-confidential.

        let page_root = get_cr3() & !(X64_PAGE_SIZE - 1);
        let pml4table = page_table(page_root >> X64_PAGE_SHIFT);
        assert!(pml4table[PML4_INDEX] & X64_PTE_PRESENT == 0);

        // Running in identical mapping.
        let pdp_table_pfn = (PDP_TABLE.get() as u64) >> X64_PAGE_SHIFT;
        let pd_table_pfn = (PD_TABLE.get() as u64) >> X64_PAGE_SHIFT;
        let page_table_pfn = (PAGE_TABLE.get() as u64) >> X64_PAGE_SHIFT;
        let page_number = ghcb_access::page_number();

        let pdp_table = page_table(pdp_table_pfn);
        let pd_table = page_table(pd_table_pfn);
        let page_table = page_table(page_table_pfn);

        pml4table[PML4_INDEX] = pte_for_pfn(pdp_table_pfn, true);
        pdp_table[PDP_INDEX] = pte_for_pfn(pd_table_pfn, true);
        pd_table[PD_INDEX] = pte_for_pfn(page_table_pfn, true);
        page_table[PT_INDEX] = pte_for_pfn(page_number, true);

        flush_tlb();
        // Evict the page from the cache before changing the encrypted state.
        cache_lines_flush_page(GHCB_GVA.into_bits());

        // Unaccept the page, invalidates page state.
        pvalidate(page_number, GHCB_GVA.into_bits(), false, false).expect("memory unaccept");
        // Issue VMG exit to request the hypervisor to update the page state to host visible in RMP.
        let resp = Ghcb::ghcb_call(GhcbCall {
            info: GhcbInfo::PAGE_STATE_CHANGE,
            extra_data: x86defs::snp::GHCB_DATA_PAGE_STATE_SHARED,
            page_number,
        });
        assert!(resp.into_bits() == GhcbInfo::PAGE_STATE_UPDATED.0);

        // Map the page as non-confidential by updating the PTE.
        page_table[PT_INDEX] = pte_for_pfn(page_number, false);
        flush_tlb();
        // Evict the page from the cache before changing the encrypted state.
        cache_lines_flush_page(GHCB_GVA.into_bits());

        // Flipping the C-bit makes the contents of the GHCB page scrambled,
        // zero it out.
        ghcb_access::clear_page();
        ghcb_access::set_protocol_version(GhcbProtocolVersion::V2);

        // Register the GHCB page with the hypervisor.

        let resp = Self::ghcb_call(GhcbCall {
            extra_data: 0,
            page_number: ghcb_access::page_number(),
            info: GhcbInfo::REGISTER_REQUEST,
        });
        assert!(
            resp.info() == GhcbInfo::REGISTER_RESPONSE.0
                && resp.extra_data() == 0
                && resp.pfn() == ghcb_access::page_number(),
            "GhcbInfo::REGISTER_RESPONSE returned msr value {resp:x?}"
        );

        // Register to issue Hyper-V hypercalls.
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        assert!(Self::set_msr(
            hvdef::HV_X64_MSR_GUEST_OS_ID,
            guest_os_id.into()
        ));
        // and make sure it is set as expected.
        assert!(
            Self::get_msr(hvdef::HV_X64_MSR_GUEST_OS_ID).expect("GHCB: Failed to set guest OS ID")
                == guest_os_id.into()
        );
        Self::set_register(HvX64RegisterName::GuestOsId, guest_os_id.into_bits().into())
            .expect("failed to set guest OS ID");

        // SAFETY: Always safe to read the GHCB MSR, no concurrency issues.
        GHCB_PREVIOUS.replace(unsafe { read_msr(X86X_AMD_MSR_GHCB) });
    }

    pub fn uninitialize() {
        // Unregister from issuing Hyper-V hypercalls.
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new();
        Self::set_register(HvX64RegisterName::GuestOsId, guest_os_id.into_bits().into())
            .expect("failed to set guest OS ID");
        assert!(Self::set_msr(
            hvdef::HV_X64_MSR_GUEST_OS_ID,
            guest_os_id.into()
        ));
        // and make sure it is set as expected.
        assert!(
            Self::get_msr(hvdef::HV_X64_MSR_GUEST_OS_ID).expect("GHCB: Failed to set guest OS ID")
                == guest_os_id.into()
        );

        // A silly trick to unmap the GHCB overlay page.
        let resp = Self::ghcb_call(GhcbCall {
            extra_data: 0,
            page_number: 0,
            info: GhcbInfo::REGISTER_REQUEST,
        });
        assert!(
            resp.info() == GhcbInfo::REGISTER_RESPONSE.0
                && resp.extra_data() == 0
                && resp.pfn() == 0,
            "GhcbInfo::REGISTER_RESPONSE returned msr value {resp:x?}"
        );

        // Map the GHCB page in the guest as confidential and accept it again
        // to return to the original state.

        // Evict the page from the cache before changing the encrypted state.
        cache_lines_flush_page(GHCB_GVA.into_bits());

        // Update the page table entry to make it confidential.
        // Running in identical mapping.
        let page_table_pfn = (PAGE_TABLE.get() as u64) >> X64_PAGE_SHIFT;
        let page_table = page_table(page_table_pfn);
        let page_number = ghcb_access::page_number();

        page_table[PT_INDEX] |= X64_PTE_CONFIDENTIAL;

        // Issue VMG exit to request the hypervisor to update the page state to private in RMP.
        let resp = Ghcb::ghcb_call(GhcbCall {
            info: GhcbInfo::PAGE_STATE_CHANGE,
            extra_data: x86defs::snp::GHCB_DATA_PAGE_STATE_PRIVATE,
            page_number,
        });
        assert!(resp.into_bits() == GhcbInfo::PAGE_STATE_UPDATED.0);

        // Accept the page, invalidates page state.
        pvalidate(page_number, GHCB_GVA.into_bits(), false, true).expect("memory accept");

        // Needed here, not before.
        flush_tlb();

        ghcb_access::clear_page();

        // SAFETY: Always safe to write the GHCB MSR, no concurrency issues.
        unsafe { write_msr(X86X_AMD_MSR_GHCB, GHCB_PREVIOUS.get()) };
    }

    #[inline(always)]
    fn vmg_exit() {
        // SAFETY: Using the `vmgexit` instruction forces an exit to the hypervisor but doesn't
        // directly change program state.
        unsafe {
            asm!("rep vmmcall", options(nomem, nostack));
        }
    }

    /// Perform the GHCB call
    fn ghcb_call(call_data: GhcbCall) -> GhcbMsr {
        let GhcbCall {
            info,
            extra_data,
            page_number,
        } = call_data;
        let ghcb_control = GhcbMsr::new()
            .with_pfn(page_number)
            .with_info(info.0)
            .with_extra_data(extra_data);

        GhcbMsr::from_bits(
            // SAFETY: Writing and reding known good value to/from the GHCB MSR, following the GHCB protocol.
            unsafe {
                write_msr(X86X_AMD_MSR_GHCB, ghcb_control.into_bits());
                Self::vmg_exit();
                read_msr(X86X_AMD_MSR_GHCB)
            },
        )
    }

    pub fn change_page_visibility(range: MemoryRange, host_visible: bool) {
        for page_number in range.start_4k_gpn()..range.end_4k_gpn() {
            let extra_data = if host_visible {
                x86defs::snp::GHCB_DATA_PAGE_STATE_SHARED
            } else {
                x86defs::snp::GHCB_DATA_PAGE_STATE_PRIVATE
            };

            let resp = Self::ghcb_call(GhcbCall {
                info: GhcbInfo::PAGE_STATE_CHANGE,
                extra_data,
                page_number,
            });

            // High 32 bits are status and should be 0 (HV_STATUS_SUCCESS), Low 32 bits should be
            // GHCB_INFO_PAGE_STATE_UPDATED. Assert if otherwise.

            assert!(
                resp.into_bits() == GhcbInfo::PAGE_STATE_UPDATED.0,
                "GhcbInfo::PAGE_STATE_UPDATED returned msr value {resp:x?}"
            );
        }
    }

    #[must_use]
    fn read_io_port(port: u16, access_size: IoAccessSize) -> Option<u32> {
        ghcb_access::set_usage(GhcbUsage::BASE);
        ghcb_access::set_protocol_version(GhcbProtocolVersion::V2);
        ghcb_access::clear_bitmaps();

        let io_exit_info = SevIoAccessInfo::new()
            .with_port(port)
            .with_read_access(true);
        let io_exit_info = match access_size {
            IoAccessSize::Byte => io_exit_info.with_access_size8(true),
            IoAccessSize::Word => io_exit_info.with_access_size16(true),
            IoAccessSize::Dword => io_exit_info.with_access_size32(true),
        };

        ghcb_access::set_sw_exit_code(SevExitCode::IOIO.0);
        ghcb_access::set_sw_exit_info1(io_exit_info.into_bits().into());
        ghcb_access::set_sw_exit_info2(0);

        Self::ghcb_call(GhcbCall {
            info: GhcbInfo::NORMAL,
            extra_data: 0,
            page_number: ghcb_access::page_number(),
        });
        ghcb_access::set_usage(GhcbUsage::INVALID);

        if ghcb_access::sw_exit_info1() != 0 {
            None
        } else {
            Some(ghcb_access::rax() as u32)
        }
    }

    #[must_use]
    fn write_io_port(port: u16, access_size: IoAccessSize, data: u32) -> bool {
        ghcb_access::set_usage(GhcbUsage::BASE);
        ghcb_access::set_protocol_version(GhcbProtocolVersion::V2);
        ghcb_access::clear_bitmaps();

        let io_exit_info = SevIoAccessInfo::new()
            .with_port(port)
            .with_read_access(false);
        let io_exit_info = match access_size {
            IoAccessSize::Byte => io_exit_info.with_access_size8(true),
            IoAccessSize::Word => io_exit_info.with_access_size16(true),
            IoAccessSize::Dword => io_exit_info.with_access_size32(true),
        };

        ghcb_access::set_sw_exit_code(SevExitCode::IOIO.0);
        ghcb_access::set_sw_exit_info1(io_exit_info.into_bits().into());
        ghcb_access::set_sw_exit_info2(0);

        ghcb_access::set_rax(data as u64);

        Self::ghcb_call(GhcbCall {
            info: GhcbInfo::NORMAL,
            extra_data: 0,
            page_number: ghcb_access::page_number(),
        });
        ghcb_access::set_usage(GhcbUsage::INVALID);

        ghcb_access::sw_exit_info1() == 0
    }

    #[must_use]
    pub fn set_msr(msr_index: u32, value: u64) -> bool {
        ghcb_access::set_usage(GhcbUsage::BASE);
        ghcb_access::set_protocol_version(GhcbProtocolVersion::V2);
        ghcb_access::clear_bitmaps();

        ghcb_access::set_sw_exit_code(SevExitCode::MSR.0);
        ghcb_access::set_sw_exit_info1(1);
        ghcb_access::set_sw_exit_info2(0);

        ghcb_access::set_rcx(msr_index as u64);
        ghcb_access::set_rax(value as u32 as u64);
        ghcb_access::set_rdx((value >> 32) as u32 as u64);

        Self::ghcb_call(GhcbCall {
            info: GhcbInfo::NORMAL,
            extra_data: 0,
            page_number: ghcb_access::page_number(),
        });
        ghcb_access::set_usage(GhcbUsage::INVALID);

        ghcb_access::sw_exit_info1() == 0
    }

    #[must_use]
    pub fn get_msr(msr_index: u32) -> Option<u64> {
        ghcb_access::set_usage(GhcbUsage::BASE);
        ghcb_access::set_protocol_version(GhcbProtocolVersion::V2);
        ghcb_access::clear_bitmaps();

        ghcb_access::set_sw_exit_code(SevExitCode::MSR.0);
        ghcb_access::set_sw_exit_info1(0);
        ghcb_access::set_sw_exit_info2(0);

        ghcb_access::set_rcx(msr_index as u64);

        Self::ghcb_call(GhcbCall {
            info: GhcbInfo::NORMAL,
            extra_data: 0,
            page_number: ghcb_access::page_number(),
        });
        ghcb_access::set_usage(GhcbUsage::INVALID);

        if ghcb_access::sw_exit_info1() != 0 {
            None
        } else {
            Some(ghcb_access::rax() | (ghcb_access::rdx() << 32))
        }
    }

    pub fn set_register(
        name: HvX64RegisterName,
        value: HvRegisterValue,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        };
        let reg_assoc = hvdef::hypercall::HvRegisterAssoc {
            name: name.into(),
            pad: Default::default(),
            value,
        };
        let control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallSetVpRegisters.0)
            .with_rep_count(1);

        ghcb_access::set_usage(GhcbUsage::HYPERCALL);
        ghcb_access::set_hypercall_data(header.as_bytes(), 0);
        ghcb_access::set_hypercall_data(reg_assoc.as_bytes(), size_of_val(&header));
        ghcb_access::set_hypercall_input(control.into_bits());
        ghcb_access::set_hypercall_output_gpa(0);

        Self::ghcb_call(GhcbCall {
            info: GhcbInfo::NORMAL,
            extra_data: 0,
            page_number: ghcb_access::page_number(),
        });
        ghcb_access::set_usage(GhcbUsage::INVALID);

        HypercallOutput::from_bits(ghcb_access::hypercall_output()).result()
    }
}

/// Wrapper around the pvalidate assembly instruction.
fn pvalidate(
    page_number: u64,
    va: u64,
    large_page: bool,
    validate: bool,
) -> Result<AcceptGpaStatus, AcceptGpaError> {
    if large_page {
        assert!(va % x86defs::X64_LARGE_PAGE_SIZE == 0);
    } else {
        assert!(va % X64_PAGE_SIZE == 0)
    }

    let validate_page = validate as u32;
    let page_size = large_page as u32;
    let mut error_code: u32;
    let mut carry_flag: u32 = 0;

    // SAFETY: Issuing pvalidate according to specification.
    unsafe {
        asm!(r#"
        pvalidate
        jnc 2f
        inc {carry_flag:e}
        2:
        "#,
        in("rax") va,
        in("ecx") page_size,
        in("edx") validate_page,
        lateout("eax") error_code,
        carry_flag = inout(reg) carry_flag);
    }

    const SEV_SUCCESS: u32 = 0;
    const SEV_FAIL_SIZEMISMATCH: u32 = 6;

    match (error_code, carry_flag) {
        (SEV_SUCCESS, 0) => Ok(AcceptGpaStatus::Success),
        (SEV_FAIL_SIZEMISMATCH, _) => Ok(AcceptGpaStatus::Retry),
        _ => Err(AcceptGpaError::MemorySecurityViolation {
            error_code,
            carry_flag,
            page_number,
            large_page,
            validate,
        }),
    }
}

/// Accepts or unaccepts a specific gpa range. On SNP systems, this corresponds to issuing a
/// pvalidate over the GPA range with the desired value of the validate bit.
pub fn set_page_acceptance(
    local_map: &mut LocalMap<'_>,
    range: MemoryRange,
    validate: bool,
) -> Result<(), AcceptGpaError> {
    let pages_per_large_page = x86defs::X64_LARGE_PAGE_SIZE / X64_PAGE_SIZE;
    let mut page_count = range.page_count_4k();
    let mut page_base = range.start_4k_gpn();

    while page_count != 0 {
        // Attempt to validate a large page.
        // Even when pvalidating a large page, the processor only does a 1 byte read. As a result
        // mapping a single page is sufficient.
        let mapping = local_map.map_pages(
            MemoryRange::from_4k_gpn_range(page_base..page_base + 1),
            true,
        );
        if page_base % pages_per_large_page == 0 && page_count >= pages_per_large_page {
            let res = pvalidate(page_base, mapping.data.as_ptr() as u64, true, validate)?;
            match res {
                AcceptGpaStatus::Success => {
                    page_count -= pages_per_large_page;
                    page_base += pages_per_large_page;
                    continue;
                }
                AcceptGpaStatus::Retry => (),
            }
        }

        // Attempt to validate a regular sized page.
        let res = pvalidate(page_base, mapping.data.as_ptr() as u64, false, validate)?;
        match res {
            AcceptGpaStatus::Success => {
                page_count -= 1;
                page_base += 1;
            }
            AcceptGpaStatus::Retry => {
                // Cannot retry on a regular sized page.
                return Err(AcceptGpaError::Unknown);
            }
        }
    }

    Ok(())
}

/// GHCB based io port access.
pub struct SnpIoAccess;

impl minimal_rt::arch::IoAccess for SnpIoAccess {
    unsafe fn inb(&self, port: u16) -> u8 {
        // Best effort
        Ghcb::read_io_port(port, IoAccessSize::Byte).unwrap_or(!0) as u8
    }

    unsafe fn outb(&self, port: u16, data: u8) {
        // Best effort
        let _ = Ghcb::write_io_port(port, IoAccessSize::Byte, data as u32);
    }
}
