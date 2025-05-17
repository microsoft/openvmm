// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SNP support for the bootshim.

use super::address_space::LocalMap;
use super::address_space::PAGE_TABLE_ENTRY_COUNT;
use super::address_space::X64_PAGE_SHIFT;
use super::address_space::X64_PTE_ACCESSED;
use super::address_space::X64_PTE_BITS;
use super::address_space::X64_PTE_PRESENT;
use super::address_space::X64_PTE_READ_WRITE;
use core::arch::asm;
use core::mem::offset_of;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvGuestOsId;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HypercallOutput;
use memory_range::MemoryRange;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use x86defs::X86X_AMD_MSR_GHCB;
use x86defs::snp::GhcbInfo;
use x86defs::snp::GhcbMsr;
use x86defs::snp::GhcbUsage;
use x86defs::snp::SevExitCode;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[repr(C, packed)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes)]
struct GhcbSaveArea {
    reserved_0x0: [u8; 203],
    cpl: u8,
    reserved_0xcc: [u8; 116],
    xss: u64,
    reserved_0x148: [u8; 24],
    dr7: u64,
    reserved_0x168: [u8; 16],
    rip: u64,
    reserved_0x180: [u8; 88],
    rsp: u64,
    reserved_0x1e0: [u8; 24],
    rax: u64,
    reserved_0x200: [u8; 264],
    rcx: u64,
    rdx: u64,
    rbx: u64,
    reserved_0x320: [u8; 8],
    rbp: u64,
    rsi: u64,
    rdi: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    reserved_0x380: [u8; 16],
    sw_exit_code: u64,
    sw_exit_info1: u64,
    sw_exit_info2: u64,
    sw_scratch: u64,
    reserved_0x3b0: [u8; 56],
    xcr0: u64,
    valid_bitmap: [u8; 16],
    x87_state_gpa: u64,
}

#[repr(C, packed)]
#[derive(Debug, Copy, Clone, IntoBytes, FromBytes)]
pub struct GhcbPage {
    save: GhcbSaveArea,
    reserved_save: [u8; 2048 - size_of::<GhcbSaveArea>()],
    shared_buffer: [u8; 2032],
    reserved_0xff0: [u8; 10],
    protocol_version: u16,
    ghcb_usage: u32,
}

const _: () = assert!(size_of::<GhcbPage>() == HV_PAGE_SIZE as usize);

static GHCB_PAGE: AtomicPtr<GhcbPage> = AtomicPtr::new(core::ptr::null_mut() as *mut GhcbPage);
static GHCB_PREVIOUS: AtomicU64 = AtomicU64::new(0);

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

#[must_use]
fn map_ghcb_page(page_number: u64) -> *mut GhcbPage {
    let mut page_root: u64;
    unsafe {
        asm!("mov {0}, cr3", out(reg) page_root, options(nostack));
    }

    page_root &= !(HV_PAGE_SIZE);

    // TODO: maybe use the volatile accessor here?
    // SAFETY: The next page address must be set, identical mapping.
    let page_table = |pfn| unsafe {
        core::slice::from_raw_parts_mut((pfn << HV_PAGE_SHIFT) as *mut u64, PAGE_TABLE_ENTRY_COUNT)
    };

    let pml4table = page_table(page_root >> HV_PAGE_SHIFT);
    let mut free_pml4index = None;
    for (pml4index, e) in pml4table.iter_mut().enumerate() {
        if *e & !X64_PTE_PRESENT == 0 {
            free_pml4index = Some(pml4index);
        }
    }
    let pml4index = free_pml4index.expect("No free PML4 entry");

    let (pdp_table_pfn, pd_table_pfn, page_table_pfn) =
        (page_number - 1, page_number - 2, page_number - 3);
    let pdp_table = page_table(pdp_table_pfn);
    let pd_table = page_table(pd_table_pfn);
    let page_table = page_table(page_table_pfn);

    // Map without the C-bit set.
    pml4table[pml4index] =
        X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE | (pdp_table_pfn << X64_PTE_BITS);
    pdp_table[0] =
        X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE | (pd_table_pfn << X64_PTE_BITS);
    pd_table[0] =
        X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE | (page_table_pfn << X64_PTE_BITS);
    page_table[0] =
        X64_PTE_PRESENT | X64_PTE_ACCESSED | X64_PTE_READ_WRITE | (page_number << X64_PTE_BITS);
    compiler_fence(Ordering::SeqCst);

    // Flush the TLB.
    // SAFETY: No concurrency issues.
    unsafe {
        asm!("mov cr3, {0}", in(reg) page_root, options(nostack));
    }
    compiler_fence(Ordering::SeqCst);

    let ghcb_addr = ((pml4index as u64) << (3 * X64_PTE_BITS + X64_PAGE_SHIFT))
        | if pml4index > 255 {
            // Make it upper-halp canonical.
            0xFFFF_0000_0000_0000
        } else {
            0
        };

    pvalidate(page_number, ghcb_addr as u64, false, true).expect("pvalidate succeeds for GHCB");

    // Flipping the C-bit makes the contents of the GHCB page scrambled,
    // zero it out.
    unsafe {
        (ghcb_addr as *mut GhcbPage)
            .as_mut()
            .expect("GHCB page is set")
            .as_mut_bytes()
            .fill(0);
    }

    ghcb_addr as *mut GhcbPage
}

