// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall infrastructure.

#[cfg(target_arch = "x86_64")]
use crate::arch::TdxHypercallPage;
#[cfg(target_arch = "x86_64")]
use crate::arch::tdx::invoke_tdcall_hypercall;
use crate::single_threaded::SingleThreaded;
use arrayvec::ArrayVec;
use cfg_if::cfg_if;
use core::cell::RefCell;
use core::cell::UnsafeCell;
use core::mem::size_of;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
#[cfg(target_arch = "x86_64")]
use hvdef::hypercall::StartVirtualProcessorX64;
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::invoke_hypercall;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

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
        assert!(addr.is_multiple_of(HV_PAGE_SIZE));

        addr
    }
}

/// Static, reusable page for hypercall input
static HVCALL_INPUT: SingleThreaded<UnsafeCell<HvcallPage>> =
    SingleThreaded(UnsafeCell::new(HvcallPage::new()));

/// Static, reusable page for hypercall output
static HVCALL_OUTPUT: SingleThreaded<UnsafeCell<HvcallPage>> =
    SingleThreaded(UnsafeCell::new(HvcallPage::new()));

static HVCALL: SingleThreaded<RefCell<HvCall>> = SingleThreaded(RefCell::new(HvCall {
    initialized: false,
    vtl: Vtl::Vtl0,
    // The hypercall page is 4KB in the standard setting, but we allocate a large page for
    // TDX compatibility. This is because the underlying static page is mapped in the
    // shim's virtual memory hieararchy as a large page, making 2-MB the minimum shareable
    // memory size between the TDX-enabled shim and hypervisor
    #[cfg(target_arch = "x86_64")]
    tdx_io_page: None,
}));

