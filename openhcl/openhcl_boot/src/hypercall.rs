// Copyright (C) Microsoft Corporation. All rights reserved.

//! Hypercall infrastructure.
use crate::single_threaded::SingleThreaded;
use arrayvec::ArrayVec;
use core::cell::RefCell;
use core::cell::UnsafeCell;
use core::cmp;
use core::mem::size_of;
use hvdef::hypercall::HvGpaRange;
use hvdef::hypercall::HvGpaRangeExtended;
use hvdef::hypercall::HvInputVtl;
use hvdef::HV_PAGE_SIZE;
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::invoke_hypercall;
use zerocopy::AsBytes;

const PIN_REQUEST_HEADER_SIZE: usize = size_of::<hvdef::hypercall::PinUnpinGpaPageRangesHeader>();
const MAX_INPUT_ELEMENTS: usize =
    (HV_PAGE_SIZE as usize - PIN_REQUEST_HEADER_SIZE) / size_of::<u64>();

/// Page-aligned, page-sized buffer for use with hypercalls
#[repr(C, align(4096))]
struct HvcallPage {
    buffer: [u8; HV_PAGE_SIZE as usize],
}

impl HvcallPage {
    pub const fn new() -> Self {
        HvcallPage {
            buffer: [0; HV_PAGE_SIZE as usize],
        }
    }

    /// Address of the hypercall page.
    fn address(&self) -> u64 {
        let addr = self.buffer.as_ptr() as u64;

        // These should be page-aligned
        assert!(addr % HV_PAGE_SIZE == 0);

        addr
    }
}

/// Static, reusable page for hypercall input
static HVCALL_INPUT: SingleThreaded<UnsafeCell<HvcallPage>> =
    SingleThreaded(UnsafeCell::new(HvcallPage::new()));

/// Static, reusable page for hypercall output
static HVCALL_OUTPUT: SingleThreaded<UnsafeCell<HvcallPage>> =
    SingleThreaded(UnsafeCell::new(HvcallPage::new()));

static HVCALL: SingleThreaded<RefCell<HvCall>> =
    SingleThreaded(RefCell::new(HvCall { initialized: false }));

/// Provides mechanisms to invoke hypercalls within the boot shim.
/// Internally uses static buffers for the hypercall page, the input
/// page, and the output page, so this should not be used in any
/// multi-threaded capacity (which the boot shim currently is not).
pub struct HvCall {
    initialized: bool,
}

/// Returns an [`HvCall`] instance.
///
/// Panics if another instance is already in use.
#[track_caller]
pub fn hvcall() -> core::cell::RefMut<'static, HvCall> {
    HVCALL.borrow_mut()
}