/// Unmap the GHCB page.
fn unmap_ghcb_page(ghcb_ptr: *mut GhcbPage) {
    let mut page_root: u64;
    unsafe {
        asm!("mov {0}, cr3", out(reg) page_root, options(nostack));
    }

    let ghcb_addr = ghcb_ptr as u64;
    let pml4index = (ghcb_addr >> (3 * X64_PTE_BITS + X64_PAGE_SHIFT)) as usize;
    page_root &= !(HV_PAGE_SIZE - 1);

    // TODO: maybe use the volatile accessor here?
    // SAFETY: The next page address must be set, identical mapping.
    let pml4table =
        unsafe { core::slice::from_raw_parts_mut(page_root as *mut u64, PAGE_TABLE_ENTRY_COUNT) };

    // TODO: pvalidate

    pml4table[pml4index] = 0;
    compiler_fence(Ordering::SeqCst);

    // Flush the TLB.
    // SAFETY: No concurrency issues.
    unsafe {
        asm!("mov cr3, {0}", in(reg) page_root, options(nostack));
    }
}

impl Ghcb {
    pub fn initialize(page_number: u64) {
        assert!(page_number != u64::MAX && page_number != 0);

        // SAFETY: Always safe to read the GHCB MSR, no concurrency issues.
        GHCB_PREVIOUS.store(unsafe { read_msr(X86X_AMD_MSR_GHCB) }, Ordering::Release);

        let ghcb_ptr = map_ghcb_page(page_number);
        assert!(!ghcb_ptr.is_null(), "GHCB page is not mapped");

        let resp = Self::ghcb_call(GhcbCall {
            extra_data: 0,
            page_number,
            info: GhcbInfo::REGISTER_REQUEST,
        });

        assert!(
            resp.info() == GhcbInfo::REGISTER_RESPONSE
                && resp.extra_data() == 0
                && resp.pfn() == page_number,
            "GhcbInfo::REGISTER_RESPONSE returned msr value {resp:x?}"
        );

        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        Self::set_msr(HvX64RegisterName::GuestOsId.0, guest_os_id.into());

        GHCB_PAGE.store(ghcb_ptr, Ordering::Release);
    }

    pub fn uninitialize() {
        // SAFETY: Always safe to write the GHCB MSR, no concurrency issues.
        unsafe { write_msr(X86X_AMD_MSR_GHCB, GHCB_PREVIOUS.load(Ordering::Acquire)) };
        unmap_ghcb_page(GHCB_PAGE.load(Ordering::Acquire));
        // TODO: clear the globals
    }

