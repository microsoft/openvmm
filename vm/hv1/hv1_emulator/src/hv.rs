// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor MSR emulation.
//!
//! In the future, this will be extended to include virtual processor registers.

use super::synic::GlobalSynic;
use super::synic::ProcessorSynic;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hv1_structs::VtlArray;
use hvdef::HvRegisterVpAssistPage;
use hvdef::HvVpVtlControl;
use hvdef::HvVtlEntryReason;
use hvdef::HV_PAGE_SIZE;
use hvdef::HV_PAGE_SIZE_USIZE;
use hvdef::HV_REFERENCE_TSC_SEQUENCE_INVALID;
use inspect::Inspect;
use parking_lot::Mutex;
use std::mem::offset_of;
use std::sync::Arc;
use virt::x86::MsrError;
use vm_topology::processor::VpIndex;
use vmcore::reference_time_source::ReferenceTimeSource;
use x86defs::cpuid::Vendor;
use zerocopy::FromZeros;

/// The partition-wide hypervisor state.
#[derive(Inspect)]
pub struct GlobalHv {
    #[inspect(flatten)]
    partition_state: Arc<GlobalHvState>,
    /// Mutable state, per VTL
    vtl_mutable_state: VtlArray<Arc<Mutex<MutableHvState>>, 2>,
    /// The per-vtl synic state.
    pub synic: VtlArray<GlobalSynic, 2>,
}

#[derive(Inspect)]
struct GlobalHvState {
    #[inspect(display)]
    vendor: Vendor,
    #[inspect(skip)]
    ref_time: Box<dyn ReferenceTimeSource>,
    tsc_frequency: u64,
    is_ref_time_backed_by_tsc: bool,
}

#[derive(Inspect)]
struct MutableHvState {
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    hypercall: hvdef::hypercall::MsrHypercallContents,
    #[inspect(skip)]
    hypercall_protector: Option<Box<dyn VtlProtectHypercallOverlay>>,
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    guest_os_id: hvdef::hypercall::HvGuestOsId,
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    reference_tsc: hvdef::HvRegisterReferenceTsc,
    tsc_sequence: u32,
}

impl MutableHvState {
    fn new(protector: Option<Box<dyn VtlProtectHypercallOverlay>>) -> Self {
        Self {
            hypercall: hvdef::hypercall::MsrHypercallContents::new(),
            hypercall_protector: protector,

            guest_os_id: hvdef::hypercall::HvGuestOsId::new(),
            reference_tsc: hvdef::HvRegisterReferenceTsc::new(),
            tsc_sequence: 0,
        }
    }

    fn reset(&mut self) {
        if let Some(p) = self.hypercall_protector.as_mut() {
            p.disable_overlay();
        }
        self.hypercall = hvdef::hypercall::MsrHypercallContents::new();
        self.guest_os_id = hvdef::hypercall::HvGuestOsId::new();
        self.reference_tsc = hvdef::HvRegisterReferenceTsc::new();
        self.tsc_sequence = 0;
    }
}

/// Parameters used when constructing a [`GlobalHv`].
pub struct GlobalHvParams {
    /// The maximum VP count for the VM.
    pub max_vp_count: u32,
    /// The vendor of the virtual processor.
    pub vendor: Vendor,
    /// The TSC frequency.
    pub tsc_frequency: u64,
    /// The reference time system to use.
    pub ref_time: Box<dyn ReferenceTimeSource>,
    /// Manages VTL protections on the VTL0 hypercall overlay page
    pub hypercall_page_protectors: VtlArray<Option<Box<dyn VtlProtectHypercallOverlay>>, 2>,
}

impl GlobalHv {
    /// Returns a new hypervisor emulator instance.
    pub fn new(params: GlobalHvParams) -> Self {
        Self {
            partition_state: Arc::new(GlobalHvState {
                vendor: params.vendor,
                tsc_frequency: params.tsc_frequency,
                is_ref_time_backed_by_tsc: params.ref_time.is_backed_by_tsc(),
                ref_time: params.ref_time,
            }),
            vtl_mutable_state: params
                .hypercall_page_protectors
                .map(|protector| Arc::new(Mutex::new(MutableHvState::new(protector)))),
            synic: VtlArray::from_fn(|_| GlobalSynic::new(params.max_vp_count)),
        }
    }