impl HvCall {
    fn input_page() -> &'static mut HvcallPage {
        // SAFETY: `HVCALL` owns the input page.
        unsafe { &mut *HVCALL_INPUT.get() }
    }

    fn output_page() -> &'static mut HvcallPage {
        // SAFETY: `HVCALL` owns the output page.
        unsafe { &mut *HVCALL_OUTPUT.get() }
    }

    /// Returns the address of the hypercall page, mapping it first if
    /// necessary.
    #[cfg(target_arch = "x86_64")]
    pub fn hypercall_page(&mut self) -> u64 {
        self.init_if_needed();
        // SAFETY: just getting the address of the page.
        unsafe { core::ptr::addr_of!(minimal_rt::arch::hypercall::HYPERCALL_PAGE) as u64 }
    }

    fn init_if_needed(&mut self) {
        if !self.initialized {
            self.initialize();
        }
    }

    pub fn initialize(&mut self) {
        assert!(!self.initialized);

        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        crate::arch::hypercall::initialize(guest_os_id.into());
        self.initialized = true;
    }

    /// Call before jumping to kernel.
    pub fn uninitialize(&mut self) {
        if self.initialized {
            crate::arch::hypercall::uninitialize();
            self.initialized = false;
        }
    }

    /// Makes a hypercall.
    /// rep_count is Some for rep hypercalls
    fn dispatch_hvcall(
        &mut self,
        code: hvdef::HypercallCode,
        rep_count: Option<usize>,
    ) -> hvdef::hypercall::HypercallOutput {
        self.init_if_needed();

        let control = hvdef::hypercall::Control::new()
            .with_code(code.0)
            .with_rep_count(rep_count.unwrap_or_default());

        // SAFETY: Invoking hypercall per TLFS spec
        unsafe {
            invoke_hypercall(
                control,
                Self::input_page().address(),
                Self::output_page().address(),
            )
        }
    }

    /// Hypercall for setting a register to a value.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub fn set_register(
        &mut self,
        name: hvdef::HvRegisterName,
        value: hvdef::HvRegisterValue,
    ) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        };

        header.write_to_prefix(Self::input_page().buffer.as_mut_slice());

        let reg = hvdef::hypercall::HvRegisterAssoc {
            name,
            pad: Default::default(),
            value,
        };

        reg.write_to_prefix(&mut Self::input_page().buffer[HEADER_SIZE..]);

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallSetVpRegisters, Some(1));

        output.result()
    }

    /// Hypercall to apply vtl protections to the pages from address start to end
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub fn apply_vtl2_protections(&mut self, range: MemoryRange) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::ModifyVtlProtectionMask>();
        const MAX_INPUT_ELEMENTS: usize = (HV_PAGE_SIZE as usize - HEADER_SIZE) / size_of::<u64>();

        let header = hvdef::hypercall::ModifyVtlProtectionMask {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            map_flags: hvdef::HV_MAP_GPA_PERMISSIONS_NONE,
            target_vtl: HvInputVtl::CURRENT_VTL,
            reserved: [0; 3],
        };

        let mut current_page = range.start_4k_gpn();
        while current_page < range.end_4k_gpn() {
            let remaining_pages = range.end_4k_gpn() - current_page;
            let count = remaining_pages.min(MAX_INPUT_ELEMENTS as u64);

            header.write_to_prefix(Self::input_page().buffer.as_mut_slice());

            let mut input_offset = HEADER_SIZE;
            for i in 0..count {
                let page_num = current_page + i;
                page_num.write_to_prefix(&mut Self::input_page().buffer[input_offset..]);
                input_offset += size_of::<u64>();
            }

            let output = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallModifyVtlProtectionMask,
                Some(count as usize),
            );

            output.result()?;

            current_page += count;
        }

        Ok(())
    }

    fn to_hv_gva_range_array(
        memory_range: &MemoryRange,
    ) -> ArrayVec<HvGpaRange, MAX_INPUT_ELEMENTS> {
        const PAGES_PER_ENTRY: u64 = 2048;
        const PAGE_SIZE: u64 = HV_PAGE_SIZE;
        let mut ranges = ArrayVec::new();

        // Calculate the total number of pages in the memory range
        let total_pages = (memory_range.end() - memory_range.start()).div_ceil(PAGE_SIZE);

        // Iterate over the memory range in chunks of 2048 pages
        let mut current_page = memory_range.start() >> 12; // Convert start address to page number
        let mut remaining_pages = total_pages;

        while remaining_pages > 0 {
            // Determine how many pages to use in this HvGvaRange (either 2048 or the remaining pages if fewer)
            let pages_in_this_range = cmp::min(PAGES_PER_ENTRY, remaining_pages);

            ranges.push(HvGpaRange(
                HvGpaRangeExtended::new()
                    .with_additional_pages(pages_in_this_range - 1)
                    .with_large_page(false)
                    .with_gpa_page_number(current_page)
                    .into_bits(),
            ));

            // Move to the next chunk of pages
            current_page += pages_in_this_range;
            remaining_pages -= pages_in_this_range;
        }

        ranges
    }

    // Hypercall to pin vtl2 memory
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub fn pin_gpa_range(&mut self, memory_range: MemoryRange) -> Result<(), hvdef::HvError> {
        const PAGES_PER_ENTRY: u64 = 2048;

        // Calculate the total number of pages per request
        let max_bytes_per_request = PAGES_PER_ENTRY * MAX_INPUT_ELEMENTS as u64 * HV_PAGE_SIZE;

        // Track the current start of the memory range
        let mut current_start = memory_range.start();
        let mut remaining_size_bytes = memory_range.end() - memory_range.start();

        let header = hvdef::hypercall::PinUnpinGpaPageRangesHeader { reserved: 0 };
        let input_offset = size_of::<hvdef::hypercall::PinUnpinGpaPageRangesHeader>();

        while remaining_size_bytes > 0 {
            // Determine the size for this chunk of memory
            let chunk_bytes = cmp::min(remaining_size_bytes, max_bytes_per_request);

            let chunk_end = current_start + chunk_bytes;

            // Create a sub-range for this chunk
            let sub_range = MemoryRange::new(current_start..chunk_end);

            // Get HvGvaRange for this sub-range
            let gva_ranges: ArrayVec<HvGpaRange, MAX_INPUT_ELEMENTS> =
                Self::to_hv_gva_range_array(&sub_range);

            // Write the header and gva_ranges to the buffer
            header.write_to_prefix(Self::input_page().buffer.as_mut_slice());

            gva_ranges.write_to_prefix(&mut Self::input_page().buffer[input_offset..]);

            // Call the hypercall with the current chunk
            let output = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallPinGpaPageRanges,
                Some(gva_ranges.len()),
            );

            // Check if the hypercall was successful
            output.result()?;

            // Update remaining memory range for the next iteration
            remaining_size_bytes -= chunk_bytes;
            current_start = chunk_end;
        }

        Ok(())
    }

    /// Hypercall to enable VP VTL
    #[cfg(target_arch = "aarch64")]
    pub fn enable_vp_vtl(&mut self, vp_index: u32) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::EnableVpVtlArm64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            // The VTL value here is just a u8 and not the otherwise usual
            // HvInputVtl value.
            target_vtl: hvdef::Vtl::Vtl2.into(),
            reserved: [0; 3],
            vp_vtl_context: zerocopy::FromZeroes::new_zeroed(),
        };

        header.write_to_prefix(Self::input_page().buffer.as_mut_slice());

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    /// Hypercall to accept vtl2 pages from address start to end with VTL 2
    /// protections and no host visibility
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub fn accept_vtl2_pages(
        &mut self,
        range: MemoryRange,
        memory_type: hvdef::hypercall::AcceptMemoryType,
    ) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::AcceptGpaPages>();
        const MAX_INPUT_ELEMENTS: usize = (HV_PAGE_SIZE as usize - HEADER_SIZE) / size_of::<u64>();

        let mut current_page = range.start_4k_gpn();
        while current_page < range.end_4k_gpn() {
            let header = hvdef::hypercall::AcceptGpaPages {
                partition_id: hvdef::HV_PARTITION_ID_SELF,
                page_attributes: hvdef::hypercall::AcceptPagesAttributes::new()
                    .with_memory_type(memory_type.0)
                    .with_host_visibility(hvdef::hypercall::HostVisibilityType::PRIVATE) // no host visibility
                    .with_vtl_set(1 << 2), // applies vtl permissions for vtl 2
                vtl_permission_set: hvdef::hypercall::VtlPermissionSet {
                    vtl_permission_from_1: [0; hvdef::hypercall::HV_VTL_PERMISSION_SET_SIZE],
                },
                gpa_page_base: current_page,
            };

            let remaining_pages = range.end_4k_gpn() - current_page;
            let count = remaining_pages.min(MAX_INPUT_ELEMENTS as u64);

            header.write_to_prefix(Self::input_page().buffer.as_mut_slice());

            let output = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallAcceptGpaPages,
                Some(count as usize),
            );

            output.result()?;

            current_page += count;
        }

        Ok(())
    }
}
