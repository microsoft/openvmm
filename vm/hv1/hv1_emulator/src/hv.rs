// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor MSR emulation.

use super::synic::GlobalSynic;
use super::synic::ProcessorSynic;
use crate::pages::LockedPage;
use crate::pages::OverlayPage;
use guestmem::GuestMemory;
use hv1_structs::VtlArray;
use hvdef::HV_REFERENCE_TSC_SEQUENCE_INVALID;
use hvdef::HvRegisterVpAssistPage;
use hvdef::HvVpVtlControl;
use hvdef::HvVtlEntryReason;
use hvdef::Vtl;
use inspect::Inspect;
use parking_lot::Mutex;
use safeatomic::AtomicSliceOps;
use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use virt::x86::MsrError;
use vm_topology::processor::VpIndex;
use vmcore::reference_time::ReferenceTimeSource;
use x86defs::cpuid::Vendor;
use zerocopy::FromZeros;

/// The partition-wide hypervisor state.
#[derive(Inspect)]
pub struct GlobalHv<const VTL_COUNT: usize> {
    #[inspect(flatten)]
    partition_state: Arc<GlobalHvState>,
    /// Mutable state, per VTL
    vtl_mutable_state: VtlArray<Arc<Mutex<MutableHvState>>, VTL_COUNT>,
    /// The per-vtl synic state.
    pub synic: VtlArray<GlobalSynic, VTL_COUNT>,
    /// The guest memory accessor for each VTL.
    guest_memory: VtlArray<GuestMemory, VTL_COUNT>,
}

#[derive(Inspect)]
struct GlobalHvState {
    #[inspect(display)]
    vendor: Vendor,
    ref_time: ReferenceTimeSource,
    tsc_frequency: u64,
    is_ref_time_backed_by_tsc: bool,
}

#[derive(Inspect)]
struct MutableHvState {
    #[inspect(hex, with = "|&x| u64::from(x)")]
    hypercall_reg: hvdef::hypercall::MsrHypercallContents,
    #[inspect(with = "|x| x.is_some()")]
    hypercall_page: Option<LockedPage>,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    guest_os_id: hvdef::hypercall::HvGuestOsId,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    reference_tsc_reg: hvdef::HvRegisterReferenceTsc,
    #[inspect(with = "|x| x.is_some()")]
    reference_tsc_page: Option<LockedPage>,
    tsc_sequence: u32,
}

impl MutableHvState {
    fn new() -> Self {
        Self {
            hypercall_reg: hvdef::hypercall::MsrHypercallContents::new(),
            hypercall_page: None,
            guest_os_id: hvdef::hypercall::HvGuestOsId::new(),
            reference_tsc_reg: hvdef::HvRegisterReferenceTsc::new(),
            reference_tsc_page: None,
            tsc_sequence: 0,
        }
    }

    fn reset(&mut self, overlay_access: &mut dyn VtlProtectHypercallOverlay) {
        overlay_access.disable_overlay();

        let Self {
            hypercall_reg,
            hypercall_page,
            guest_os_id,
            reference_tsc_reg,
            reference_tsc_page,
            tsc_sequence,
        } = self;

        *hypercall_reg = hvdef::hypercall::MsrHypercallContents::new();
        *hypercall_page = None;
        *guest_os_id = hvdef::hypercall::HvGuestOsId::new();
        *reference_tsc_reg = hvdef::HvRegisterReferenceTsc::new();
        *reference_tsc_page = None;
        *tsc_sequence = 0;
    }
}

/// Parameters used when constructing a [`GlobalHv`].
pub struct GlobalHvParams<const VTL_COUNT: usize> {
    /// The maximum VP count for the VM.
    pub max_vp_count: u32,
    /// The vendor of the virtual processor.
    pub vendor: Vendor,
    /// The TSC frequency.
    pub tsc_frequency: u64,
    /// The reference time system to use.
    pub ref_time: ReferenceTimeSource,
    /// If true, the reference time is backed by the TSC, with an implicit
    /// offset of zero.
    pub is_ref_time_backed_by_tsc: bool,
    /// The guest memory accessor for each VTL.
    pub guest_memory: VtlArray<GuestMemory, VTL_COUNT>,
}

