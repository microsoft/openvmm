// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the XSAVE state that a restore sets.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp::AccessVpState as _;
use virt::x86::vp::Xsave;
use x86defs::xsave::X86X_XSAVE_LEGACY_FEATURES;
use x86defs::xsave::XCOMP_COMPRESSED;
use x86defs::xsave::XSAVE_LEGACY_LEN;
use x86defs::xsave::XSAVE_VARIABLE_OFFSET;

/// The index, among the 64-bit words of an XSAVE area, of XSTATE_BV, the first
/// word of the header, which XCOMP_BV follows.
const XSTATE_BV: usize = XSAVE_LEGACY_LEN / size_of::<u64>();
const XCOMP_BV: usize = XSTATE_BV + 1;
/// The number of words of the legacy area and the header, which the area holds
/// before its components.
const HEADER_END: usize = XSAVE_VARIABLE_OFFSET / size_of::<u64>();

// A restore writes the compact XSAVE area of the saved state in the standard
// format that KVM takes: its legacy area, its header, and each component that
// the header selects, from the place that the header gives it. A save always
// returns an area that holds them, so only a crafted or damaged saved state
// does not. Its restore must be refused, instead of panicking, and leave the
// VP's XSAVE state as it was.
backend_test!(refuses_a_short_or_malformed_xsave_area, kvm_only, requires: []);
fn refuses_a_short_or_malformed_xsave_area(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let xsave = &partition.caps().xsave;
    let mut state = processor.access_state(Vtl::Vtl0);
    let saved = state.xsave()?;

    // Copies of the saved area cut short before the header ends, down to the
    // legacy area alone and to no word at all, are refused, and the saved area
    // itself restores.
    let mut refused = Vec::new();
    for words in [0, 1, XSTATE_BV, XCOMP_BV, HEADER_END - 1] {
        refused.push((
            format!("the saved area cut to {words} words"),
            Xsave {
                data: saved.data[..words].to_vec(),
            },
        ));
    }
    let mut accepted = vec![(
        "the saved area".to_string(),
        Xsave {
            data: saved.data.clone(),
        },
    )];

    // Areas that select the first component past x87 and SSE, which is AVX on
    // a host that has it, in XSTATE_BV and XCOMP_BV, so that the header places
    // it right after itself, are refused when they end before the component
    // does, and restore when they end where it does.
    if let Some(component) = (xsave.features & !X86X_XSAVE_LEGACY_FEATURES).lowest_one() {
        let size = xsave.feature_info[component as usize].len as usize;
        let end = (XSAVE_VARIABLE_OFFSET + size).div_ceil(size_of::<u64>());
        let selecting = |words| {
            let mut data = saved.data[..words].to_vec();
            data[XSTATE_BV] = 1 << component;
            data[XCOMP_BV] = XCOMP_COMPRESSED | 1 << component;
            let name = format!("{words} words that select component {component}");
            (name, Xsave { data })
        };
        refused.extend([selecting(HEADER_END), selecting(end - 1)]);
        accepted.push(selecting(end));
    }

    for (name, area) in &refused {
        let before = state.xsave()?;
        anyhow::ensure!(state.set_xsave(area).is_err(), "restoring {name} succeeded");
        anyhow::ensure!(
            state.xsave()? == before,
            "the refused restore of {name} changed the XSAVE state"
        );
    }
    for (name, area) in &accepted {
        state
            .set_xsave(area)
            .with_context(|| format!("restoring {name} failed"))?;
    }
    Ok(())
}
