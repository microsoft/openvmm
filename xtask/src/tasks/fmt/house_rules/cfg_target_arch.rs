// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::anyhow;
use fs_err::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

const SUPPRESS: &str = "xtask-fmt allow-target-arch";

/// Using `target_arch` in order to execute CPU-specific intrinsics
const SUPPRESS_REASON_CPU_INTRINSIC: &str = "cpu-intrinsic";
/// Using `target_arch` in order to implement a '*-sys'-like crate (where the
/// structure changes depending on the host-arch)
const SUPPRESS_REASON_SYS_CRATE: &str = "sys-crate";
/// A dependency of this crate will not build on other target architectures
const SUPPRESS_REASON_DEPENDENCY: &str = "dependency";
/// One off - support for the auto-arch selection logic in
/// `build_rs_guest_arch`.
const SUPPRESS_REASON_ONEOFF_GUEST_ARCH_IMPL: &str = "oneoff-guest-arch-impl";
/// One off - used as part of flowey CI infra
const SUPPRESS_REASON_ONEOFF_FLOWEY: &str = "oneoff-flowey";
/// One off - used by petri to select native test dependencies
const SUPPRESS_REASON_ONEOFF_PETRI_NATIVE_TEST_DEPS: &str = "oneoff-petri-native-test-deps";
/// Onee off - used by petri to return the host architecture for test filtering
const SUPPRESS_REASON_ONEOFF_PETRI_HOST_ARCH: &str = "oneoff-petri-host-arch";

fn has_suppress(s: &str) -> bool {
    let Some((_, after)) = s.split_once(SUPPRESS) else {
        return false;
    };

    let after = after.trim();
    let justification = after.split(' ').next().unwrap();

    let ok = matches!(
        justification,
        SUPPRESS_REASON_CPU_INTRINSIC
            | SUPPRESS_REASON_SYS_CRATE
            | SUPPRESS_REASON_DEPENDENCY
            | SUPPRESS_REASON_ONEOFF_GUEST_ARCH_IMPL
            | SUPPRESS_REASON_ONEOFF_FLOWEY
            | SUPPRESS_REASON_ONEOFF_PETRI_NATIVE_TEST_DEPS
            | SUPPRESS_REASON_ONEOFF_PETRI_HOST_ARCH
    );

    if !ok {
        log::error!(
            "invalid justification '{}' (must be one of [sys-crate, cpu-intrinsic]",
            after.split(' ').next().unwrap()
        );
    }

    ok
}

pub fn check_cfg_target_arch(path: &Path, _fix: bool) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if !matches!(ext, "rs") {
        return Ok(());
    }

    // need to exclude self (and house_rules.rs, which includes help-text) from
    // the lint
    if path == Path::new(file!()) || path == Path::new(super::PATH_TO_HOUSE_RULES_RS) {
        return Ok(());
    }

    // guest_test_uefi is a guest-side crate (the code runs in the guest), so
    // target_arch here is actually referring to the guest_arch
    //
    // openhcl_boot uses target_arch liberally, since it runs in VTL2 entirely
    // in-service to the VTL2 linux kernel, which will always be native-arch.
    // Similar for the sidecar kernel and TMKs. And minimal_rt provides the
    // (arch-specific) runtime for both of them.
    //
    // support crates are not VM specific, so guest_arch doesn't make sense
    // there.
    //
    // the whp/kvm crates are inherently arch-specific, as they contain
    // low-level bindings to a particular platform's virtualization APIs
    //
    // The TMK-related crates run in the guest and are inherently arch-specific.
    if path.starts_with("guest_test_uefi")
        || path.starts_with("openhcl/openhcl_boot")
        || path.starts_with("openhcl/minimal_rt")
        || path.starts_with("openhcl/sidecar")
        || path.starts_with("support/")
        || path.starts_with("tmk/simple_tmk")
        || path.starts_with("tmk/tmk_core")
        || path.starts_with("vm/whp")
        || path.starts_with("vm/kvm")
    {
        return Ok(());
    }

    let mut error = false;

    // TODO: this lint really ought to be a dynlint / clippy lint
    let f = BufReader::new(File::open(path)?);
    let mut prev_line = String::new();
    for (i, line) in f.lines().enumerate() {
        let line = line?;
        if line.contains("target_arch =") || line.contains("CARGO_CFG_TARGET_ARCH") {
            // check if current line contains valid suppress, or is commented out
            if !line.trim().starts_with("//") && !has_suppress(&line) && !has_suppress(&prev_line) {
                error = true;
                log::error!(
                    "unjustified `cfg(target_arch = ...)`: {}:{}",
                    path.display(),
                    i + 1
                );
            }
        }
        prev_line = line;
    }

    if error {
        Err(anyhow!(
            "found unjustified uses of `cfg(target_arch = ...)` in {}",
            path.display()
        ))
    } else {
        Ok(())
    }
}
