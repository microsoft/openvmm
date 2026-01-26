// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interface to `VmsaWrapper`, which combines a SEV-SNP VMSA
//! with a bitmap to allow for register protection.

use std::array;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use x86defs::snp::SevEventInjectInfo;
use x86defs::snp::SevFeatures;
use x86defs::snp::SevSelector;
use x86defs::snp::SevVirtualInterruptControl;
use x86defs::snp::SevVmsa;
use x86defs::snp::SevXmmRegister;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// VMSA and register tweak bitmap.
pub struct VmsaWrapper<'a, T> {
    vmsa: T,
    bitmap: &'a [u8; 64],
}

impl<'a, T> VmsaWrapper<'a, T> {
    /// Create a VmsaWrapper
    pub(crate) fn new(vmsa: T, bitmap: &'a [u8; 64]) -> Self {
        VmsaWrapper { vmsa, bitmap }
    }
}

/// Wraps a SEV VMSA structure with the register tweak bitmap to provide safe access methods.
impl<T: Deref<Target = SevVmsa>> VmsaWrapper<'_, T> {
    /// 64 bit register read
    fn get_u64(&self, offset: usize) -> u64 {
        assert!(offset.is_multiple_of(8));
        let vmsa_raw = &self.vmsa;
        let v = u64::from_ne_bytes(vmsa_raw.as_bytes()[offset..offset + 8].try_into().unwrap());
        if is_protected(self.bitmap, offset) {
            v ^ self.vmsa.register_protection_nonce
        } else {
            v
        }
    }
    /// 32 bit register read
    fn get_u32(&self, offset: usize) -> u32 {
        assert!(offset.is_multiple_of(4));
        (self.get_u64(offset & !7) >> ((offset & 4) * 8)) as u32
    }
    /// 128 bit register read
    fn get_u128(&self, offset: usize) -> u128 {
        self.get_u64(offset) as u128 | ((self.get_u64(offset + 8) as u128) << 64)
    }

    /// Gets an XMM VMSA register as u128
    pub fn xmm_registers(&self, n: usize) -> u128 {
        assert!(n < 16);
        let off = std::mem::offset_of!(SevVmsa, xmm_registers) + (n * 16);
        self.get_u128(off)
    }

    /// Gets a YMM VMSA register as u128
    pub fn ymm_registers(&self, n: usize) -> u128 {
        assert!(n < 16);
        let off = std::mem::offset_of!(SevVmsa, ymm_registers) + (n * 16);
        self.get_u128(off)
    }

    /// Gets the x87 VMSA registers
    pub fn x87_registers(&self) -> [u64; 10] {
        let base = std::mem::offset_of!(SevVmsa, x87_registers);
        array::from_fn(|i| i * 8).map(|offset| self.get_u64(base + offset))
    }
}

