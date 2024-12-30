// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to support emulating hypercalls.

/// Sets a VP context, as in `HvEnableVpVtl` and `HvStartVirtualProcessor`.
///
/// Sets the register state and clears halt/wait-for-sipi states.
pub fn set_x86_vp_context<T: virt::x86::vp::AccessVpState>(
    access: &mut T,
    context: &hvdef::hypercall::InitialVpContextX64,
) -> Result<(), T::Error> {
    let &hvdef::hypercall::InitialVpContextX64 {
        rip,
        rsp,
        rflags,
        cs,
        ds,
        es,
        fs,
        gs,
        ss,
        tr,
        ldtr,
        idtr,
        gdtr,
        efer,
        cr0,
        cr3,
        cr4,
        msr_cr_pat,
    } = context;

    let registers = access.registers()?;
    let registers = virt::x86::vp::Registers {
        rsp,
        rip,
        rflags,
        cr0,
        cr3,
        cr4,
        efer,
        cs: cs.into(),
        ds: ds.into(),
        es: es.into(),
        fs: fs.into(),
        gs: gs.into(),
        ss: ss.into(),
        tr: tr.into(),
        ldtr: ldtr.into(),
        idtr: idtr.into(),
        gdtr: gdtr.into(),
        ..registers
    };

    access.set_registers(&registers)?;
    access.set_pat(&virt::x86::vp::Pat { value: msr_cr_pat })?;

    let mut activity = access.activity()?;
    activity.mp_state = virt::x86::vp::MpState::Running;
    access.set_activity(&activity)?;

    Ok(())
}