impl<const VTL_COUNT: usize> GlobalHv<VTL_COUNT> {
    /// Returns a new hypervisor emulator instance.
    pub fn new(params: GlobalHvParams<VTL_COUNT>) -> Self {
        Self {
            partition_state: Arc::new(GlobalHvState {
                vendor: params.vendor,
                tsc_frequency: params.tsc_frequency,
                is_ref_time_backed_by_tsc: params.is_ref_time_backed_by_tsc,
                ref_time: params.ref_time,
            }),
            vtl_mutable_state: VtlArray::from_fn(|_| Arc::new(Mutex::new(MutableHvState::new()))),
            synic: VtlArray::from_fn(|vtl| {
                GlobalSynic::new(params.guest_memory[vtl].clone(), params.max_vp_count)
            }),
            guest_memory: params.guest_memory,
        }
    }

    /// Adds a virtual processor to the vtl.
    pub fn add_vp(&self, vp_index: VpIndex, vtl: Vtl) -> ProcessorVtlHv {
        ProcessorVtlHv {
            vp_index,
            partition_state: self.partition_state.clone(),
            vtl_state: self.vtl_mutable_state[vtl].clone(),
            synic: self.synic[vtl].add_vp(vp_index),
            vp_assist_page_reg: Default::default(),
            vp_assist_page: OverlayPage::default(),
            guest_memory: self.guest_memory[vtl].clone(),
        }
    }

    /// Resets the global (but not per-processor) state.
    pub fn reset(&self, mut overlay_access: VtlArray<&mut dyn VtlProtectHypercallOverlay, 2>) {
        for (state, overlay_access) in self.vtl_mutable_state.iter().zip(overlay_access.iter_mut())
        {
            state.lock().reset(*overlay_access);
        }
        // There is no global synic state to reset, since the synic is per-VP.
    }

    /// The current guest_os_id value.
    pub fn guest_os_id(&self, vtl: Vtl) -> hvdef::hypercall::HvGuestOsId {
        self.vtl_mutable_state[vtl].lock().guest_os_id
    }

    /// Returns the reference time source.
    pub fn ref_time_source(&self) -> &ReferenceTimeSource {
        &self.partition_state.ref_time
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
    #[inspect(hex, with = "|&x| u64::from(x)")]
    vp_assist_page_reg: HvRegisterVpAssistPage,
    vp_assist_page: OverlayPage,
}

impl ProcessorVtlHv {
    /// The current reference time.
    pub fn ref_time_now(&self) -> u64 {
        self.partition_state.ref_time.now().ref_time
    }

    /// Resets the processor's state.
    pub fn reset(&mut self) {
        let Self {
            vp_index: _,
            partition_state: _,
            vtl_state: _,
            guest_memory: _,
            synic,
            vp_assist_page_reg,
            vp_assist_page,
        } = self;

        synic.reset();
        *vp_assist_page_reg = Default::default();
        *vp_assist_page = OverlayPage::default();
    }

    /// Emulates an MSR write for the guest OS ID MSR.
    pub fn msr_write_guest_os_id(&mut self, v: u64) {
        self.vtl_state.lock().guest_os_id = v.into();
    }

    /// Emulates an MSR write for the VP assist page MSR.
    pub fn msr_write_vp_assist_page(&mut self, v: u64) -> Result<(), MsrError> {
        if v & !u64::from(
            HvRegisterVpAssistPage::new()
                .with_enabled(true)
                .with_gpa_page_number(!0 >> 12),
        ) != 0
        {
            return Err(MsrError::InvalidAccess);
        }
        let new_vp_assist_page_reg = HvRegisterVpAssistPage::from(v);

        if new_vp_assist_page_reg.enabled()
            && (!self.vp_assist_page_reg.enabled()
                || new_vp_assist_page_reg.gpa_page_number()
                    != self.vp_assist_page_reg.gpa_page_number())
        {
            self.vp_assist_page
                .remap(&self.guest_memory, new_vp_assist_page_reg.gpa_page_number())
                .map_err(|_| MsrError::InvalidAccess)?
        } else if !new_vp_assist_page_reg.enabled() {
            self.vp_assist_page.unmap();
        }

        self.vp_assist_page_reg = new_vp_assist_page_reg;

        Ok(())
    }

