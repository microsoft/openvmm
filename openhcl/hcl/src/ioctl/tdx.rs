// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for TDX partitions.

use super::Hcl;
use super::HclVp;
use super::MshvVtl;
use super::NoRunner;
use super::ProcessorRunner;
use super::hcl_tdcall;
use super::mshv_tdcall;
use crate::GuestVtl;
use crate::protocol::tdx_l2_tsc_deadline_state;
use crate::protocol::tdx_tdg_vp_enter_exit_info;
use crate::protocol::tdx_vp_context;
use crate::protocol::tdx_vp_state;
use crate::protocol::tdx_vp_state_flags;
use hv1_structs::VtlArray;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use memory_range::MemoryRange;
use sidecar_client::SidecarVp;
use std::cell::UnsafeCell;
use std::os::fd::AsRawFd;
use tdcall::Tdcall;
use tdcall::tdcall_vp_invgla;
use tdcall::tdcall_vp_rd;
use tdcall::tdcall_vp_wr;
use x86defs::tdx::TdCallResult;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdGlaVmAndFlags;
use x86defs::tdx::TdVpsClassCode;
use x86defs::tdx::TdgMemPageAttrWriteR8;
use x86defs::tdx::TdgMemPageGpaAttr;
use x86defs::tdx::TdxContextCode;
use x86defs::tdx::TdxExtendedFieldCode;
use x86defs::tdx::TdxGlaListInfo;
use x86defs::tdx::TdxGp;
use x86defs::tdx::TdxL2Ctls;
use x86defs::tdx::TdxL2EnterGuestState;
use x86defs::tdx::TdxVmFlags;
use x86defs::vmx::ApicPage;
use x86defs::vmx::VmcsField;

/// Runner backing for TDX partitions.
pub struct Tdx<'a> {
    apic_pages: VtlArray<&'a UnsafeCell<ApicPage>, 2>,
}

impl MshvVtl {
    /// Issues a tdcall to set page attributes.
    pub fn tdx_set_page_attributes(
        &self,
        range: MemoryRange,
        attributes: TdgMemPageGpaAttr,
        mask: TdgMemPageAttrWriteR8,
    ) -> Result<(), TdCallResultCode> {
        tdcall::set_page_attributes(&mut MshvVtlTdcall(self), range, attributes, mask)
    }

    /// Issues a tdcall to accept pages, optionally also setting attributes.
    ///
    /// These operations are combined because this code tries accepting at 2MB
    /// granularity first and then falls back to 4KB. A separate call to
    /// [`Self::tdx_set_page_attributes`] has to re-derive the appropriate
    /// granularity.
    pub fn tdx_accept_pages(
        &self,
        range: MemoryRange,
        attributes: Option<(TdgMemPageGpaAttr, TdgMemPageAttrWriteR8)>,
    ) -> Result<(), tdcall::AcceptPagesError> {
        let attributes = attributes
            .map_or(tdcall::AcceptPagesAttributes::None, |(attributes, mask)| {
                tdcall::AcceptPagesAttributes::Set { attributes, mask }
            });

        tdcall::accept_pages(&mut MshvVtlTdcall(self), range, attributes)
    }
}