/// Provides mechanisms to invoke hypercalls within the boot shim.
/// Internally uses static buffers for the hypercall page, the input
/// page, and the output page, so this should not be used in any
/// multi-threaded capacity (which the boot shim currently is not).
pub struct HvCall {
    initialized: bool,
    vtl: Vtl,
    #[cfg(target_arch = "x86_64")]
    tdx_io_page: Option<TdxHypercallPage>,
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
        core::ptr::addr_of!(minimal_rt::arch::hypercall::HYPERCALL_PAGE) as u64
    }

    #[cfg(target_arch = "x86_64")]
    fn init_if_needed(&mut self) {
        if !self.initialized {
            self.initialize();
        }
    }

    /// Hypercall initialization.
    pub fn initialize(&mut self) {
        assert!(!self.initialized);
        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);

        crate::arch::hypercall::initialize(guest_os_id);
        self.initialized = true;

        self.vtl = self
            .get_register(hvdef::HvAllArchRegisterName::VsmVpStatus.into())
            .map_or(Vtl::Vtl0, |status| {
                hvdef::HvRegisterVsmVpStatus::from(status.as_u64())
                    .active_vtl()
                    .try_into()
                    .unwrap()
            });
    }

    /// Call before jumping to kernel.
    pub fn uninitialize(&mut self) {
        if self.initialized {
            self.initialized = false;

            // TODO: this unintuitive flow is a consequence of isolation-specific code
            // being conditionally compiled for each architecture
            //
            // This should be revisited as more isolation-specific flows are added to
            // the shim
            cfg_if! {
                if #[cfg(target_arch = "x86_64")] {
                    if self.tdx_io_page.is_some() {
                        self.uninitialize_tdx()
                    }
                    else {
                        crate::arch::hypercall::uninitialize();
                    }
                }
                else {
                    crate::arch::hypercall::uninitialize();
                }
            }
        }
    }

    /// Returns the environment's VTL.
    pub fn vtl(&mut self) -> Vtl {
        assert!(self.initialized);
        self.vtl
    }

    /// Makes a hypercall.
    /// rep_count is Some for rep hypercalls
    fn dispatch_hvcall(
        &mut self,
        code: hvdef::HypercallCode,
        rep_count: Option<usize>,
    ) -> hvdef::hypercall::HypercallOutput {
        assert!(self.initialized);

        let control = hvdef::hypercall::Control::new()
            .with_code(code.0)
            .with_rep_count(rep_count.unwrap_or_default());

        #[cfg(target_arch = "x86_64")]
        if let Some(tdx_io_page) = &self.tdx_io_page {
            return invoke_tdcall_hypercall(control, tdx_io_page);
        }
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

        // PANIC: Infallable, since the hypercall header is less than the size of a page
        header
            .write_to_prefix(Self::input_page().buffer.as_mut_slice())
            .unwrap();

        let reg = hvdef::hypercall::HvRegisterAssoc {
            name,
            pad: Default::default(),
            value,
        };

        // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
        reg.write_to_prefix(&mut Self::input_page().buffer[HEADER_SIZE..])
            .unwrap();

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallSetVpRegisters, Some(1));

        output.result()
    }

    /// Hypercall for setting a register to a value.
    pub fn get_register(
        &mut self,
        name: hvdef::HvRegisterName,
    ) -> Result<hvdef::HvRegisterValue, hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        };

        // PANIC: Infallable, since the hypercall header is less than the size of a page
        header
            .write_to_prefix(Self::input_page().buffer.as_mut_slice())
            .unwrap();
        // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
        name.write_to_prefix(&mut Self::input_page().buffer[HEADER_SIZE..])
            .unwrap();

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallGetVpRegisters, Some(1));
        output.result()?;
        let value = hvdef::HvRegisterValue::read_from_prefix(&Self::output_page().buffer)
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        Ok(value)
    }

    /// Hypercall to apply vtl protections to the pages from address start to end
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
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
            let count = remaining_pages.min(MAX_INPUT_ELEMENTS as u64) as usize;

            // PANIC: Infallable, since the hypercall header is less than the size of a page
            header
                .write_to_prefix(Self::input_page().buffer.as_mut_slice())
                .unwrap();

            let mut input_offset = HEADER_SIZE;
            for i in 0..count {
                let page_num = current_page + i as u64;
                // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
                page_num
                    .write_to_prefix(&mut Self::input_page().buffer[input_offset..])
                    .unwrap();
                input_offset += size_of::<u64>();
            }

            let output = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallModifyVtlProtectionMask,
                Some(count),
            );

            output.result()?;

            current_page += count as u64;
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
            target_vtl: Vtl::Vtl2.into(),
            reserved: [0; 3],
            vp_vtl_context: zerocopy::FromZeros::new_zeroed(),
        };

        // PANIC: Infallable, since the hypercall header is less than the size of a page
        header
            .write_to_prefix(Self::input_page().buffer.as_mut_slice())
            .unwrap();

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    /// Hypercall to accept vtl2 pages from address start to end with VTL 2
    /// protections and no host visibility
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
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
            let count = remaining_pages.min(MAX_INPUT_ELEMENTS as u64) as usize;

            // PANIC: Infallable, since the hypercall header is less than the size of a page
            header
                .write_to_prefix(Self::input_page().buffer.as_mut_slice())
                .unwrap();

            let output =
                self.dispatch_hvcall(hvdef::HypercallCode::HvCallAcceptGpaPages, Some(count));

            output.result()?;

            current_page += count as u64;
        }

        Ok(())
    }

    /// Get the corresponding VP indices from a list of VP hardware IDs (APIC
    /// IDs on x64, MPIDR on ARM64).
    ///
    /// This always queries VTL0, since the hardware IDs are the same across the
    /// VTLs in practice, and the hypercall only succeeds for VTL2 once VTL2 has
    /// been enabled (which it might not be at this point).
    pub fn get_vp_index_from_hw_id<const N: usize>(
        &mut self,
        hw_ids: &[HwId],
        output: &mut ArrayVec<u32, N>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::GetVpIndexFromApicId {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            target_vtl: 0,
            reserved: [0; 7],
        };

        // Split the call up to avoid exceeding the hypercall input/output size limits.
        const MAX_PER_CALL: usize = 512;

        for hw_ids in hw_ids.chunks(MAX_PER_CALL) {
            // PANIC: Infallable, since the hypercall header is less than the size of a page
            header
                .write_to_prefix(Self::input_page().buffer.as_mut_slice())
                .unwrap();
            // PANIC: Infallable, since the hypercall parameters are chunked to be less
            // than the remaining size (after the header) of the input page.
            // todo: This is *not true* for aarch64, where the hw_ids are u64s. Tracked via
            // https://github.com/microsoft/openvmm/issues/745
            hw_ids
                .write_to_prefix(&mut Self::input_page().buffer[header.as_bytes().len()..])
                .unwrap();

            // SAFETY: The input header and rep slice are the correct types for this hypercall.
            //         The hypercall output is validated right after the hypercall is issued.
            let r = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallGetVpIndexFromApicId,
                Some(hw_ids.len()),
            );

            let n = r.elements_processed();
            output.extend(
                <[u32]>::ref_from_bytes(&Self::output_page().buffer[..n * 4])
                    .unwrap()
                    .iter()
                    .copied(),
            );
            r.result()?;
            assert_eq!(n, hw_ids.len());
        }

        Ok(())
    }
}

