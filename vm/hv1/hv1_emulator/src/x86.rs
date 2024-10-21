// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X86-specific HV1 definitions.

/// The value returned by the Microsoft hypervisor for reads of the
/// [`x86defs::X86X_IA32_MSR_MISC_ENABLE`] MSR.
///
/// This may be tweaked if performance monitoring is available.
pub const MISC_ENABLE: x86defs::MiscEnable = x86defs::MiscEnable::new()
    .with_fast_string(true)
    .with_tm1(true)
    .with_bts_unavailable(true)
    .with_pebs_unavailable(true)
    .with_enhanced_speedstep(true)
    .with_mwait(true)
    .with_xtpr_disable(true);
