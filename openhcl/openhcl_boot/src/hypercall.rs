// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall infrastructure.

use crate::PageAlign;
use crate::host_params::shim_params::IsolationType;
use crate::off_stack;
use crate::single_threaded::OffStackRef;
use crate::zeroed;
use arrayvec::ArrayVec;
use core::mem::MaybeUninit;
use core::mem::size_of;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::invoke_hypercall;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Provides mechanisms to invoke hypercalls within the boot shim.
/// Internally uses static buffers for the hypercall page, the input
/// page, and the output page, so this should not be used in any
/// multi-threaded capacity (which the boot shim currently is not).
struct HvCallNoHardIsolation {
    input: OffStackRef<'static, PageAlign<[u8; HV_PAGE_SIZE as usize]>>,
    output: OffStackRef<'static, PageAlign<[u8; HV_PAGE_SIZE as usize]>>,
    vtl: Vtl,
}

impl HvCallNoHardIsolation {
    pub fn new() -> Self {
        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        crate::arch::hypercall::initialize(guest_os_id.into());

        // Use the internal function to bootstrap.
        let vtl = Self::get_register_internal(hvdef::HvAllArchRegisterName::VsmVpStatus.into())
            .map_or(Vtl::Vtl0, |status| {
                hvdef::HvRegisterVsmVpStatus::from(status.as_u64())
                    .active_vtl()
                    .try_into()
                    .unwrap()
            });

        Self {
            input: off_stack!(PageAlign<[u8; HV_PAGE_SIZE as usize]>, zeroed()),
            output: off_stack!(PageAlign<[u8; HV_PAGE_SIZE as usize]>, zeroed()),
            vtl,
        }
    }

    /// Returns the address of the hypercall page.
    pub fn hypercall_page(&mut self) -> Option<u64> {
        #[cfg(target_arch = "x86_64")]
        {
            Some(core::ptr::addr_of!(minimal_rt::arch::hypercall::HYPERCALL_PAGE) as u64)
        }
        #[cfg(target_arch = "aarch64")]
        {
            None
        }
    }

    /// Call before jumping to kernel.
    pub fn uninitialize(self) {
        crate::arch::hypercall::uninitialize();
    }

    /// Returns the environment's VTL.
    pub fn vtl(&self) -> Vtl {
        self.vtl
    }

    pub fn input_page(&self) -> &[u8] {
        self.input.0.as_slice()
    }

    pub fn output_page(&self) -> &[u8] {
        self.output.0.as_slice()
    }

    pub fn input_page_mut(&mut self) -> &mut [u8] {
        self.input.0.as_mut_slice()
    }

    pub fn output_page_mut(&mut self) -> &mut [u8] {
        self.input.0.as_mut_slice()
    }

    /// Makes a hypercall.
    /// rep_count is Some for rep hypercalls
    fn dispatch_hvcall(
        &mut self,
        code: hvdef::HypercallCode,
        rep_count: Option<usize>,
    ) -> hvdef::hypercall::HypercallOutput {
        let control = hvdef::hypercall::Control::new()
            .with_code(code.0)
            .with_rep_count(rep_count.unwrap_or_default());

        // SAFETY: Invoking hypercall per TLFS spec
        unsafe {
            invoke_hypercall(
                control,
                self.input_page().as_ptr() as u64,
                self.output_page_mut().as_ptr() as u64,
            )
        }
    }

    /// Hypercall for setting a register to a value.
    ///
    /// The implementatio takes advantage of the fact that just one register
    /// is set so a fixed on-stack buffer is used. Make sure the hypercall
    /// interface with the hypervisor is established before calling this
    /// function.
    fn set_register_internal(
        name: hvdef::HvRegisterName,
        value: hvdef::HvRegisterValue,
    ) -> Result<(), hvdef::HvError> {
        #[repr(C, align(8))]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        struct Input {
            header: hvdef::hypercall::GetSetVpRegisters,
            assoc: hvdef::hypercall::HvRegisterAssoc,
        }