    /// Emulates an MSR write for an HV#1 synthetic MSR.
    pub fn msr_write(
        &mut self,
        n: u32,
        v: u64,
        overlay_access: &mut dyn VtlProtectHypercallOverlay,
    ) -> Result<(), MsrError> {
        match n {
            hvdef::HV_X64_MSR_GUEST_OS_ID => {
                self.msr_write_guest_os_id(v);
            }
            hvdef::HV_X64_MSR_HYPERCALL => {
                let mut mutable = self.vtl_state.lock();
                if mutable.hypercall_reg.locked() {
                    return Err(MsrError::InvalidAccess);
                }
                let hc = hvdef::hypercall::MsrHypercallContents::from(v);
                if hc.reserved_p() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
                if hc.enable()
                    && (!mutable.hypercall_reg.enable() || hc.gpn() != mutable.hypercall_reg.gpn())
                {
                    // TODO GUEST VSM: make sure the guest has writable vtl
                    // permissions to this page and that it's not in shared
                    // memory.
                    let new_page = LockedPage::new(&self.guest_memory, hc.gpn())
                        .map_err(|_| MsrError::InvalidAccess)?;
                    self.write_hypercall_page(&new_page);
                    overlay_access.change_overlay(hc.gpn());
                    mutable.hypercall_page = Some(new_page);
                } else if !hc.enable() {
                    overlay_access.disable_overlay();
                    mutable.hypercall_page = None;
                }
                mutable.hypercall_reg = hc;
            }
            hvdef::HV_X64_MSR_VP_INDEX => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_TIME_REF_COUNT => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_REFERENCE_TSC => {
                let mut mutable = self.vtl_state.lock();
                let v = hvdef::HvRegisterReferenceTsc::from(v);
                if v.reserved_p() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
                if v.enable() && mutable.reference_tsc_reg.gpn() != v.gpn() {
                    let new_page = LockedPage::new(&self.guest_memory, v.gpn())
                        .map_err(|_| MsrError::InvalidAccess)?;
                    new_page[..4].atomic_write_obj(&HV_REFERENCE_TSC_SEQUENCE_INVALID);

                    if self.partition_state.is_ref_time_backed_by_tsc {
                        // TDX TODO: offset might need to be included
                        let tsc_scale = (((10_000_000_u128) << 64)
                            / self.partition_state.tsc_frequency as u128)
                            as u64;
                        mutable.tsc_sequence = mutable.tsc_sequence.wrapping_add(1);
                        if mutable.tsc_sequence == HV_REFERENCE_TSC_SEQUENCE_INVALID {
                            mutable.tsc_sequence = mutable.tsc_sequence.wrapping_add(1);
                        }
                        let reference_page = hvdef::HvReferenceTscPage {
                            tsc_sequence: mutable.tsc_sequence,
                            tsc_scale,
                            ..FromZeros::new_zeroed()
                        };
                        new_page.atomic_write_obj(&reference_page);
                        mutable.reference_tsc_page = Some(new_page);
                    }
                } else if !v.enable() {
                    mutable.reference_tsc_page = None;
                }

                mutable.reference_tsc_reg = v;
            }
            hvdef::HV_X64_MSR_TSC_FREQUENCY => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_VP_ASSIST_PAGE => self.msr_write_vp_assist_page(v)?,
            msr @ hvdef::HV_X64_MSR_SCONTROL..=hvdef::HV_X64_MSR_STIMER3_COUNT => {
                self.synic.write_msr(msr, v)?
            }
            _ => return Err(MsrError::Unknown),
        }
        Ok(())
    }