impl<'a> ProcessorRunner<'a, Tdx<'a>> {
    /// Gets a reference to the TDX VP context that is unioned inside the run
    /// page.
    fn tdx_vp_context(&self) -> &tdx_vp_context {
        // SAFETY: the VP context will not be concurrently accessed by the
        // processor while this VP is in VTL2. This is a TDX partition so the
        // context union should be interpreted as a `tdx_vp_context`.
        unsafe { &*(&raw mut (*self.run.get()).context).cast() }
    }

    /// Gets a mutable reference to the TDX VP context that is unioned inside
    /// the run page.
    fn tdx_vp_context_mut(&mut self) -> &mut tdx_vp_context {
        // SAFETY: the VP context will not be concurrently accessed by the
        // processor while this VP is in VTL2. This is a TDX partition so the
        // context union should be interpreted as a `tdx_vp_context`.
        unsafe { &mut *(&raw mut (*self.run.get()).context).cast() }
    }

    /// Gets a reference to the TDX enter guest state.
    fn tdx_enter_guest_state(&self) -> &TdxL2EnterGuestState {
        &self.tdx_vp_context().gpr_list
    }

    /// Gets a mutable reference to the TDX enter guest state.
    fn tdx_enter_guest_state_mut(&mut self) -> &mut TdxL2EnterGuestState {
        &mut self.tdx_vp_context_mut().gpr_list
    }

    /// Gets a reference to the TDX enter guest state's GP list.
    pub fn tdx_enter_guest_gps(&self) -> &[u64; 16] {
        &self.tdx_enter_guest_state().gps
    }

    /// Gets a mutable reference to the TDX enter guest state's GP list.
    pub fn tdx_enter_guest_gps_mut(&mut self) -> &mut [u64; 16] {
        &mut self.tdx_enter_guest_state_mut().gps
    }

    /// Gets a reference to the tdx exit info from a VP.ENTER call.
    pub fn tdx_vp_enter_exit_info(&self) -> &tdx_tdg_vp_enter_exit_info {
        &self.tdx_vp_context().exit_info
    }

    /// Gets a reference to the tdx APIC page for the given VTL.
    pub fn tdx_apic_page(&self, vtl: GuestVtl) -> &ApicPage {
        // SAFETY: the APIC pages will not be concurrently accessed by the processor
        // while this VP is in VTL2.
        unsafe { &*self.state.apic_pages[vtl].get() }
    }

    /// Gets a mutable reference to the tdx APIC page for the given VTL.
    pub fn tdx_apic_page_mut(&mut self, vtl: GuestVtl) -> &mut ApicPage {
        // SAFETY: the APIC pages will not be concurrently accessed by the processor
        // while this VP is in VTL2.
        unsafe { &mut *self.state.apic_pages[vtl].get() }
    }

    /// Gets a reference to TDX VP specific state.
    fn tdx_vp_state(&self) -> &tdx_vp_state {
        &self.tdx_vp_context().vp_state
    }

    /// Gets a mutable reference to TDX VP specific state
    fn tdx_vp_state_mut(&mut self) -> &mut tdx_vp_state {
        &mut self.tdx_vp_context_mut().vp_state
    }

    /// Gets the value of CR2 from the shared kernel state.
    pub fn cr2(&self) -> u64 {
        self.tdx_vp_state().cr2
    }

    /// Gets the value of CR2 from the shared kernel state.
    pub fn set_cr2(&mut self, value: u64) {
        self.tdx_vp_state_mut().cr2 = value;
    }

    /// Gets a mutable reference to TDX specific VP flags.
    pub fn tdx_vp_state_flags_mut(&mut self) -> &mut tdx_vp_state_flags {
        &mut self.tdx_vp_state_mut().flags
    }

    /// Gets a reference to the TDX VP entry flags.
    fn tdx_vp_entry_flags(&self) -> &TdxVmFlags {
        &self.tdx_vp_context().entry_rcx
    }

    /// Gets a mutable reference to the TDX VP entry flags.
    fn tdx_vp_entry_flags_mut(&mut self) -> &mut TdxVmFlags {
        &mut self.tdx_vp_context_mut().entry_rcx
    }

    /// Gets a reference to the TDX L2 TSC deadline state.
    pub fn tdx_l2_tsc_deadline_state(&self) -> &tdx_l2_tsc_deadline_state {
        &self.tdx_vp_context().l2_tsc_deadline
    }

    /// Gets a mutable reference to the TDX L2 TSC deadline state.
    pub fn tdx_l2_tsc_deadline_state_mut(&mut self) -> &mut tdx_l2_tsc_deadline_state {
        &mut self.tdx_vp_context_mut().l2_tsc_deadline
    }

    /// Reads the private registers from the kernel's shared run page into
    /// the given [`TdxPrivateRegs`].
    pub fn read_private_regs(&self, regs: &mut TdxPrivateRegs) {
        let TdxL2EnterGuestState {
            gps, // Shared between VTLs except for RSP
            rflags,
            rip,
            ssp,
            rvi,
            svi,
            reserved: _reserved,
        } = self.tdx_enter_guest_state();
        regs.rflags = *rflags;
        regs.rip = *rip;
        regs.rsp = gps[TdxGp::RSP];
        regs.ssp = *ssp;
        regs.rvi = *rvi;
        regs.svi = *svi;

        let tdx_vp_state {
            msr_kernel_gs_base,
            msr_star,
            msr_lstar,
            msr_sfmask,
            msr_xss,
            cr2: _cr2, // Shared between VTLs
            msr_tsc_aux,
            flags: _flags, // Global flags
        } = self.tdx_vp_state();
        regs.msr_kernel_gs_base = *msr_kernel_gs_base;
        regs.msr_star = *msr_star;
        regs.msr_lstar = *msr_lstar;
        regs.msr_sfmask = *msr_sfmask;
        regs.msr_xss = *msr_xss;
        regs.msr_tsc_aux = *msr_tsc_aux;

        regs.vp_entry_flags = *self.tdx_vp_entry_flags();
    }

    /// Writes the private registers from the given [`TdxPrivateRegs`] to the
    /// kernel's shared run page.
    pub fn write_private_regs(&mut self, regs: &TdxPrivateRegs) {
        let TdxPrivateRegs {
            rflags,
            rip,
            rsp,
            ssp,
            rvi,
            svi,
            msr_kernel_gs_base,
            msr_star,
            msr_lstar,
            msr_sfmask,
            msr_xss,
            msr_tsc_aux,
            vp_entry_flags,
        } = regs;

        let enter_guest_state = self.tdx_enter_guest_state_mut();
        enter_guest_state.rflags = *rflags;
        enter_guest_state.rip = *rip;
        enter_guest_state.ssp = *ssp;
        enter_guest_state.rvi = *rvi;
        enter_guest_state.svi = *svi;
        enter_guest_state.gps[TdxGp::RSP] = *rsp;

        let vp_state = self.tdx_vp_state_mut();
        vp_state.msr_kernel_gs_base = *msr_kernel_gs_base;
        vp_state.msr_star = *msr_star;
        vp_state.msr_lstar = *msr_lstar;
        vp_state.msr_sfmask = *msr_sfmask;
        vp_state.msr_xss = *msr_xss;
        vp_state.msr_tsc_aux = *msr_tsc_aux;

        *self.tdx_vp_entry_flags_mut() = *vp_entry_flags;
    }

    fn write_vmcs(&mut self, vtl: GuestVtl, field: VmcsField, mask: u64, value: u64) -> u64 {
        tdcall_vp_wr(
            &mut MshvVtlTdcall(&self.hcl.mshv_vtl),
            vmcs_field_code(field, vtl),
            value,
            mask,
        )
        .expect("fatal vmcs access failure")
    }

    fn read_vmcs(&self, vtl: GuestVtl, field: VmcsField) -> u64 {
        tdcall_vp_rd(
            &mut MshvVtlTdcall(&self.hcl.mshv_vtl),
            vmcs_field_code(field, vtl),
        )
        .expect("fatal vmcs access failure")
    }

    /// Write a 64-bit VMCS field.
    ///
    /// Only updates the bits that are set in `mask`. Returns the old value of
    /// the field.
    ///
    /// Panics if the field is not a 64-bit field, or if there is an error in
    /// the TDX module when writing the field.
    pub fn write_vmcs64(&mut self, vtl: GuestVtl, field: VmcsField, mask: u64, value: u64) -> u64 {
        assert!(matches!(
            field.field_width(),
            x86defs::vmx::FieldWidth::WidthNatural | x86defs::vmx::FieldWidth::Width64
        ));
        self.write_vmcs(vtl, field, mask, value)
    }

    /// Reads a 64-bit VMCS field.
    ///
    /// Panics if the field is not a 64-bit field, or if there is an error in
    /// the TDX module when reading the field.
    pub fn read_vmcs64(&self, vtl: GuestVtl, field: VmcsField) -> u64 {
        assert!(matches!(
            field.field_width(),
            x86defs::vmx::FieldWidth::WidthNatural | x86defs::vmx::FieldWidth::Width64
        ));
        self.read_vmcs(vtl, field)
    }

    /// Write a 32-bit VMCS field.
    ///
    /// Only updates the bits that are set in `mask`. Returns the old value of
    /// the field.
    ///
    /// Panics if the field is not a 32-bit field, or if there is an error in
    /// the TDX module when writing the field.
    pub fn write_vmcs32(&mut self, vtl: GuestVtl, field: VmcsField, mask: u32, value: u32) -> u32 {
        assert_eq!(field.field_width(), x86defs::vmx::FieldWidth::Width32);
        self.write_vmcs(vtl, field, mask.into(), value.into()) as u32
    }

    /// Reads a 32-bit VMCS field.
    ///
    /// Panics if the field is not a 32-bit field, or if there is an error in
    /// the TDX module when reading the field.
    pub fn read_vmcs32(&self, vtl: GuestVtl, field: VmcsField) -> u32 {
        assert_eq!(field.field_width(), x86defs::vmx::FieldWidth::Width32);
        self.read_vmcs(vtl, field) as u32
    }

    /// Write a 16-bit VMCS field.
    ///
    /// Only updates the bits that are set in `mask`. Returns the old value of
    /// the field.
    ///
    /// Panics if the field is not a 16-bit field, or if there is an error in
    /// the TDX module when writing the field.
    pub fn write_vmcs16(&mut self, vtl: GuestVtl, field: VmcsField, mask: u16, value: u16) -> u16 {
        assert_eq!(field.field_width(), x86defs::vmx::FieldWidth::Width16);
        self.write_vmcs(vtl, field, mask.into(), value.into()) as u16
    }

    /// Reads a 16-bit VMCS field.
    ///
    /// Panics if the field is not a 16-bit field, or if there is an error in
    /// the TDX module when reading the field.
    pub fn read_vmcs16(&self, vtl: GuestVtl, field: VmcsField) -> u16 {
        assert_eq!(field.field_width(), x86defs::vmx::FieldWidth::Width16);
        self.read_vmcs(vtl, field) as u16
    }

    /// Sets the MSR bitmap intercept bit for the given MSR index.
    ///
    /// Panics if there is an error in the TDX module when writing the bit.
    pub fn set_msr_bit(&self, vtl: GuestVtl, msr_index: u32, write: bool, intercept: bool) {
        let mut word_index = (msr_index & 0xFFFF) / 64;

        if msr_index & 0x80000000 == 0x80000000 {
            assert!((0xC0000000..=0xC0001FFF).contains(&msr_index));
            word_index += 0x80;
        } else {
            assert!(msr_index <= 0x00001FFF);
        }

        if write {
            word_index += 0x100;
        }

        self.write_msr_bitmap(
            vtl,
            word_index,
            1 << (msr_index as u64 & 0x3F),
            if intercept { !0 } else { 0 },
        );
    }

    /// Writes 64-bit word with index `i` of the MSR bitmap.
    ///
    /// Only updates the bits that are set in `mask`. Returns the old value of
    /// the word.
    ///
    /// Panics if there is an error in the TDX module when writing the word.
    pub fn write_msr_bitmap(&self, vtl: GuestVtl, i: u32, mask: u64, word: u64) -> u64 {
        let class_code = match vtl {
            GuestVtl::Vtl0 => TdVpsClassCode::MSR_BITMAPS_1,
            GuestVtl::Vtl1 => TdVpsClassCode::MSR_BITMAPS_2,
        };
        let field_code = TdxExtendedFieldCode::new()
            .with_context_code(TdxContextCode::TD_VCPU)
            .with_field_size(x86defs::tdx::FieldSize::Size64Bit)
            .with_field_code(i)
            .with_class_code(class_code.0);

        tdcall_vp_wr(
            &mut MshvVtlTdcall(&self.hcl.mshv_vtl),
            field_code,
            word,
            mask,
        )
        .unwrap()
    }

    /// Sets the L2_CTLS field of the VP.
    ///
    /// Returns the old value of the field.
    pub fn set_l2_ctls(&self, vtl: GuestVtl, value: TdxL2Ctls) -> Result<TdxL2Ctls, TdCallResult> {
        let field_code = match vtl {
            GuestVtl::Vtl0 => x86defs::tdx::TDX_FIELD_CODE_L2_CTLS_VM1,
            GuestVtl::Vtl1 => x86defs::tdx::TDX_FIELD_CODE_L2_CTLS_VM2,
        };
        tdcall_vp_wr(
            &mut MshvVtlTdcall(&self.hcl.mshv_vtl),
            field_code,
            value.into(),
            !0,
        )
        .map(Into::into)
    }

    /// Issues an INVGLA instruction for the VP.
    pub fn invgla(
        &self,
        gla_flags: TdGlaVmAndFlags,
        gla_info: TdxGlaListInfo,
    ) -> Result<(), TdCallResult> {
        tdcall_vp_invgla(&mut MshvVtlTdcall(&self.hcl.mshv_vtl), gla_flags, gla_info)
    }

    /// Gets the FPU state for the VP.
    pub fn fx_state(&self) -> &x86defs::xsave::Fxsave {
        &self.tdx_vp_context().fx_state
    }

    /// Sets the FPU state for the VP.
    pub fn fx_state_mut(&mut self) -> &mut x86defs::xsave::Fxsave {
        &mut self.tdx_vp_context_mut().fx_state
    }
}