/// Wraps a mutable SEV VMSA structure with the register tweak bitmap to provide safe access methods.
impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
    /// 64 bit value to set in register
    fn set_u64(&self, v: u64, offset: usize) -> u64 {
        assert!(offset.is_multiple_of(8));
        if is_protected(self.bitmap, offset) {
            v ^ self.vmsa.register_protection_nonce
        } else {
            v
        }
    }
    /// 32 bit value to set in register
    fn set_u32(&self, v: u32, offset: usize) -> u32 {
        assert!(offset.is_multiple_of(4));
        let val = (v as u64) << ((offset & 4) * 8);
        (self.set_u64(val, offset & !7) >> ((offset & 4) * 8)) as u32
    }
    /// 128 bit value to set in register
    fn set_u128(&self, v: u128, offset: usize) -> u128 {
        self.set_u64(v as u64, offset) as u128
            | ((self.set_u64((v >> 64) as u64, offset + 8) as u128) << 64)
    }

    /// Create a new VMSA
    pub fn reset(&mut self, vmsa_reg_prot: bool) {
        *self.vmsa = FromZeros::new_zeroed();
        if vmsa_reg_prot {
            // Initialize nonce and all protected fields.
            getrandom::fill(self.vmsa.register_protection_nonce.as_mut_bytes())
                .expect("rng failure");
            let nonce = self.vmsa.register_protection_nonce;
            let chunk_size = 8;
            for (i, b) in self
                .vmsa
                .as_mut_bytes()
                .chunks_exact_mut(chunk_size)
                .enumerate()
            {
                let field_offset = i * chunk_size;
                // Ensure direct accesses are not included in bitmap.
                if field_offset == (std::mem::offset_of!(SevVmsa, vmpl) & !7)
                    || field_offset == std::mem::offset_of!(SevVmsa, exit_info1)
                    || field_offset == std::mem::offset_of!(SevVmsa, exit_info2)
                    || field_offset == std::mem::offset_of!(SevVmsa, exit_int_info)
                    || field_offset == std::mem::offset_of!(SevVmsa, sev_features)
                    || field_offset == std::mem::offset_of!(SevVmsa, v_intr_cntrl)
                    || field_offset == std::mem::offset_of!(SevVmsa, guest_error_code)
                    || field_offset == std::mem::offset_of!(SevVmsa, virtual_tom)
                {
                    assert!(!is_protected(self.bitmap, field_offset));
                }
                if is_protected(self.bitmap, field_offset) {
                    b.copy_from_slice(&nonce.to_ne_bytes());
                }
            }
        }
    }

    /// Sets an XMM VMSA register from u128
    pub fn set_xmm_registers(&mut self, n: usize, v: u128) {
        assert!(n < 16);
        let off = std::mem::offset_of!(SevVmsa, xmm_registers) + (n * 16);
        let val: SevXmmRegister = self.set_u128(v, off).into();
        let vmsa_raw = &mut *self.vmsa;
        vmsa_raw.xmm_registers[n] = val;
    }

    /// Sets an XMM VMSA register from u128
    pub fn set_ymm_registers(&mut self, n: usize, v: u128) {
        assert!(n < 16);
        let off = std::mem::offset_of!(SevVmsa, ymm_registers) + (n * 16);
        let val: SevXmmRegister = self.set_u128(v, off).into();
        let vmsa_raw = &mut *self.vmsa;
        vmsa_raw.ymm_registers[n] = val;
    }

    /// Sets the x87 registers
    pub fn set_x87_registers(&mut self, v: &[u64; 10]) {
        let base = std::mem::offset_of!(SevVmsa, x87_registers);
        for (i, new_v) in v.iter().enumerate() {
            let val = self.set_u64(*new_v, base + (i * 8));
            self.vmsa.x87_registers[i] = val;
        }
    }

    /// Gets an atomic reference to the v_intr_cntrl field.
    pub fn v_intr_cntrl_atomic(&self) -> &AtomicU64 {
        // SAFETY: caller responsible for synchronizing with any non-atomic access.
        unsafe { &*(core::ptr::from_ref(&self.vmsa.v_intr_cntrl) as *const AtomicU64) }
    }

    /// Atomically test and set the guest busy bit in v_intr_cntrl.
    pub fn guest_busy_bit_test_and_set(&mut self) -> bool {
        const VINTR_GUEST_BUSYBIT_MASK: u64 = 1u64 << 63;
        let prev = self.v_intr_cntrl_atomic().fetch_or(VINTR_GUEST_BUSYBIT_MASK, Ordering::SeqCst);
        (prev & VINTR_GUEST_BUSYBIT_MASK) != 0
    }

}

/// Check bitmap to see if a register is included in masking.
fn is_protected(bitmap: &[u8; 64], field_offset: usize) -> bool {
    let byte_index = field_offset / 64;
    let bit_index = (field_offset % 64) / 8;
    bitmap[byte_index] & (1 << bit_index) != 0
}

