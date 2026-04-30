// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Error;
use crate::ErrorInner;
use crate::MshvProcessor;
use crate::VcpuFdExt;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_bindings::MSHV_VP_STATE_SIEFP;
use mshv_bindings::MSHV_VP_STATE_SIMP;
use mshv_bindings::MSHV_VP_STATE_SYNTHETIC_TIMERS;
use mshv_bindings::mshv_get_set_vp_state;
use std::ptr::NonNull;
use std::sync::OnceLock;
use virt::state::HvRegisterState;
use virt::vp::ApicRegisters;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

impl MshvProcessor<'_> {
    pub(crate) fn set_register_state<T, const N: usize>(&self, regs: &T) -> Result<(), Error>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        regs.get_values(assoc.iter_mut().map(|assoc| &mut assoc.value));

        self.runner
            .vcpufd
            .set_hvdef_regs(&assoc[..])
            .map_err(ErrorInner::Register)?;

        Ok(())
    }

    pub(crate) fn get_register_state<T, const N: usize>(&self) -> Result<T, Error>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        let mut regs = T::default();
        let mut assoc = regs.names().map(|name| HvRegisterAssoc {
            name: name.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        });

        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc[..])
            .map_err(ErrorInner::Register)?;

        regs.set_values(assoc.iter().map(|assoc| assoc.value));
        Ok(regs)
    }

    fn set_state(&self, ty: u32, data: &[u8]) -> Result<(), Error> {
        // The kernel requires a page-aligned buffer for VP state operations.
        let mut buf = PageAlignedBuffer::new(data.len());
        buf.as_mut_bytes().copy_from_slice(data);

        let vp_state = mshv_get_set_vp_state {
            type_: ty as u8,
            buf_sz: buf.aligned_len() as u32,
            buf_ptr: buf.as_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .set_vp_state_ioctl(&vp_state)
            .map_err(|e| ErrorInner::SetVpState {
                error: e.into(),
                ty: ty as u8,
            })?;
        Ok(())
    }

    fn get_fixed_state<T: zerocopy::FromBytes>(&self, ty: u32) -> Result<T, Error> {
        let state = self.get_state(ty, size_of::<T>())?;
        Ok(T::read_from_prefix(state.as_bytes()).unwrap().0)
    }

    fn get_state(&self, ty: u32, size: usize) -> Result<PageAlignedBuffer, Error> {
        // The kernel requires a page-aligned buffer for VP state operations.
        let mut buf = PageAlignedBuffer::new(size);
        let mut vp_state = mshv_get_set_vp_state {
            type_: ty as u8,
            buf_sz: buf.aligned_len() as u32,
            buf_ptr: buf.as_mut_ptr() as u64,
            ..Default::default()
        };
        self.runner
            .vcpufd
            .get_vp_state_ioctl(&mut vp_state)
            .map_err(|e| ErrorInner::GetVpState {
                error: e.into(),
                ty: ty as u8,
            })?;
        Ok(buf)
    }

    fn get_lapic(&self) -> Result<ApicRegisters, Error> {
        let hv_state: hvdef::HvX64InterruptControllerState =
            self.get_fixed_state(mshv_bindings::MSHV_VP_STATE_LAPIC)?;

        Ok(ApicRegisters::from(hv_state))
    }

    fn set_lapic(&self, lapic: &ApicRegisters) -> Result<(), Error> {
        let hv_state: hvdef::HvX64InterruptControllerState = (*lapic).into();
        self.set_state(mshv_bindings::MSHV_VP_STATE_LAPIC, hv_state.as_bytes())
    }
}

struct PageAlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    layout: std::alloc::Layout,
}

impl PageAlignedBuffer {
    fn page_size() -> usize {
        static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
        // SAFETY: sysconf(_SC_PAGESIZE) is always safe to call.
        *PAGE_SIZE.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize)
    }

    fn new(len: usize) -> Self {
        let page_size = Self::page_size();
        let layout =
            std::alloc::Layout::from_size_align(len.next_multiple_of(page_size), page_size)
                .unwrap();
        // SAFETY: layout has non-zero size and page alignment.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout);
        };
        Self { ptr, len, layout }
    }

    fn aligned_len(&self) -> usize {
        self.layout.size()
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: ptr is valid for layout.size() >= self.len bytes and is
        // uniquely owned.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.len) }
    }

    fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() >= self.len bytes, and &mut
        // self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), self.len) }
    }
}