        let input = Input {
            header: hvdef::hypercall::GetSetVpRegisters {
                partition_id: hvdef::HV_PARTITION_ID_SELF,
                vp_index: hvdef::HV_VP_INDEX_SELF,
                target_vtl: HvInputVtl::CURRENT_VTL,
                rsvd: [0; 3],
            },
            assoc: hvdef::hypercall::HvRegisterAssoc {
                name,
                pad: Default::default(),
                value,
            },
        };

        // SAFETY: Invoking hypercall per TLFS spec
        unsafe {
            invoke_hypercall(
                hvdef::hypercall::Control::new()
                    .with_code(hvdef::HypercallCode::HvCallSetVpRegisters.0)
                    .with_rep_count(1),
                input.as_bytes().as_ptr() as u64,
                0,
            )
        }
        .result()
    }

    /// Hypercall for setting a register to a value.
    pub fn set_register(
        &mut self,
        name: hvdef::HvRegisterName,
        value: hvdef::HvRegisterValue,
    ) -> Result<(), hvdef::HvError> {
        Self::set_register_internal(name, value)
    }

    /// Hypercall for getting a value of a register.
    ///
    /// The implementatio takes advantage of the fact that just one register
    /// is gotten so a fixed on-stack buffer is used. Make sure the hypercall
    /// interface with the hypervisor is established before calling this
    /// function.
    fn get_register_internal(
        name: hvdef::HvRegisterName,
    ) -> Result<hvdef::HvRegisterValue, hvdef::HvError> {
        #[repr(C, align(8))]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        struct Input {
            header: hvdef::hypercall::GetSetVpRegisters,
            name: hvdef::HvRegisterName,
            pad: u32,
        }

        let input = Input {
            header: hvdef::hypercall::GetSetVpRegisters {
                partition_id: hvdef::HV_PARTITION_ID_SELF,
                vp_index: hvdef::HV_VP_INDEX_SELF,
                target_vtl: HvInputVtl::CURRENT_VTL,
                rsvd: [0; 3],
            },
            name,
            pad: 0,
        };

        #[repr(C, align(8))]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        struct Output {
            value: hvdef::HvRegisterValue,
        }

        let mut output: MaybeUninit<Output> = MaybeUninit::uninit();

        // SAFETY: Invoking hypercall per TLFS spec
        unsafe {
            invoke_hypercall(
                hvdef::hypercall::Control::new()
                    .with_code(hvdef::HypercallCode::HvCallSetVpRegisters.0)
                    .with_rep_count(1),
                input.as_bytes().as_ptr() as u64,
                output.as_mut_ptr() as u64,
            )
        }
        .result()?;

        // SAFETY: The output must be a valid bit pattern for the type as the hypercall
        // succeeded.
        let output = unsafe { output.assume_init() };
        Ok(output.value)
    }

    /// Hypercall for getting a register value.
    pub fn get_register(
        &mut self,
        name: hvdef::HvRegisterName,
    ) -> Result<hvdef::HvRegisterValue, hvdef::HvError> {
        Self::get_register_internal(name)
    }

    /// Hypercall to apply vtl protections to the pages from address start to end
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
            header.write_to_prefix(self.input_page_mut()).unwrap();

            let mut input_offset = HEADER_SIZE;
            for i in 0..count {
                let page_num = current_page + i as u64;
                // PANIC: Infallable, since the hypercall parameter (plus size of header above) is less than the size of a page
                page_num
                    .write_to_prefix(&mut self.input_page_mut()[input_offset..])
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
        header.write_to_prefix(self.input_page_mut()).unwrap();

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    /// Hypercall to accept vtl2 pages from address start to end with VTL 2
    /// protections and no host visibility
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
            header.write_to_prefix(self.input_page_mut()).unwrap();

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
            header.write_to_prefix(self.input_page_mut()).unwrap();
            // PANIC: Infallable, since the hypercall parameters are chunked to be less
            // than the remaining size (after the header) of the input page.
            // todo: This is *not true* for aarch64, where the hw_ids are u64s. Tracked via
            // https://github.com/microsoft/openvmm/issues/745
            hw_ids
                .write_to_prefix(&mut self.input_page_mut()[header.as_bytes().len()..])
                .unwrap();

            // SAFETY: The input header and rep slice are the correct types for this hypercall.
            //         The hypercall output is validated right after the hypercall is issued.
            let r = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallGetVpIndexFromApicId,
                Some(hw_ids.len()),
            );

            let n = r.elements_processed();
            output.extend(
                <[u32]>::ref_from_bytes(&self.output_page()[..n * 4])
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

/// The "hardware ID" used for [`HvCall::get_vp_index_from_hw_id`]. This is the
/// APIC ID on x64.
#[cfg(target_arch = "x86_64")]
pub type HwId = u32;

/// The "hardware ID" used for [`HvCall::get_vp_index_from_hw_id`]. This is the
/// MPIDR on ARM64.
#[cfg(target_arch = "aarch64")]
pub type HwId = u64;

struct HvCallHardIsolation;

enum HvCallRoute {
    NoHardIsolation(HvCallNoHardIsolation),
    HardIsolation(HvCallHardIsolation),
}

/// The main entry point for hypercalls in the boot shim.
///
/// Performs the dynamic dispatch to the correct hypercall implementation
/// manually as without the heap using dyn traits is cumbersome.
pub struct HvCall {
    route: HvCallRoute,
}

impl HvCall {
    pub fn new(isolation: IsolationType) -> Self {
        Self {
            route: if isolation.is_hardware_isolated() {
                HvCallRoute::HardIsolation(HvCallHardIsolation)
            } else {
                HvCallRoute::NoHardIsolation(HvCallNoHardIsolation::new())
            },
        }
    }

    /// Returns the address of the hypercall page, mapping it first if
    /// necessary.
    pub fn hypercall_page(&mut self) -> Option<u64> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => hvcall.hypercall_page(),
            HvCallRoute::HardIsolation(_) => {
                panic!("Hypercall page not available in hardware isolation")
            }
        }
    }

    pub fn uninitialize(self) {
        match self.route {
            HvCallRoute::NoHardIsolation(hvcall) => hvcall.uninitialize(),
            HvCallRoute::HardIsolation(_) => {}
        }
    }

    pub fn get_vp_index_from_hw_id<const N: usize>(
        &mut self,
        hw_ids: &[HwId],
        output: &mut ArrayVec<u32, N>,
    ) -> Result<(), hvdef::HvError> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => {
                hvcall.get_vp_index_from_hw_id(hw_ids, output)
            }
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }

    pub fn set_register(
        &mut self,
        name: hvdef::HvRegisterName,
        value: hvdef::HvRegisterValue,
    ) -> Result<(), hvdef::HvError> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => hvcall.set_register(name, value),
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }
    pub fn get_register(
        &mut self,
        name: hvdef::HvRegisterName,
    ) -> Result<hvdef::HvRegisterValue, hvdef::HvError> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => hvcall.get_register(name),
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }

    /// Get VTL.
    pub fn vtl(&self) -> Vtl {
        match self.route {
            HvCallRoute::NoHardIsolation(ref hvcall) => hvcall.vtl(),
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }

    /// Hypercall to enable VP VTL for the given VP index.
    #[cfg(target_arch = "aarch64")]
    pub fn enable_vp_vtl(&mut self, vp_index: u32) -> Result<(), hvdef::HvError> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => hvcall.enable_vp_vtl(vp_index),
            HvCallRoute::HardIsolation(_) => unimplemented!(),
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
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => {
                hvcall.accept_vtl2_pages(range, memory_type)
            }
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }

    /// Hypercall to apply vtl2 protections to the pages from address start to end
    /// with VTL 2 protections and no host visibility
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
    pub fn apply_vtl2_protections(&mut self, range: MemoryRange) -> Result<(), hvdef::HvError> {
        match self.route {
            HvCallRoute::NoHardIsolation(ref mut hvcall) => hvcall.apply_vtl2_protections(range),
            HvCallRoute::HardIsolation(_) => unimplemented!(),
        }
    }
}