macro_rules! regss {
    ($reg:ident, $set:ident) => {
        impl<T: Deref<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Gets a SevSelector VMSA register
            pub fn $reg(&self) -> SevSelector {
                SevSelector::from(self.get_u128(std::mem::offset_of!(SevVmsa, $reg)))
            }
        }
        impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Sets a SevSelector VMSA register
            pub fn $set(&mut self, v: SevSelector) {
                let val = SevSelector::from(
                    self.set_u128(v.as_u128(), std::mem::offset_of!(SevVmsa, $reg)),
                );
                let vmsa_raw = &mut *self.vmsa;
                vmsa_raw.$reg = val;
            }
        }
    };
}
macro_rules! reg64 {
    ($reg:ident, $set:ident) => {
        impl<T: Deref<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Gets a VMSA register
            pub fn $reg(&self) -> u64 {
                self.get_u64(std::mem::offset_of!(SevVmsa, $reg))
            }
        }
        impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Sets a VMSA register
            pub fn $set(&mut self, v: u64) {
                let val = self.set_u64(v, std::mem::offset_of!(SevVmsa, $reg));
                let vmsa_raw = &mut *self.vmsa;
                vmsa_raw.$reg = val;
            }
        }
    };
}
macro_rules! reg32 {
    ($reg:ident, $set:ident) => {
        impl<T: Deref<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Gets a VMSA register
            pub fn $reg(&self) -> u32 {
                self.get_u32(std::mem::offset_of!(SevVmsa, $reg))
            }
        }
        impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Sets a VMSA register
            pub fn $set(&mut self, v: u32) {
                let val = self.set_u32(v, std::mem::offset_of!(SevVmsa, $reg));
                let vmsa_raw = &mut *self.vmsa;
                vmsa_raw.$reg = val;
            }
        }
    };
}
macro_rules! get_reg_direct {
    ($reg:ident, $ty:ty) => {
        impl<T: Deref<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Gets a VMSA register directly
            pub fn $reg(&self) -> $ty {
                let vmsa_raw = &self.vmsa;
                vmsa_raw.$reg
            }
        }
    };
}
macro_rules! reg_direct {
    ($reg:ident, $set:ident, $ty:ty) => {
        get_reg_direct!($reg, $ty);
        impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Sets a VMSA register directly
            pub fn $set(&mut self, v: $ty) {
                let vmsa_raw = &mut *self.vmsa;
                vmsa_raw.$reg = v;
            }
        }
    };
}
macro_rules! reg_direct_mut {
    ($reg:ident, $set:ident, $ty:ty) => {
        get_reg_direct!($reg, $ty);
        impl<T: DerefMut<Target = SevVmsa>> VmsaWrapper<'_, T> {
            /// Access VMSA field directly in order to manipulate fields.
            pub fn $set(&mut self) -> &mut $ty {
                &mut self.vmsa.$reg
            }
        }
    };
}