    /// Adds a virtual processor to the vtl.
    pub fn add_vp(
        &self,
        guest_memory: GuestMemory,
        vp_index: VpIndex,
        vtl: hvdef::Vtl,
    ) -> ProcessorVtlHv {
        ProcessorVtlHv {
            vp_index,
            partition_state: self.partition_state.clone(),
            vtl_state: self.vtl_mutable_state[vtl].clone(),
            synic: self.synic[vtl].add_vp(vp_index),
            vp_assist_page: 0.into(),
            guest_memory,
        }
    }

    /// Resets the global (but not per-processor) state.
    pub fn reset(&self) {
        for state in self.vtl_mutable_state.iter() {
            state.lock().reset();
        }
        // There is no global synic state to reset, since the synic is per-VP.
    }

    /// The current guest_os_id value.
    pub fn guest_os_id(&self, vtl: hvdef::Vtl) -> hvdef::hypercall::HvGuestOsId {
        self.vtl_mutable_state[vtl].lock().guest_os_id
    }
}

/// A virtual processor's per-VTL hypervisor state.
#[derive(Inspect)]
pub struct ProcessorVtlHv {
    vp_index: VpIndex,
    #[inspect(skip)]
    partition_state: Arc<GlobalHvState>,
    vtl_state: Arc<Mutex<MutableHvState>>,
    guest_memory: GuestMemory,
    /// The virtual processor's synic state.
    pub synic: ProcessorSynic,
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    vp_assist_page: HvRegisterVpAssistPage,
}

impl ProcessorVtlHv {
    /// The current reference time.
    pub fn ref_time_now(&self) -> u64 {
        self.partition_state.ref_time.now_100ns()
    }

    /// Resets the processor's state.
    pub fn reset(&mut self) {
        let Self {
            vp_index: _,
            partition_state: _,
            vtl_state: _,
            guest_memory: _,
            synic,
            vp_assist_page,
        } = self;

        synic.reset();
        *vp_assist_page = Default::default();
    }

