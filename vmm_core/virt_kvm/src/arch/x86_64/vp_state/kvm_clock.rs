// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM paravirtual clock MSR state, which is saved and restored with the
//! synthetic MSRs.

use super::KvmVpStateAccess;
use crate::KvmError;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;

const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;

impl KvmVpStateAccess<'_, '_> {
    /// Reads the Hyper-V synthetic MSRs and the KVM clock MSRs that the
    /// partition exposes.
    pub(super) fn synthetic_msrs(&mut self) -> Result<vp::SyntheticMsrs, KvmError> {
        let mut value = if self.caps().hv1 {
            self.get_register_state()?
        } else {
            vp::SyntheticMsrs::default()
        };
        if self.caps().kvm_clock {
            let mut msrs = [0; 2];
            self.kvm().get_msrs(
                &[MSR_KVM_WALL_CLOCK_NEW, MSR_KVM_SYSTEM_TIME_NEW],
                &mut msrs,
            )?;
            [value.kvm_wall_clock, value.kvm_system_time] = msrs;
        }
        Ok(value)
    }

    /// Writes the Hyper-V synthetic MSRs and the KVM clock MSRs that the
    /// partition exposes.
    ///
    /// Returns whether the Hyper-V synthetic MSRs were written, in which case
    /// the caller must mirror them into the processor state.
    pub(super) fn set_synthetic_msrs(
        &mut self,
        value: &vp::SyntheticMsrs,
    ) -> Result<bool, KvmError> {
        if self.caps().hv1 {
            self.set_register_state(value)?;
        }
        if self.caps().kvm_clock {
            self.kvm().set_msrs(&[
                (MSR_KVM_WALL_CLOCK_NEW, value.kvm_wall_clock),
                (MSR_KVM_SYSTEM_TIME_NEW, value.kvm_system_time),
            ])?;
        }
        Ok(self.caps().hv1)
    }
}
