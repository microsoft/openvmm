// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall infrastructure.

// UNSAFETY: This module contains unsafe code to perform low-level operations such as invoking hypercalls
#![expect(unsafe_code)]

use core::mem::size_of;
use core::sync::atomic::AtomicU16;
use core::sync::atomic::Ordering;

use hvdef::HV_PAGE_SIZE;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::EnablePartitionVtlFlags;
use hvdef::hypercall::HvInputVtl;
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::invoke_hypercall;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Page-aligned, page-sized buffer for use with hypercalls
#[repr(C, align(4096))]
pub(crate) struct HvcallPage {
    pub(crate) buffer: [u8; HV_PAGE_SIZE as usize],
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

/// Hypercall interface.
pub struct HvCall {
    pub(crate) input_page: HvcallPage,
    pub(crate) output_page: HvcallPage,
    initialized: bool,
}

static HV_PAGE_INIT_STATUS: AtomicU16 = AtomicU16::new(0);

impl HvCall {
    /// Hypercall to apply vtl protections (NO ACCESS) to the pages from address start to end
    pub fn apply_vtl_protections(
        &mut self,
        range: MemoryRange,
        vtl: Vtl,
    ) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::ModifyVtlProtectionMask>();
        const MAX_INPUT_ELEMENTS: usize = (HV_PAGE_SIZE as usize - HEADER_SIZE) / size_of::<u64>();

        let header = hvdef::hypercall::ModifyVtlProtectionMask {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            map_flags: hvdef::HV_MAP_GPA_PERMISSIONS_NONE,
            target_vtl: HvInputVtl::new()
                .with_target_vtl_value(vtl.into())
                .with_use_target_vtl(true),
            reserved: [0; 3],
        };

        let mut current_page = range.start_4k_gpn();
        while current_page < range.end_4k_gpn() {
            let remaining_pages = range.end_4k_gpn() - current_page;
            let count = remaining_pages.min(MAX_INPUT_ELEMENTS as u64);

            let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

            let mut input_offset = HEADER_SIZE;
            for i in 0..count {
                let page_num = current_page + i;
                let _ = page_num.write_to_prefix(&mut self.input_page().buffer[input_offset..]);
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

    /// Makes a hypercall.
    /// rep_count is Some for rep hypercalls
    pub(crate) fn dispatch_hvcall(
        &mut self,
        code: hvdef::HypercallCode,
        rep_count: Option<usize>,
    ) -> hvdef::hypercall::HypercallOutput {
        let control: hvdef::hypercall::Control = hvdef::hypercall::Control::new()
            .with_code(code.0)
            .with_rep_count(rep_count.unwrap_or_default());

        // SAFETY: Invoking hypercall per TLFS spec
        unsafe {
            invoke_hypercall(
                control,
                self.input_page().address(),
                self.output_page().address(),
            )
        }
    }

    /// Enables a VTL for the specified partition.
    pub fn enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
    ) -> Result<(), hvdef::HvError> {
        let flags: EnablePartitionVtlFlags = EnablePartitionVtlFlags::new()
            .with_enable_mbec(false)
            .with_enable_supervisor_shadow_stack(false);

        let header = hvdef::hypercall::EnablePartitionVtl {
            partition_id,
            target_vtl: target_vtl.into(),
            flags,
            reserved_z0: 0,
            reserved_z1: 0,
        };

        let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnablePartitionVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    /// Enables VTL protection for the specified VTL.
    pub fn enable_vtl_protection(&mut self, vtl: HvInputVtl) -> Result<(), hvdef::HvError> {
        let mut hvreg: HvRegisterVsmPartitionConfig = HvRegisterVsmPartitionConfig::new();
        hvreg.set_enable_vtl_protection(true);
        hvreg.set_default_vtl_protection_mask(0xF);
        let bits = hvreg.into_bits();
        let hvre: HvRegisterValue = HvRegisterValue::from(bits);
        self.set_register(
            HvX64RegisterName::VsmPartitionConfig.into(),
            hvre,
            Some(vtl),
        )
    }

    /// Hypercall for getting a register value.
    pub fn get_register(
        &mut self,
        name: hvdef::HvRegisterName,
        vtl: Option<HvInputVtl>,
    ) -> Result<HvRegisterValue, hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: vtl.unwrap_or(HvInputVtl::CURRENT_VTL),
            rsvd: [0; 3],
        };

        let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());
        let _ = name.write_to_prefix(&mut self.input_page().buffer[HEADER_SIZE..]);

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallGetVpRegisters, Some(1));
        output.result()?;
        let value = HvRegisterValue::read_from_prefix(&self.output_page().buffer).unwrap();

        Ok(value.0)
    }

    /// Initializes the hypercall interface.
    pub fn initialize(&mut self) {
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        // This is an idempotent operation, so we can call it multiple times.
        // we proceed and initialize the hypercall interface because we don't know the current vtl
        // This prohibit us to call this selectively for new VTLs
        crate::arch::hypercall::initialize(guest_os_id.into());

        HV_PAGE_INIT_STATUS.fetch_add(1, Ordering::SeqCst);
        self.initialized = true;
    }

    /// Returns a mutable reference to the hypercall input page.
    pub(crate) fn input_page(&mut self) -> &mut HvcallPage {
        &mut self.input_page
    }

    /// Creates a new `HvCall` instance.
    pub const fn new() -> Self {
        HvCall {
            input_page: HvcallPage::new(),
            output_page: HvcallPage::new(),
            initialized: false,
        }
    }

    /// Returns a mutable reference to the hypercall output page.
    pub(crate) fn output_page(&mut self) -> &mut HvcallPage {
        &mut self.output_page
    }

    /// Hypercall for setting a register to a value.
    pub fn set_register(
        &mut self,
        name: hvdef::HvRegisterName,
        value: HvRegisterValue,
        vtl: Option<HvInputVtl>,
    ) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl: vtl.unwrap_or(HvInputVtl::CURRENT_VTL),
            rsvd: [0; 3],
        };

        let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

        let reg = hvdef::hypercall::HvRegisterAssoc {
            name,
            pad: Default::default(),
            value,
        };

        let _ = reg.write_to_prefix(&mut self.input_page().buffer[HEADER_SIZE..]);

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallSetVpRegisters, Some(1));

        output.result()
    }

    /// call to initialize the hypercall interface
    pub fn uninitialize(&mut self) {
        crate::arch::hypercall::uninitialize();
    }

    /// Returns the environment's VTL.
    pub fn vtl(&mut self) -> Vtl {
        self.get_register(hvdef::HvAllArchRegisterName::VsmVpStatus.into(), None)
            .map_or(Vtl::Vtl0, |status| {
                hvdef::HvRegisterVsmVpStatus::from(status.as_u64())
                    .active_vtl()
                    .try_into()
                    .unwrap()
            })
    }
}

impl Drop for HvCall {
    fn drop(&mut self) {
        if self.initialized {
            let seq = HV_PAGE_INIT_STATUS.fetch_sub(1, Ordering::SeqCst);
            if seq == 1 {
                self.uninitialize();
            }
        }
    }
}