    fn write_hypercall_page(&self, page: &LockedPage) {
        // Fill the page with int3 to catch invalid jumps into the page.
        let int3 = 0xcc;
        page.atomic_fill(int3);

        let page_contents: &[u8] = if self.partition_state.vendor.is_amd_compatible() {
            &AMD_HYPERCALL_PAGE.page
        } else if self.partition_state.vendor.is_intel_compatible() {
            &INTEL_HYPERCALL_PAGE.page
        } else {
            unreachable!()
        };

        page[..page_contents.len()].atomic_write(page_contents);
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
            hvdef::HV_X64_MSR_HYPERCALL => self.vtl_state.lock().hypercall_reg.into(),
            hvdef::HV_X64_MSR_VP_INDEX => self.vp_index.index() as u64, // VP index
            hvdef::HV_X64_MSR_TIME_REF_COUNT => self.partition_state.ref_time.now().ref_time,
            hvdef::HV_X64_MSR_REFERENCE_TSC => self.vtl_state.lock().reference_tsc_reg.into(),
            hvdef::HV_X64_MSR_TSC_FREQUENCY => self.partition_state.tsc_frequency,
            hvdef::HV_X64_MSR_VP_ASSIST_PAGE => self.vp_assist_page_reg.into(),
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
        self.vp_assist_page_reg.into()
    }

    /// Sets the lazy EOI bit in the VP assist page.
    ///
    /// If this returns true, the caller must call `clear_lazy_eoi` after the
    /// next VP exit but before manipulating the APIC.
    #[must_use]
    pub fn set_lazy_eoi(&mut self) -> bool {
        if !self.vp_assist_page_reg.enabled() {
            return false;
        }

        let offset = offset_of!(hvdef::HvVpAssistPage, apic_assist);
        let v = 1u32;
        self.vp_assist_page[offset..offset + 4].atomic_write_obj(&v);
        true
    }

    /// Clears the lazy EOI bit in the VP assist page.
    ///
    /// Must only be called if `set_lazy_eoi` returned true.
    ///
    /// If the bit was already clear, returns true; the caller must then send an
    /// EOI to the APIC.
    #[must_use]
    pub fn clear_lazy_eoi(&mut self) -> bool {
        let offset = offset_of!(hvdef::HvVpAssistPage, apic_assist);
        let v: u32 = self.vp_assist_page[offset..offset + 4].atomic_read_obj();

        if v & 1 == 0 {
            // The guest cleared the bit. The caller will perform the EOI to the
            // APIC.
            true
        } else {
            // Clear the bit in case the EOI state changes before the guest runs
            // again.
            let v = v & !1;
            self.vp_assist_page[offset..offset + 4].atomic_write_obj(&v);
            false
        }
    }

    /// Get the register values to restore on vtl return
    pub fn return_registers(&self) -> [u64; 2] {
        let offset =
            offset_of!(hvdef::HvVpAssistPage, vtl_control) + offset_of!(HvVpVtlControl, registers);
        self.vp_assist_page[offset..offset + 16].atomic_read_obj()
    }

    /// Set the reason for the vtl return into the vp assist page
    pub fn set_return_reason(&mut self, reason: HvVtlEntryReason) {
        let offset = offset_of!(hvdef::HvVpAssistPage, vtl_control)
            + offset_of!(HvVpVtlControl, entry_reason);
        self.vp_assist_page[offset..offset + 4].atomic_write_obj(&reason);
    }

    /// Gets whether VINA is currently asserted.
    pub fn vina_asserted(&self) -> bool {
        let offset = offset_of!(hvdef::HvVpAssistPage, vtl_control)
            + offset_of!(HvVpVtlControl, vina_status);
        self.vp_assist_page[offset].load(Ordering::Relaxed) != 0
    }

    /// Sets whether VINA is currently asserted.
    pub fn set_vina_asserted(&mut self, value: bool) {
        let offset = offset_of!(hvdef::HvVpAssistPage, vtl_control)
            + offset_of!(HvVpVtlControl, vina_status);
        self.vp_assist_page[offset].store(value as u8, Ordering::Relaxed);
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
pub trait VtlProtectHypercallOverlay {
    /// Change the location of the overlay.
    fn change_overlay(&mut self, gpn: u64);
    /// Disable the overlay.
    fn disable_overlay(&mut self);
}