    fn ghcb() -> &'static GhcbPage {
        // SAFETY: The GHCB page is statically allocated and initialized.
        unsafe {
            GHCB_PAGE
                .load(Ordering::Acquire)
                .as_ref()
                .expect("GHCB page is set")
        }
    }

    fn ghcb_mut() -> &'static mut GhcbPage {
        // SAFETY: The GHCB page is statically allocated and initialized.
        unsafe {
            GHCB_PAGE
                .load(Ordering::Acquire)
                .as_mut()
                .expect("GHCB page is set")
        }
    }

    fn ghcb_address() -> u64 {
        let addr = GHCB_PAGE.load(Ordering::Acquire) as u64;
        assert!(addr != 0, "GHCB page is not set");
        addr
    }

    #[inline(always)]
    fn vmgs_exit() {
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
            .with_info(info)
            .with_extra_data(extra_data);

        GhcbMsr::from_bits(
            // SAFETY: Writing and reding known good value to/from the GHCB MSR, following the GHCB protocol.
            unsafe {
                write_msr(X86X_AMD_MSR_GHCB, ghcb_control.into_bits());
                Self::vmgs_exit();
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

    pub fn set_msr(msr_index: u32, value: u64) {
        let ghcb = Self::ghcb_mut();
        ghcb.ghcb_usage = GhcbUsage::BASE.0;
        ghcb.save.rcx = msr_index as u64;
        ghcb.save.rax = value as u32 as u64;
        ghcb.save.rdx = (value >> 32) as u32 as u64;
        ghcb.save.sw_exit_code = SevExitCode::MSR.0;
        ghcb.save.sw_exit_info1 = 1;
        ghcb.save.sw_exit_info2 = 0;

        ghcb.save.valid_bitmap[0] = 1u8 << offset_of!(GhcbSaveArea, rax) / 8;
        ghcb.save.valid_bitmap[1] = (1u8 << (offset_of!(GhcbSaveArea, rcx) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, rdx) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_code) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_info1) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_info2) / 8 - 64));

        Self::vmgs_exit();

        ghcb.ghcb_usage = GhcbUsage::INVALID.0;
        assert!(ghcb.save.sw_exit_info1 == 0);
    }

    pub fn get_msr(msr_index: u32) -> u64 {
        let ghcb = Self::ghcb_mut();
        ghcb.ghcb_usage = GhcbUsage::BASE.0;
        ghcb.save.rcx = msr_index as u64;
        ghcb.save.sw_exit_code = SevExitCode::MSR.0;
        ghcb.save.sw_exit_info1 = 0;
        ghcb.save.sw_exit_info2 = 0;

        ghcb.save.valid_bitmap[0] = 0;
        ghcb.save.valid_bitmap[1] = (1u8 << (offset_of!(GhcbSaveArea, rcx) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_code) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_info1) / 8 - 64))
            | (1u8 << (offset_of!(GhcbSaveArea, sw_exit_info2) / 8 - 64));

        Self::vmgs_exit();

        ghcb.ghcb_usage = GhcbUsage::INVALID.0;
        assert!(ghcb.save.sw_exit_info1 == 0);

        ghcb.save.rax | (ghcb.save.rdx << 32)
    }

    pub fn read_io_port(port: u16, access_size: u8) -> u64 {
        let ghcb = Self::ghcb_mut();
        ghcb.ghcb_usage = GhcbUsage::BASE.0;
        ghcb.save.sw_exit_code = SevExitCode::IOIO.0;

        todo!("fill out ghcb for io port read");
        Self::vmgs_exit();

        ghcb.ghcb_usage = GhcbUsage::INVALID.0;
        assert!(ghcb.save.sw_exit_info1 == 0);
    }

    pub fn write_io_port(port: u16, access_size: u8, data: u32) {
        let ghcb = Self::ghcb_mut();
        ghcb.ghcb_usage = GhcbUsage::BASE.0;
        ghcb.save.sw_exit_code = SevExitCode::IOIO.0;

        todo!("fill out ghcb for io port write");
        Self::vmgs_exit();

        ghcb.ghcb_usage = GhcbUsage::INVALID.0;
        assert!(ghcb.save.sw_exit_info1 == 0);
    }

    fn get_register(&self, name: HvX64RegisterName) -> Result<HvRegisterValue, hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        };

        let ghcb_page = Self::ghcb_mut().as_mut_bytes();

        // PANIC: Infallable, since the hypercall header is less than the size of a page
        header.write_to_prefix(ghcb_page).unwrap();
        // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
        name.write_to_prefix(&mut ghcb_page[HEADER_SIZE..]).unwrap();

        let control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallGetVpRegisters.0)
            .with_rep_count(1)
            .with_fast(false);

        let resp = Self::ghcb_call(GhcbCall {
            info: GhcbInfo::SPECIAL_HYPERCALL,
            extra_data: control.into_bits(),
            page_number: Self::ghcb_address(),
        });

        assert!(resp.info().0 == GhcbInfo::HYPERCALL_OUTPUT.0);
        HypercallOutput::from_bits(((resp.into_bits() >> 16) & 0xFFF) as u64).result()?;

        Ok(HvRegisterValue::read_from_prefix(&ghcb_page).unwrap().0)
    }

    fn set_register(name: HvRegisterName, value: HvRegisterValue) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        };
        let reg = hvdef::hypercall::HvRegisterAssoc {
            name,
            pad: Default::default(),
            value,
        };

        let ghcb_page = Self::ghcb_mut().as_mut_bytes();

        // PANIC: Infallable, since the hypercall header is less than the size of a page
        header.write_to_prefix(ghcb_page).unwrap();
        // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
        reg.write_to_prefix(&mut ghcb_page[HEADER_SIZE..]).unwrap();

        let control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallGetVpRegisters.0)
            .with_rep_count(1)
            .with_fast(false);

        let resp = Self::ghcb_call(GhcbCall {
            info: GhcbInfo::SPECIAL_HYPERCALL,
            extra_data: control.into_bits(),
            page_number: Self::ghcb_address(),
        });

        assert!(resp.info() == GhcbInfo::HYPERCALL_OUTPUT);

        HypercallOutput::from_bits(((resp.into_bits() >> 16) & 0xFFF) as u64).result()
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
        assert!(va % HV_PAGE_SIZE == 0)
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
    let pages_per_large_page = x86defs::X64_LARGE_PAGE_SIZE / HV_PAGE_SIZE;
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
        Ghcb::read_io_port(port, 1) as u8
    }

    unsafe fn outb(&self, port: u16, data: u8) {
        Ghcb::write_io_port(port, 1, data as u32);
    }
}