    /// Emulates an MSR write for an HV#1 synthetic MSR.
    pub fn msr_write(&mut self, n: u32, v: u64) -> Result<(), MsrError> {
        match n {
            hvdef::HV_X64_MSR_GUEST_OS_ID => {
                self.vtl_state.lock().guest_os_id = v.into();
            }
            hvdef::HV_X64_MSR_HYPERCALL => {
                let mut mutable = self.vtl_state.lock();
                if mutable.hypercall.locked() {
                    return Err(MsrError::InvalidAccess);
                }
                let hc = hvdef::hypercall::MsrHypercallContents::from(v);
                if hc.reserved_p() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
                if hc.enable()
                    && (!mutable.hypercall.enable() || hc.gpn() != mutable.hypercall.gpn())
                {
                    let gpa = hc.gpn() * HV_PAGE_SIZE;
                    if let Err(err) = self.write_hypercall_page(gpa) {
                        tracelimit::warn_ratelimited!(
                            gpa,
                            error = &err as &dyn std::error::Error,
                            "failed to write hypercall page"
                        );
                        return Err(MsrError::InvalidAccess);
                    }

                    if let Some(p) = mutable.hypercall_protector.as_mut() {
                        p.change_overlay(hc.gpn());
                    }
                } else if !hc.enable() {
                    if let Some(p) = mutable.hypercall_protector.as_mut() {
                        p.disable_overlay();
                    }
                }
                mutable.hypercall = hc;
            }
            hvdef::HV_X64_MSR_VP_INDEX => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_TIME_REF_COUNT => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_REFERENCE_TSC => {
                let mut mutable = self.vtl_state.lock();
                let v = hvdef::HvRegisterReferenceTsc::from(v);
                if v.reserved_p() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
                if v.enable() && mutable.reference_tsc.gpn() != v.gpn() {
                    let gm = &self.guest_memory;
                    let gpa = v.gpn() * HV_PAGE_SIZE;
                    if let Err(err) = gm.write_plain(gpa, &HV_REFERENCE_TSC_SEQUENCE_INVALID) {
                        tracelimit::warn_ratelimited!(
                            gpa,
                            error = &err as &dyn std::error::Error,
                            "failed to write reference tsc page"
                        );
                        return Err(MsrError::InvalidAccess);
                    }
                    if self.partition_state.is_ref_time_backed_by_tsc {
                        // TDX TODO: offset might need to be included
                        let tsc_scale = (((10_000_000_u128) << 64)
                            / self.partition_state.tsc_frequency as u128)
                            as u64;
                        let reference_page = hvdef::HvReferenceTscPage {
                            tsc_scale,
                            ..FromZeros::new_zeroed()
                        };
                        if let Err(err) = gm.write_plain(gpa, &reference_page) {
                            tracelimit::warn_ratelimited!(
                                gpa,
                                error = &err as &dyn std::error::Error,
                                "failed to write reference tsc page"
                            );
                            return Err(MsrError::InvalidAccess);
                        }
                        mutable.tsc_sequence = mutable.tsc_sequence.wrapping_add(1);
                        if mutable.tsc_sequence == HV_REFERENCE_TSC_SEQUENCE_INVALID {
                            mutable.tsc_sequence = mutable.tsc_sequence.wrapping_add(1);
                        }
                        if let Err(err) = gm.write_plain(gpa, &mutable.tsc_sequence) {
                            tracelimit::warn_ratelimited!(
                                gpa,
                                error = &err as &dyn std::error::Error,
                                "failed to write reference tsc page"
                            );
                            return Err(MsrError::InvalidAccess);
                        }
                    }
                }

                mutable.reference_tsc = v;
            }
            hvdef::HV_X64_MSR_TSC_FREQUENCY => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_VP_ASSIST_PAGE => {
                if v & !u64::from(
                    HvRegisterVpAssistPage::new()
                        .with_enabled(true)
                        .with_gpa_page_number(!0 >> 12),
                ) != 0
                {
                    return Err(MsrError::InvalidAccess);
                }
                let vp_assist_page = HvRegisterVpAssistPage::from(v);

                // Clear the target page if it is being enabled or moved.
                if vp_assist_page.enabled()
                    && (!self.vp_assist_page.enabled()
                        || vp_assist_page.gpa_page_number()
                            != self.vp_assist_page.gpa_page_number())
                {
                    let gpa = vp_assist_page.gpa_page_number() * HV_PAGE_SIZE;
                    if let Err(err) = self.guest_memory.fill_at(gpa, 0, HV_PAGE_SIZE_USIZE) {
                        tracelimit::warn_ratelimited!(
                            gpa,
                            error = &err as &dyn std::error::Error,
                            "failed to clear vp assist page"
                        );
                        return Err(MsrError::InvalidAccess);
                    }
                }
                self.vp_assist_page = vp_assist_page;
            }
            msr @ hvdef::HV_X64_MSR_SCONTROL..=hvdef::HV_X64_MSR_STIMER3_COUNT => {
                self.synic.write_msr(&self.guest_memory, msr, v)?
            }
            _ => return Err(MsrError::Unknown),
        }
        Ok(())
    }

    fn write_hypercall_page(&self, gpa: u64) -> Result<(), GuestMemoryError> {
        let page_contents: &[u8] = if self.partition_state.vendor.is_amd_compatible() {
            &AMD_HYPERCALL_PAGE.page
        } else if self.partition_state.vendor.is_intel_compatible() {
            &INTEL_HYPERCALL_PAGE.page
        } else {
            unreachable!()
        };

        self.guest_memory.write_at(gpa, page_contents)?;

        // Fill the rest with int3 to catch invalid jumps into the page.
        let int3 = 0xcc;
        self.guest_memory.fill_at(
            gpa + page_contents.len() as u64,
            int3,
            HV_PAGE_SIZE_USIZE - page_contents.len(),
        )?;

        Ok(())
    }