fn vmcs_field_code(field: VmcsField, vtl: GuestVtl) -> TdxExtendedFieldCode {
    let class_code = match vtl {
        GuestVtl::Vtl0 => TdVpsClassCode::VMCS_1,
        GuestVtl::Vtl1 => TdVpsClassCode::VMCS_2,
    };
    let field_size = match field.field_width() {
        x86defs::vmx::FieldWidth::Width16 => x86defs::tdx::FieldSize::Size16Bit,
        x86defs::vmx::FieldWidth::Width32 => x86defs::tdx::FieldSize::Size32Bit,
        x86defs::vmx::FieldWidth::Width64 => x86defs::tdx::FieldSize::Size64Bit,
        x86defs::vmx::FieldWidth::WidthNatural => x86defs::tdx::FieldSize::Size64Bit,
    };
    TdxExtendedFieldCode::new()
        .with_context_code(TdxContextCode::TD_VCPU)
        .with_class_code(class_code.0)
        .with_field_code(field.into())
        .with_field_size(field_size)
}

impl<'a> super::private::BackingPrivate<'a> for Tdx<'a> {
    fn new(vp: &'a HclVp, sidecar: Option<&SidecarVp<'_>>, hcl: &Hcl) -> Result<Self, NoRunner> {
        assert!(sidecar.is_none());
        let super::BackingState::Tdx {
            vtl0_apic_page,
            vtl1_apic_page,
        } = &vp.backing
        else {
            return Err(NoRunner::MismatchedIsolation);
        };

        // Register the VTL 1 APIC page with the TD module.
        // The VTL 0 APIC page is registered by the kernel.
        let vtl1_apic_page_addr = vtl1_apic_page.pfns()[0] * user_driver::memory::PAGE_SIZE64;
        tdcall_vp_wr(
            &mut MshvVtlTdcall(&hcl.mshv_vtl),
            vmcs_field_code(VmcsField::VMX_VMCS_VIRTUAL_APIC_PAGE, GuestVtl::Vtl1),
            vtl1_apic_page_addr,
            !0,
        )
        .expect("failed registering VTL1 APIC page");

        // SAFETY: The mapping is held for the appropriate lifetime, and the
        // APIC page is never accessed as any other type, or by any other location.
        let vtl1_apic_page = unsafe { &*vtl1_apic_page.base().cast() };

        Ok(Self {
            apic_pages: [vtl0_apic_page.as_ref(), vtl1_apic_page].into(),
        })
    }

    fn try_set_reg(
        _runner: &mut ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        _name: HvRegisterName,
        _value: HvRegisterValue,
    ) -> Result<bool, super::Error> {
        Ok(false)
    }

    fn must_flush_regs_on(_runner: &ProcessorRunner<'a, Self>, _name: HvRegisterName) -> bool {
        false
    }

    fn try_get_reg(
        _runner: &ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        _name: HvRegisterName,
    ) -> Result<Option<HvRegisterValue>, super::Error> {
        Ok(None)
    }

    fn flush_register_page(_runner: &mut ProcessorRunner<'a, Self>) {}
}