// TDX specific hypercall methods
#[cfg(target_arch = "x86_64")]
impl HvCall {
    /// Initialize HvCall for use in a TDX CVM
    pub fn initialize_tdx(&mut self, tdx_io_page: TdxHypercallPage) {
        assert!(!self.initialized);
        self.initialized = true;
        self.vtl = Vtl::Vtl2;
        self.tdx_io_page = Some(tdx_io_page);

        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);

        crate::arch::tdx::initialize_hypercalls(
            guest_os_id.into(),
            self.tdx_io_page.as_ref().unwrap(),
        );
    }

    /// Uninitialize HvCall for TDX, unshare IO pages with the hypervisor
    pub fn uninitialize_tdx(&mut self) {
        crate::arch::tdx::uninitialize_hypercalls(
            self.tdx_io_page
                .take()
                .expect("an initialized instance of HvCall on TDX must have an io page"),
        );
    }

    /// Hypercall to enable VTL2 on a TDX VP
    pub fn tdx_enable_vp_vtl2(&mut self, vp_index: u32) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::EnableVpVtlX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            // The VTL value here is just a u8 and not the otherwise usual
            // HvInputVtl value.
            target_vtl: Vtl::Vtl2.into(),
            reserved: [0; 3],
            // Context must be zeroed for the VP VTL startup hypercall on TDX
            vp_vtl_context: zerocopy::FromZeros::new_zeroed(),
        };
        assert!(self.initialized);

        let input_page_addr = self
            .tdx_io_page
            .as_ref()
            .expect("an initialized instance of HvCall on TDX must have an io page")
            .input();
        // SAFETY: the input page passed into the initialization of HvCall is a
        // free buffer that is not concurrently accessed because openhcl_boot
        // is single threaded
        let input_page = unsafe { &mut *(input_page_addr as *mut [u8; 4096]) };
        header
            .write_to_prefix(input_page)
            .expect("unable to write to hypercall page");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    /// Hypercall to start VP on TDX
    pub fn tdx_start_vp(&mut self, vp_index: u32) -> Result<(), hvdef::HvError> {
        let header = StartVirtualProcessorX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: Vtl::Vtl2.into(),
            rsvd0: 0,
            rsvd1: 0,
            // Context must be zeroed for the AP startup hypercall on TDX
            vp_context: zerocopy::FromZeros::new_zeroed(),
        };
        assert!(self.initialized);

        let input_page_addr = self
            .tdx_io_page
            .as_ref()
            .expect("an initialized instance of HvCall on TDX must have an io page")
            .input();
        // SAFETY: the input page passed into the initialization of HvCall is a
        // free buffer that is not concurrently accessed
        let input_page = unsafe { &mut *(input_page_addr as *mut [u8; 4096]) };
        header
            .write_to_prefix(input_page)
            .expect("unable to write to hypercall page");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallStartVirtualProcessor, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }
}

/// The "hardware ID" used for [`HvCall::get_vp_index_from_hw_id`]. This is the
/// APIC ID on x64.
#[cfg(target_arch = "x86_64")]
pub type HwId = u32;

/// The "hardware ID" used for [`HvCall::get_vp_index_from_hw_id`]. This is the
/// MPIDR on ARM64.
#[cfg(target_arch = "aarch64")]
pub type HwId = u64;