    /// Gets the VSM code page offset register that corresponds to the hypercall
    /// page generated by this emulator.
    pub fn vsm_code_page_offsets(&self, bit64: bool) -> hvdef::HvRegisterVsmCodePageOffsets {
        // The code page offsets are the same for all VTLs.
        let page = if self.partition_state.vendor.is_amd_compatible() {
            &AMD_HYPERCALL_PAGE
        } else if self.partition_state.vendor.is_intel_compatible() {
            &INTEL_HYPERCALL_PAGE
        } else {
            unreachable!()
        };
        if bit64 {
            page.offsets64
        } else {
            page.offsets32
        }
    }

    /// Emulates an MSR read for an HV#1 synthetic MSR.
    pub fn msr_read(&self, msr: u32) -> Result<u64, MsrError> {
        let v = match msr {
            hvdef::HV_X64_MSR_GUEST_OS_ID => self.vtl_state.lock().guest_os_id.into(),
            hvdef::HV_X64_MSR_HYPERCALL => self.vtl_state.lock().hypercall.into(),
            hvdef::HV_X64_MSR_VP_INDEX => self.vp_index.index() as u64, // VP index
            hvdef::HV_X64_MSR_TIME_REF_COUNT => self.partition_state.ref_time.now_100ns(),
            hvdef::HV_X64_MSR_REFERENCE_TSC => self.vtl_state.lock().reference_tsc.into(),
            hvdef::HV_X64_MSR_TSC_FREQUENCY => self.partition_state.tsc_frequency,
            hvdef::HV_X64_MSR_VP_ASSIST_PAGE => self.vp_assist_page.into(),
            msr @ hvdef::HV_X64_MSR_SCONTROL..=hvdef::HV_X64_MSR_STIMER3_COUNT => {
                self.synic.read_msr(msr)?
            }
            _ => {
                return Err(MsrError::Unknown);
            }
        };
        Ok(v)
    }

    /// Returns the current value of the VP assist page register.
    pub fn vp_assist_page(&self) -> u64 {
        self.vp_assist_page.into()
    }

    /// Sets the lazy EOI bit in the VP assist page.
    ///
    /// If this returns true, the caller must call `clear_lazy_eoi` after the
    /// next VP exit but before manipulating the APIC.
    #[must_use]
    pub fn set_lazy_eoi(&mut self) -> bool {
        if !self.vp_assist_page.enabled() {
            return false;
        }

        let gpa = self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE
            + offset_of!(hvdef::HvVpAssistPage, apic_assist) as u64;

        let v = 1u32;

        match self.guest_memory.write_plain(gpa, &v) {
            Ok(()) => true,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write lazy eoi to assist page"
                );
                false
            }
        }
    }

    /// Clears the lazy EOI bit in the VP assist page.
    ///
    /// Must only be called if `set_lazy_eoi` returned true.
    ///
    /// If the bit was already clear, returns true; the caller must then send an
    /// EOI to the APIC.
    #[must_use]
    pub fn clear_lazy_eoi(&mut self) -> bool {
        let gpa = self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE
            + offset_of!(hvdef::HvVpAssistPage, apic_assist) as u64;

        let v: u32 = match self.guest_memory.read_plain(gpa) {
            Ok(v) => v,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to read lazy eoi from assist page"
                );
                return false;
            }
        };

        if v & 1 == 0 {
            // The guest cleared the bit. The caller will perform the EOI to the
            // APIC.
            true
        } else {
            // Clear the bit in case the EOI state changes before the guest runs
            // again.
            let v = v & !1;
            if let Err(err) = self.guest_memory.write_plain(gpa, &v) {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to clear lazy eoi from assist page"
                );
            }
            false
        }
    }

    /// Get the register values to restore on vtl return
    pub fn return_registers(&self) -> Result<[u64; 2], GuestMemoryError> {
        let gpa = (self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE)
            + offset_of!(hvdef::HvVpAssistPage, vtl_control) as u64
            + offset_of!(HvVpVtlControl, registers) as u64;

        self.guest_memory.read_plain(gpa)
    }

    /// Set the reason for the vtl return into the vp assist page
    pub fn set_return_reason(&self, reason: HvVtlEntryReason) -> Result<(), GuestMemoryError> {
        let gpa = (self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE)
            + offset_of!(hvdef::HvVpAssistPage, vtl_control) as u64
            + offset_of!(HvVpVtlControl, entry_reason) as u64;

        self.guest_memory.write_plain(gpa, &(reason.0))
    }

    /// Gets whether VINA is currently asserted.
    pub fn vina_asserted(&self) -> Result<bool, GuestMemoryError> {
        let gpa = (self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE)
            + offset_of!(hvdef::HvVpAssistPage, vtl_control) as u64
            + offset_of!(HvVpVtlControl, vina_status) as u64;

        self.guest_memory.read_plain(gpa).map(|v: u8| v != 0)
    }

    /// Sets whether VINA is currently asserted.
    pub fn set_vina_asserted(&self, value: bool) -> Result<(), GuestMemoryError> {
        let gpa = (self.vp_assist_page.gpa_page_number() * HV_PAGE_SIZE)
            + offset_of!(hvdef::HvVpAssistPage, vtl_control) as u64
            + offset_of!(HvVpVtlControl, vina_status) as u64;

        self.guest_memory.write_plain(gpa, &(value as u8))
    }
}