impl Drop for PageAlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated with this layout via alloc_zeroed.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

impl AccessVpState for &'_ mut MshvProcessor<'_> {
    type Error = Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let mut activity: vp::Activity = self.get_register_state()?;
        // The NMI pending bit is not part of the register state; it lives
        // in the APIC page.
        activity.nmi_pending = self.get_lapic()?.hv_apic_nmi_pending();
        Ok(activity)
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        self.set_register_state(value)?;
        // The NMI pending bit is not part of the register state; it must
        // be set via the APIC page.
        let mut lapic = self.get_lapic()?;
        if lapic.hv_apic_nmi_pending() != value.nmi_pending {
            lapic.set_hv_apic_nmi_pending(value.nmi_pending);
            self.set_lapic(&lapic)?;
        }
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        let xsave = self.get_state(
            mshv_bindings::MSHV_VP_STATE_XSAVE,
            self.partition.caps.xsave.compact_len as usize,
        )?;
        Ok(vp::Xsave::from_compact(
            xsave.as_bytes(),
            &self.partition.caps,
        ))
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        self.set_state(mshv_bindings::MSHV_VP_STATE_XSAVE, value.compact())?;
        Ok(())
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        // Get the APIC base register.
        let mut assoc = [HvRegisterAssoc {
            name: HvX64RegisterName::ApicBase.into(),
            pad: [0; 3],
            value: FromZeros::new_zeroed(),
        }];
        self.runner
            .vcpufd
            .get_hvdef_regs(&mut assoc)
            .map_err(ErrorInner::Register)?;
        let apic_base = assoc[0].value.as_u64();

        // Get the LAPIC state page.
        let mut lapic = self.get_lapic()?;
        // Clear the non-architectural NMI pending bit.
        lapic.set_hv_apic_nmi_pending(false);
        Ok(vp::Apic::new(apic_base.into(), lapic, [0; 8]))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        // Set the APIC base register first to set the APIC mode before
        // updating the APIC register state.
        self.runner
            .vcpufd
            .set_hvdef_regs(&[HvRegisterAssoc::from((
                HvX64RegisterName::ApicBase,
                value.apic_base,
            ))])
            .map_err(ErrorInner::Register)?;

        // Preserve the current NMI pending state across the restore.
        let nmi_pending = self.get_lapic()?.hv_apic_nmi_pending();

        // Set the LAPIC state page, restoring the NMI pending bit.
        let mut lapic = *value.registers();
        lapic.set_hv_apic_nmi_pending(nmi_pending);
        self.set_lapic(&lapic)?;

        Ok(())
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        self.get_register_state()
    }

    fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        self.get_register_state()
    }

    fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
        self.get_register_state()
    }

    fn set_mtrrs(&mut self, value: &vp::Mtrrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
        self.get_register_state()
    }

    fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_virtual_msrs(&mut self, value: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        self.get_register_state()
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc(&mut self, value: &vp::Tsc) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        self.get_register_state()
    }

    fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        self.get_register_state()
    }

    fn set_cet_ss(&mut self, value: &vp::CetSs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_synic_msrs(&mut self, value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        Ok(vp::SynicTimers::from_hv(
            self.get_fixed_state(MSHV_VP_STATE_SYNTHETIC_TIMERS)?,
        ))
    }

    fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
        self.set_state(MSHV_VP_STATE_SYNTHETIC_TIMERS, value.as_hv().as_bytes())?;
        Ok(())
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Ok(self.inner.message_queues.save())
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        self.inner.message_queues.restore(value);
        Ok(())
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        let data = self.get_fixed_state(MSHV_VP_STATE_SIMP)?;
        Ok(vp::SynicMessagePage { data })
    }

    fn set_synic_message_page(&mut self, value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        self.set_state(MSHV_VP_STATE_SIMP, &value.data)
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        let data = self.get_fixed_state(MSHV_VP_STATE_SIEFP)?;
        Ok(vp::SynicEventFlagsPage { data })
    }

    fn set_synic_event_flags_page(
        &mut self,
        value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        self.set_state(MSHV_VP_STATE_SIEFP, &value.data)
    }
}