/// Private registers that are copied to/from the kernel's shared run page.
#[derive(inspect::InspectMut)]
#[expect(missing_docs, reason = "Self-describing field names")]
pub struct TdxPrivateRegs {
    // Registers on [`TdxL2EnterGuestState`].
    pub rflags: u64,
    pub rip: u64,
    pub rsp: u64,
    pub ssp: u64,
    pub rvi: u8,
    pub svi: u8,
    // Registers on [`tdx_vp_state`].
    pub msr_kernel_gs_base: u64,
    pub msr_star: u64,
    pub msr_lstar: u64,
    pub msr_sfmask: u64,
    pub msr_xss: u64,
    pub msr_tsc_aux: u64,
    // VP Entry flags
    #[inspect(hex, with = "|x| x.into_bits()")]
    pub vp_entry_flags: TdxVmFlags,
}

impl TdxPrivateRegs {
    /// Creates a new register set with the given values.
    /// Other values are initialized to zero.
    pub fn new(vtl: GuestVtl) -> Self {
        Self {
            rflags: x86defs::RFlags::at_reset().into(),
            rip: 0,
            rsp: 0,
            ssp: 0,
            rvi: 0,
            svi: 0,
            msr_kernel_gs_base: 0,
            msr_star: 0,
            msr_lstar: 0,
            msr_sfmask: 0,
            msr_xss: 0,
            msr_tsc_aux: 0,
            // We initialize with a TLB flush pending so that save/restore/reset
            // operations (not supported yet, but maybe someday) will start with
            // a clear TLB. During regular boots this won't matter, as the TLB
            // will already be empty.
            vp_entry_flags: TdxVmFlags::new()
                .with_vm_index(vtl as u8 + 1)
                .with_invd_translations(x86defs::tdx::TDX_VP_ENTER_INVD_INVEPT),
        }
    }
}