struct HypercallPage {
    page: [u8; 50],
    offsets32: hvdef::HvRegisterVsmCodePageOffsets,
    offsets64: hvdef::HvRegisterVsmCodePageOffsets,
}

const fn hypercall_page(use_vmmcall: bool) -> HypercallPage {
    let [hc0, hc1, hc2] = if use_vmmcall {
        [0x0f, 0x01, 0xd9] // vmmcall
    } else {
        [0x0f, 0x01, 0xc1] // vmcall
    };

    #[rustfmt::skip]
    let page = [
        // Normal entry
        hc0, hc1, hc2,                  // 0:  0f 01 d9                vmmcall
        0xc3,                           // 3:  c3                      ret
        // 32-bit VTL call
        0x89, 0xc1,                     // 4:  89 c1                   mov    ecx,eax
        0xb8, 0x11, 0x00, 0x00, 0x00,   // 6:  b8 11 00 00 00          mov    eax,0x11
        hc0, hc1, hc2,                  // b:  0f 01 d9                vmmcall
        0xc3,                           // e:  c3                      ret
        // 64-bit VTL call
        0x48, 0x89, 0xc8,               // f:  48 89 c8                mov    rax,rcx
        0xb9, 0x11, 0x00, 0x00, 0x00,   // 12: b9 11 00 00 00          mov    ecx,0x11
        hc0, hc1, hc2,                  // 17: 0f 01 d9                vmmcall
        0xc3,                           // 1a: c3                      ret
        // 32-bit VTL return
        0x89, 0xc1,                     // 1b: 89 c1                   mov    ecx,eax
        0xb8, 0x12, 0x00, 0x00, 0x00,   // 1d: b8 12 00 00 00          mov    eax,0x12
        hc0, hc1, hc2,                  // 22: 0f 01 d9                vmmcall
        0xc3,                           // 25: c3                      ret
        // 64-bit VTL return
        0x48, 0x89, 0xc8,               // 26: 48 89 c8                mov    rax,rcx
        0xb9, 0x12, 0x00, 0x00, 0x00,   // 29: b9 12 00 00 00          mov    ecx,0x12
        hc0, hc1, hc2,                  // 2e: 0f 01 d9                vmmcall
        0xc3,                           // 31: c3                      ret
    ];

    HypercallPage {
        page,
        offsets32: hvdef::HvRegisterVsmCodePageOffsets::new()
            .with_call_offset(0x4)
            .with_return_offset(0x1b),
        offsets64: hvdef::HvRegisterVsmCodePageOffsets::new()
            .with_call_offset(0xf)
            .with_return_offset(0x26),
    }
}

const AMD_HYPERCALL_PAGE: HypercallPage = hypercall_page(true);
const INTEL_HYPERCALL_PAGE: HypercallPage = hypercall_page(false);

/// A trait for managing the hypercall code page overlay, including its location
/// and vtl protections.
pub trait VtlProtectHypercallOverlay: Send + Sync {
    /// Change the location of the overlay.
    fn change_overlay(&self, gpn: u64);
    /// Disable the overlay.
    fn disable_overlay(&self);
}
