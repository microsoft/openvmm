// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall infrastructure.

#![allow(dead_code)]
use core::{
    arch::asm,
    mem::size_of,
    sync::atomic::{AtomicU16, Ordering},
};

use arrayvec::ArrayVec;
use hvdef::{
    hypercall::{EnablePartitionVtlFlags, HvInputVtl, InitialVpContextX64},
    HvRegisterValue, HvRegisterVsmPartitionConfig, HvX64RegisterName, HvX64SegmentRegister, Vtl,
    HV_PAGE_SIZE,
};
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::{invoke_hypercall, HYPERCALL_PAGE};
use zerocopy::{FromBytes, IntoBytes};

/// Page-aligned, page-sized buffer for use with hypercalls
#[repr(C, align(4096))]
struct HvcallPage {
    buffer: [u8; HV_PAGE_SIZE as usize],
}

#[inline(never)]
pub fn invoke_hypercall_vtl(control: hvdef::hypercall::Control) {
    // SAFETY: the caller guarantees the safety of this operation.
    unsafe {
        core::arch::asm! {
            "call {hypercall_page}",
            hypercall_page = sym HYPERCALL_PAGE,
            inout("rcx") u64::from(control) => _,
            in("rdx") 0,
            in("rax") 0,
        }
    }
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

/// Provides mechanisms to invoke hypercalls within the boot shim.
///
/// This module defines the `HvCall` struct and associated methods to interact with
/// hypervisor functionalities through hypercalls. It includes utilities for managing
/// hypercall pages, setting and getting virtual processor (VP) registers, enabling
/// VTL (Virtual Trust Levels), and applying memory protections.
///
/// # Overview
///
/// - **Hypercall Pages**: Manages page-aligned buffers for hypercall input and output.
/// - **VP Registers**: Provides methods to set and get VP registers.
/// - **VTL Management**: Includes methods to enable VTLs, apply VTL protections, and
///   manage VTL-specific operations.
/// - **Memory Protections**: Supports applying VTL protections and accepting VTL2 pages.
///
/// # Safety
///
/// Many methods in this module involve unsafe operations, such as invoking hypercalls
/// or interacting with low-level memory structures. The caller must ensure the safety
/// of these operations by adhering to the requirements of the hypervisor and the
/// underlying architecture.
///
/// # Usage
///
/// This module is designed for use in single-threaded environments, such as the boot
/// shim. It uses static buffers for hypercall pages, so it is not thread-safe.
///
/// # Features
///
/// - **Architecture-Specific Implementations**: Some methods are only available for
///   specific architectures (e.g., `x86_64` or `aarch64`).
/// - **Error Handling**: Methods return `Result` types to handle hypervisor errors.
///
/// # Examples
///
/// ```rust
/// let mut hv_call = HvCall::new();
/// hv_call.initialize();
/// let vtl = hv_call.vtl();
/// println!("Current VTL: {:?}", vtl);
/// hv_call.uninitialize();
/// ```
///
/// # Modules and Types
///
/// - `HvCall`: Main struct for managing hypercalls.
/// - `HvcallPage`: Struct for page-aligned buffers.
/// - `HwId`: Type alias for hardware IDs (APIC ID on `x86_64`, MPIDR on `aarch64`).
///
/// # Notes
///
/// - This module assumes the presence of a hypervisor that supports the required
///   hypercalls.
/// - The boot shim must ensure that hypercalls are invoked in a valid context.
/// Internally uses static buffers for the hypercall page, the input
/// page, and the output page, so this should not be used in any
/// multi-threaded capacity (which the boot shim currently is not).
pub struct HvCall {
    input_page: HvcallPage,
    output_page: HvcallPage,
}

static HV_PAGE_INIT_STATUS: AtomicU16 = AtomicU16::new(0);

#[expect(unsafe_code)]
impl HvCall {
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

            let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

            let output = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallAcceptGpaPages,
                Some(count as usize),
            );

            output.result()?;

            current_page += count;
        }

        Ok(())
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

    /// Hypercall to apply vtl protections to the pages from address start to end
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
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
    fn dispatch_hvcall(
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
        // let hvreg = self.get_register(HvX64RegisterName::VsmPartitionConfig.into(), Some(vtl))?;
        let mut hvreg: HvRegisterVsmPartitionConfig = HvRegisterVsmPartitionConfig::new();
        hvreg.set_enable_vtl_protection(true);
        // hvreg.set_intercept_page(true);
        hvreg.set_default_vtl_protection_mask(0xF);
        // hvreg.set_intercept_enable_vtl_protection(true);
        let bits = hvreg.into_bits();
        let hvre: HvRegisterValue = HvRegisterValue::from(bits);
        self.set_register(
            HvX64RegisterName::VsmPartitionConfig.into(),
            hvre,
            Some(vtl),
        )
    }

    #[cfg(target_arch = "x86_64")]
    /// Enables a VTL for a specific virtual processor (VP) on x86_64.
    pub fn enable_vp_vtl(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextX64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::EnableVpVtlX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: target_vtl.into(),
            reserved: [0; 3],
            vp_vtl_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
        };

        header
            .write_to_prefix(self.input_page().buffer.as_mut_slice())
            .expect("size of enable_vp_vtl header is not correct");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
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
            vp_vtl_context: zerocopy::FromZeroes::new_zeroed(),
        };

        header.write_to_prefix(self.input_page().buffer.as_mut_slice());

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        match output.result() {
            Ok(()) | Err(hvdef::HvError::VtlAlreadyEnabled) => Ok(()),
            err => err,
        }
    }

    fn get_segment_descriptor(segment_reg: &str) -> HvX64SegmentRegister {
        unsafe {
            use core::arch::asm;
            let mut descriptor = HvX64SegmentRegister {
                base: 0,
                limit: 0,
                selector: 0,
                attributes: 0,
            };
            match segment_reg {
                "cs" => {
                    asm!("mov {0:x}, cs", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "ds" => {
                    asm!("mov {0:x}, ds", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "es" => {
                    asm!("mov {0:x}, es", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "ss" => {
                    asm!("mov {0:x}, ss", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "fs" => {
                    asm!("mov {0:x}, fs", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "gs" => {
                    asm!("mov {0:x}, gs", out(reg) descriptor.selector, options(nomem, nostack))
                }
                "tr" => asm!("str {0:x}", out(reg) descriptor.selector, options(nomem, nostack)),
                _ => panic!("Invalid segment register"),
            }

            // For FS and GS in 64-bit mode, we can get the base directly via MSRs
            if segment_reg == "fs" {
                let mut base_low: u32;
                let mut base_high: u32;
                asm!(
                    "mov ecx, 0xC0000100", // FS_BASE MSR
                    "rdmsr",
                    out("eax") base_low,
                    out("edx") base_high,
                    options(nomem, nostack)
                );
                descriptor.base = ((base_high as u64) << 32) | (base_low as u64);
            } else if segment_reg == "gs" {
                let mut base_low: u32;
                let mut base_high: u32;
                asm!(
                    "mov ecx, 0xC0000101", // GS_BASE MSR
                    "rdmsr",
                    out("eax") base_low,
                    out("edx") base_high,
                    options(nomem, nostack)
                );
                descriptor.base = ((base_high as u64) << 32) | (base_low as u64);
            } else {
                // For other segments, need to look up in GDT/LDT
                // Allocate 10 bytes for storing GDTR/LDTR content
                let mut descriptor_table = [0u8; 10];

                // Determine if selector is in GDT or LDT
                let table_indicator = descriptor.selector & 0x04;

                if table_indicator == 0 {
                    // Get GDT base
                    asm!("sgdt [{}]", in(reg) descriptor_table.as_mut_ptr(), options(nostack));
                } else {
                    // Get LDT base
                    asm!("sldt [{}]", in(reg) descriptor_table.as_mut_ptr(), options(nostack));
                }

                // Extract GDT/LDT base (bytes 2-9 of descriptor_table)
                let table_base = u64::from_ne_bytes([
                    descriptor_table[2],
                    descriptor_table[3],
                    descriptor_table[4],
                    descriptor_table[5],
                    descriptor_table[6],
                    descriptor_table[7],
                    descriptor_table[8],
                    descriptor_table[9],
                ]);

                // Calculate descriptor entry address
                let index = (descriptor.selector & 0xFFF8) as u64; // Clear RPL and TI bits
                let desc_addr = table_base + index;

                // Read the 8-byte descriptor
                let desc_bytes = alloc::slice::from_raw_parts(desc_addr as *const u8, 8);
                let desc_low = u32::from_ne_bytes([
                    desc_bytes[0],
                    desc_bytes[1],
                    desc_bytes[2],
                    desc_bytes[3],
                ]);
                let desc_high = u32::from_ne_bytes([
                    desc_bytes[4],
                    desc_bytes[5],
                    desc_bytes[6],
                    desc_bytes[7],
                ]);

                // Extract base (bits 16-39 and 56-63)
                let base_low = ((desc_low >> 16) & 0xFFFF) as u64;
                let base_mid = (desc_high & 0xFF) as u64;
                let base_high = ((desc_high >> 24) & 0xFF) as u64;
                descriptor.base = base_low | (base_mid << 16) | (base_high << 24);

                // Extract limit (bits 0-15 and 48-51)
                let limit_low = desc_low & 0xFFFF;
                let limit_high = (desc_high >> 16) & 0x0F;
                descriptor.limit = limit_low | (limit_high << 16);

                // Extract attributes (bits 40-47 and 52-55)
                let attr_low = (desc_high >> 8) & 0xFF;
                let attr_high = (desc_high >> 20) & 0x0F;
                descriptor.attributes = (attr_low as u16) | ((attr_high as u16) << 8);

                // If G bit is set (bit 55), the limit is in 4K pages
                if (desc_high & 0x00800000) != 0 {
                    descriptor.limit = (descriptor.limit << 12) | 0xFFF;
                }

                // For TR, which is a system segment in 64-bit mode, read the second 8 bytes to get the high 32 bits of base
                if segment_reg == "tr" {
                    // Check if it's a system descriptor (bit 4 of attributes is 0)
                    if (descriptor.attributes & 0x10) == 0 {
                        // Read the next 8 bytes of the descriptor (high part of 16-byte descriptor)
                        let high_desc_bytes =
                            alloc::slice::from_raw_parts((desc_addr + 8) as *const u8, 8);
                        let high_base = u32::from_ne_bytes([
                            high_desc_bytes[0],
                            high_desc_bytes[1],
                            high_desc_bytes[2],
                            high_desc_bytes[3],
                        ]) as u64;

                        // Combine with existing base to get full 64-bit base
                        descriptor.base |= high_base << 32;
                    }
                }
            }

            descriptor
        }
    }

    #[cfg(target_arch = "x86_64")]
    /// Hypercall to get the current VTL VP context
    pub fn get_current_vtl_vp_context(&mut self) -> Result<InitialVpContextX64, hvdef::HvError> {
        use minimal_rt::arch::msr::read_msr;
        use zerocopy::FromZeros;
        let mut context: InitialVpContextX64 = FromZeros::new_zeroed();

        let rsp: u64;
        unsafe { asm!("mov {0:r}, rsp", out(reg) rsp, options(nomem, nostack)) };

        let cr0;
        unsafe { asm!("mov {0:r}, cr0", out(reg) cr0, options(nomem, nostack)) };
        let cr3;
        unsafe { asm!("mov {0:r}, cr3", out(reg) cr3, options(nomem, nostack)) };
        let cr4;
        unsafe { asm!("mov {0:r}, cr4", out(reg) cr4, options(nomem, nostack)) };

        let rflags: u64;
        unsafe {
            asm!(
                "pushfq",
                "pop {0}",
                out(reg) rflags,
            );
        }

        context.cr0 = cr0;
        context.cr3 = cr3;
        context.cr4 = cr4;

        context.rsp = rsp;
        context.rip = 0;

        context.rflags = rflags;

        // load segment registers

        let cs: u16;
        let ss: u16;
        let ds: u16;
        let es: u16;
        let fs: u16;
        let gs: u16;

        unsafe {
            asm!("
                mov {0:x}, cs
                mov {1:x}, ss
                mov {2:x}, ds
                mov {3:x}, es
                mov {4:x}, fs
                mov {5:x}, gs
            ", out(reg) cs, out(reg) ss, out(reg) ds, out(reg) es, out(reg) fs, out(reg) gs, options(nomem, nostack))
        }

        context.cs.selector = cs;
        context.cs.attributes = 0xA09B;
        context.cs.limit = 0xFFFFFFFF;

        context.ss.selector = ss;
        context.ss.attributes = 0xC093;
        context.ss.limit = 0xFFFFFFFF;

        context.ds.selector = ds;
        context.ds.attributes = 0xC093;
        context.ds.limit = 0xFFFFFFFF;

        context.es.selector = es;
        context.es.attributes = 0xC093;
        context.es.limit = 0xFFFFFFFF;

        context.fs.selector = fs;
        context.fs.attributes = 0xC093;
        context.fs.limit = 0xFFFFFFFF;

        context.gs.selector = gs;
        context.gs.attributes = 0xC093;
        context.gs.limit = 0xFFFFFFFF;

        context.tr.selector = 0;
        context.tr.attributes = 0x8B;
        context.tr.limit = 0xFFFF;

        let idt = x86_64::instructions::tables::sidt();
        context.idtr.base = idt.base.as_u64();
        context.idtr.limit = idt.limit;

        let gdtr = x86_64::instructions::tables::sgdt();
        context.gdtr.base = gdtr.base.as_u64();
        context.gdtr.limit = gdtr.limit;

        let efer = unsafe { read_msr(0xC0000080) };
        context.efer = efer;

        log::info!("Current VTL VP context: {:?}", context);
        Ok(context)
    }

    /// Hypercall for setting a register to a value.
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
            let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());
            let _ =
                hw_ids.write_to_prefix(&mut self.input_page().buffer[header.as_bytes().len()..]);

            // SAFETY: The input header and rep slice are the correct types for this hypercall.
            //         The hypercall output is validated right after the hypercall is issued.
            let r = self.dispatch_hvcall(
                hvdef::HypercallCode::HvCallGetVpIndexFromApicId,
                Some(hw_ids.len()),
            );

            let n = r.elements_processed();

            output.extend(
                <[u32]>::ref_from_bytes(&mut self.output_page().buffer[..n * 4])
                    .unwrap()
                    .iter()
                    .copied(),
            );
            r.result()?;
            assert_eq!(n, hw_ids.len());
        }

        Ok(())
    }

    /// Initializes the hypercall interface.
    pub fn initialize(&mut self) {
        // TODO: revisit os id value. For now, use 1 (which is what UEFI does)
        let guest_os_id = hvdef::hypercall::HvGuestOsMicrosoft::new().with_os_id(1);
        // This is an idempotent operation, so we can call it multiple times.
        // we proceed and initialize the hypercall interface because we don't know the current vtl
        // This prohibit us to call this selectively for new VTLs
        crate::arch::hypercall::initialize(guest_os_id.into());

        HV_PAGE_INIT_STATUS.fetch_add(1, Ordering::SeqCst);
    }

    /// Returns a mutable reference to the hypercall input page.
    fn input_page(&mut self) -> &mut HvcallPage {
        &mut self.input_page
    }

    /// Creates a new `HvCall` instance.
    pub const fn new() -> Self {
        HvCall {
            input_page: HvcallPage::new(),
            output_page: HvcallPage::new(),
        }
    }

    /// Returns a mutable reference to the hypercall output page.
    fn output_page(&mut self) -> &mut HvcallPage {
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

    /// Sets multiple virtual processor (VP) registers for a given VP and VTL.
    pub fn set_vp_registers(
        &mut self,
        vp: u32,
        vtl: Option<HvInputVtl>,
        vp_context: Option<InitialVpContextX64>,
    ) -> Result<(), hvdef::HvError> {
        const HEADER_SIZE: usize = size_of::<hvdef::hypercall::GetSetVpRegisters>();

        let header = hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: vp,
            target_vtl: vtl.unwrap_or(HvInputVtl::CURRENT_VTL),
            rsvd: [0; 3],
        };

        let _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

        let mut input_offset = HEADER_SIZE;

        let mut count = 0;
        let mut write_reg = |reg_name: hvdef::HvRegisterName, reg_value: HvRegisterValue| {
            let reg = hvdef::hypercall::HvRegisterAssoc {
                name: reg_name,
                pad: Default::default(),
                value: reg_value,
            };

            let _ = reg.write_to_prefix(&mut self.input_page().buffer[input_offset..]);

            input_offset += size_of::<hvdef::hypercall::HvRegisterAssoc>();
            count += 1;
        };
        // pub msr_cr_pat: u64,

        write_reg(
            HvX64RegisterName::Cr0.into(),
            vp_context.unwrap().cr0.into(),
        );
        write_reg(
            HvX64RegisterName::Cr3.into(),
            vp_context.unwrap().cr3.into(),
        );
        write_reg(
            HvX64RegisterName::Cr4.into(),
            vp_context.unwrap().cr4.into(),
        );
        write_reg(
            HvX64RegisterName::Rip.into(),
            vp_context.unwrap().rip.into(),
        );
        write_reg(
            HvX64RegisterName::Rsp.into(),
            vp_context.unwrap().rsp.into(),
        );
        write_reg(
            HvX64RegisterName::Rflags.into(),
            vp_context.unwrap().rflags.into(),
        );
        write_reg(HvX64RegisterName::Cs.into(), vp_context.unwrap().cs.into());
        write_reg(HvX64RegisterName::Ss.into(), vp_context.unwrap().ss.into());
        write_reg(HvX64RegisterName::Ds.into(), vp_context.unwrap().ds.into());
        write_reg(HvX64RegisterName::Es.into(), vp_context.unwrap().es.into());
        write_reg(HvX64RegisterName::Fs.into(), vp_context.unwrap().fs.into());
        write_reg(HvX64RegisterName::Gs.into(), vp_context.unwrap().gs.into());
        write_reg(
            HvX64RegisterName::Gdtr.into(),
            vp_context.unwrap().gdtr.into(),
        );
        write_reg(
            HvX64RegisterName::Idtr.into(),
            vp_context.unwrap().idtr.into(),
        );
        write_reg(
            HvX64RegisterName::Ldtr.into(),
            vp_context.unwrap().ldtr.into(),
        );
        write_reg(HvX64RegisterName::Tr.into(), vp_context.unwrap().tr.into());
        write_reg(
            HvX64RegisterName::Efer.into(),
            vp_context.unwrap().efer.into(),
        );

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallSetVpRegisters, Some(count));

        output.result()
    }

    #[cfg(target_arch = "x86_64")]
    /// Starts a virtual processor (VP) with the specified VTL and context on x86_64.
    pub fn start_virtual_processor(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextX64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::StartVirtualProcessorX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: target_vtl.into(),
            vp_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
            rsvd0: 0u8,
            rsvd1: 0u16,
        };

        header
            .write_to_prefix(self.input_page().buffer.as_mut_slice())
            .expect("size of start_virtual_processor header is not correct");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallStartVirtualProcessor, None);
        match output.result() {
            Ok(()) => Ok(()),
            err => panic!("Failed to start virtual processor: {:?}", err),
        }
    }

    /// Call before jumping to kernel.
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

    #[inline(never)]
    /// Invokes the HvCallVtlCall hypercall.
    pub fn vtl_call() {
        let control: hvdef::hypercall::Control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallVtlCall.0)
            .with_rep_count(0);
        invoke_hypercall_vtl(control);
    }

    #[inline(never)]
    /// Invokes the HvCallVtlReturn hypercall.
    pub fn vtl_return() {
        let control: hvdef::hypercall::Control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallVtlReturn.0)
            .with_rep_count(0);
        invoke_hypercall_vtl(control);
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

impl Drop for HvCall {
    fn drop(&mut self) {
        let seq = HV_PAGE_INIT_STATUS.fetch_sub(1, Ordering::SeqCst);
        if seq == 0 {
            self.uninitialize();
        }
    }
}