struct MshvVtlTdcall<'a>(&'a MshvVtl);

impl Tdcall for MshvVtlTdcall<'_> {
    fn tdcall(&mut self, input: tdcall::TdcallInput) -> tdcall::TdcallOutput {
        let mut mshv_tdcall_args = {
            let tdcall::TdcallInput {
                leaf,
                rcx,
                rdx,
                r8,
                r9,
                r10,
                r11,
                r12,
                r13,
                r14,
                r15,
            } = input;

            // NOTE: Only TD module calls are supported by the kernel, so assert
            // that here before dispatching. Additionally, the kernel only
            // supports a limited set of input registers.
            assert_ne!(leaf, x86defs::tdx::TdCallLeaf::VP_VMCALL);
            assert_eq!(r10, 0);
            assert_eq!(r11, 0);
            assert_eq!(r12, 0);
            assert_eq!(r13, 0);
            assert_eq!(r14, 0);
            assert_eq!(r15, 0);

            mshv_tdcall {
                rax: leaf.0,
                rcx,
                rdx,
                r8,
                r9,
                r10_out: 0,
                r11_out: 0,
            }
        };

        // SAFETY: Calling tdcall ioctl with the correct arguments.
        unsafe {
            // NOTE: This ioctl should never fail, as the tdcall itself failing
            // is returned as output in the structure given by the kernel.
            hcl_tdcall(self.0.file.as_raw_fd(), &mut mshv_tdcall_args)
                .expect("todo handle tdcall ioctl error");
        }

        tdcall::TdcallOutput {
            rax: TdCallResult::from(mshv_tdcall_args.rax),
            rcx: mshv_tdcall_args.rcx,
            rdx: mshv_tdcall_args.rdx,
            r8: mshv_tdcall_args.r8,
            r10: mshv_tdcall_args.r10_out,
            r11: mshv_tdcall_args.r11_out,
        }
    }
}