reg_direct!(vmpl, set_vmpl, u8);
get_reg_direct!(cpl, u8);
get_reg_direct!(exit_info1, u64);
get_reg_direct!(exit_info2, u64);
reg_direct!(exit_int_info, set_exit_int_info, u64);
reg_direct_mut!(sev_features, sev_features_mut, SevFeatures);
reg_direct_mut!(v_intr_cntrl, v_intr_cntrl_mut, SevVirtualInterruptControl);
reg_direct!(virtual_tom, set_virtual_tom, u64);
reg_direct!(event_inject, set_event_inject, SevEventInjectInfo);
reg_direct!(guest_error_code, set_guest_error_code, u64);
regss!(es, set_es);
regss!(cs, set_cs);
regss!(ss, set_ss);
regss!(ds, set_ds);
regss!(fs, set_fs);
regss!(gs, set_gs);
regss!(gdtr, set_gdtr);
regss!(ldtr, set_ldtr);
regss!(idtr, set_idtr);
regss!(tr, set_tr);
reg64!(pl0_ssp, set_pl0_ssp);
reg64!(pl1_ssp, set_pl1_ssp);
reg64!(pl2_ssp, set_pl2_ssp);
reg64!(pl3_ssp, set_pl3_ssp);
reg64!(u_cet, set_u_cet);
reg64!(efer, set_efer);
reg64!(xss, set_xss);
reg64!(cr4, set_cr4);
reg64!(cr3, set_cr3);
reg64!(cr0, set_cr0);
reg64!(dr7, set_dr7);
reg64!(dr6, set_dr6);
reg64!(rflags, set_rflags);
reg64!(rip, set_rip);
reg64!(dr0, set_dr0);
reg64!(dr1, set_dr1);
reg64!(dr2, set_dr2);
reg64!(dr3, set_dr3);
reg64!(rsp, set_rsp);
reg64!(s_cet, set_s_cet);
reg64!(ssp, set_ssp);
reg64!(interrupt_ssp_table_addr, set_interrupt_ssp_table_addr);
reg64!(rax, set_rax);
reg64!(star, set_star);
reg64!(lstar, set_lstar);
reg64!(cstar, set_cstar);
reg64!(sfmask, set_sfmask);
reg64!(kernel_gs_base, set_kernel_gs_base);
reg64!(sysenter_cs, set_sysenter_cs);
reg64!(sysenter_esp, set_sysenter_esp);
reg64!(sysenter_eip, set_sysenter_eip);
reg64!(cr2, set_cr2);
reg64!(pat, set_pat);
reg64!(spec_ctrl, set_spec_ctrl);
reg32!(tsc_aux, set_tsc_aux);
reg64!(rcx, set_rcx);
reg64!(rdx, set_rdx);
reg64!(rbx, set_rbx);
reg64!(rbp, set_rbp);
reg64!(rsi, set_rsi);
reg64!(rdi, set_rdi);
reg64!(r8, set_r8);
reg64!(r9, set_r9);
reg64!(r10, set_r10);
reg64!(r11, set_r11);
reg64!(r12, set_r12);
reg64!(r13, set_r13);
reg64!(r14, set_r14);
reg64!(r15, set_r15);
reg64!(next_rip, set_next_rip);
reg64!(pcpu_id, set_pcpu_id);
reg64!(xcr0, set_xcr0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reg_access() {
        let nonce = 0xffff_ffff_ffff_ffffu64;
        let nonce128 = ((nonce as u128) << 64) | nonce as u128;
        let mut vmsa: SevVmsa = FromZeros::new_zeroed();
        vmsa.register_protection_nonce = nonce;
        let bitmap = [0xffu8; 64];
        let mut vmsa_wrapper = VmsaWrapper {
            vmsa: &mut vmsa,
            bitmap: &bitmap,
        };

        let val = 0x0000_0055_0000_0055u128;
        let val_xor = val ^ nonce128;
        let cs = SevSelector::from(val);
        let cs_xor = SevSelector::from(val_xor);
        let vmpl = 2u8;
        let rip = 0x55u64;
        let rip_xor = rip ^ nonce;
        let tsc = 0x55u32;
        let tsc_xor = tsc ^ (nonce as u32);
        let xmm_idx = 1;
        let ymm_idx = 1;
        let x87 = [0x55u64; 10];
        let x87_xor = x87.map(|v| v ^ nonce);

        vmsa_wrapper.set_cs(cs);
        vmsa_wrapper.set_vmpl(vmpl);
        vmsa_wrapper.set_rip(rip);
        vmsa_wrapper.set_tsc_aux(tsc);
        vmsa_wrapper.set_xmm_registers(xmm_idx, val);
        vmsa_wrapper.set_ymm_registers(ymm_idx, val);
        vmsa_wrapper.set_x87_registers(&x87);

        assert!(vmsa_wrapper.cs() == cs);
        assert!(vmsa_wrapper.vmpl() == vmpl);
        assert!(vmsa_wrapper.rip() == rip);
        assert!(vmsa_wrapper.xmm_registers(xmm_idx) == val);
        assert!(vmsa_wrapper.ymm_registers(ymm_idx) == val);
        assert!(vmsa_wrapper.tsc_aux() == tsc);
        assert!(vmsa_wrapper.x87_registers() == x87);
        assert!(vmsa.cs == cs_xor); // bitmask applied to u128
        assert!(vmsa.vmpl == vmpl); // no bitmask applied
        assert!(vmsa.rip == rip_xor); // bitmask applied
        assert!(vmsa.tsc_aux == tsc_xor); // bitmask applied to u32
        assert!(vmsa.pkru == 0); // untouched
        assert!(vmsa.xmm_registers[xmm_idx].as_u128() == val_xor); // bitmask applied to correct XMM offset
        assert!(vmsa.ymm_registers[ymm_idx].as_u128() == val_xor); // bitmask applied to correct YMM offset
        assert!(vmsa.x87_registers == x87_xor);
    }

    #[test]
    fn test_init() {
        let mut vmsa: SevVmsa = FromZeros::new_zeroed();
        let mut bitmap = [0x0u8; 64];
        let xmm_idx = 1;
        bitmap[5] = 0x80u8; // rip
        bitmap[18] = 0x03u8; // xmm_registers[1]
        let mut vmsa_wrapper = VmsaWrapper {
            vmsa: &mut vmsa,
            bitmap: &bitmap,
        };
        vmsa_wrapper.reset(true);

        assert!(vmsa_wrapper.rip() == 0);
        assert!(vmsa_wrapper.xmm_registers(xmm_idx) == 0);

        let nonce = vmsa.register_protection_nonce;
        let xmm_val = ((nonce as u128) << 64) | nonce as u128;
        assert!(vmsa.rip == nonce);
        assert!(vmsa.xmm_registers[xmm_idx].as_u128() == xmm_val);
    }
}
