// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that code uses `guest_arch` instead of `target_arch` for
//! guest-architecture-specific conditionals.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use std::path::Path;
use toml_edit::DocumentMut;

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
/// One off - used by petri to return the host architecture for test filtering
const SUPPRESS_REASON_ONEOFF_PETRI_HOST_ARCH: &str = "oneoff-petri-host-arch";

fn has_suppress(s: &str) -> bool {
    let Some((_, after)) = s.split_once(SUPPRESS) else {
        return false;
    };

    let after = after.trim();
    let justification = after.split(' ').next().unwrap();

    matches!(
        justification,
        SUPPRESS_REASON_CPU_INTRINSIC
            | SUPPRESS_REASON_SYS_CRATE
            | SUPPRESS_REASON_DEPENDENCY
            | SUPPRESS_REASON_ONEOFF_GUEST_ARCH_IMPL
            | SUPPRESS_REASON_ONEOFF_FLOWEY
            | SUPPRESS_REASON_ONEOFF_PETRI_NATIVE_TEST_DEPS
            | SUPPRESS_REASON_ONEOFF_PETRI_HOST_ARCH
    )
}

/// Paths that are exempt from the target_arch lint.
fn is_exempt(path: &Path) -> bool {
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
    // The [open]TMK-related crates run in the guest and are inherently arch-specific.
    path.starts_with("guest_test_uefi")
        || path.starts_with("openhcl/openhcl_boot")
        || path.starts_with("openhcl/minimal_rt")
        || path.starts_with("openhcl/minimal_rt_reloc")
        || path.starts_with("openhcl/sidecar")
        || path.starts_with("support")
        || path.starts_with("tmk/simple_tmk")
        || path.starts_with("tmk/tmk_core")
        || path.starts_with("opentmk")
        || path.starts_with("vm/whp")
        || path.starts_with("vm/kvm")
}

pub struct CfgTargetArch;

impl Lint for CfgTargetArch {
    fn new(_ctx: &LintCtx) -> Self {
        CfgTargetArch
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        let path = content.path();

        // Exclude ourselves from the lint (we mention the patterns in strings).
        if path.ends_with(file!()) || is_exempt(path) {
            return;
        }

        let mut prev_line = "";
        for (i, line) in content.lines().enumerate() {
            if line.contains("target_arch =") || line.contains("CARGO_CFG_TARGET_ARCH") {
                // check if current line contains valid suppress, or is commented out
                if !line.trim().starts_with("//") && !has_suppress(line) && !has_suppress(prev_line)
                {
                    content.unfixable(&format!(
                        "unjustified `cfg(target_arch = ...)` at line {}",
                        i + 1
                    ));
                }
            }
            prev_line = line;
        }
    }

    fn exit_crate(&mut self, _content: &mut Lintable<DocumentMut>) {}
    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}

#[cfg(test)]
mod tests {
    use super::super::test_support::failed;
    use super::super::test_support::lintable;
    use super::*;

    fn fails(path: &str, text: &str) -> bool {
        let mut file = lintable(path, text.to_owned(), false);
        CfgTargetArch.visit_file(&mut file);
        failed(&file)
    }

    // Test: Lint target-architecture conditionals that are unjustified, justified
    //       on the same or previous line, commented out, or use an unknown reason.
    // Expected: Only recognized justifications, comments and exempt paths pass;
    //       unknown or empty reasons and justifications two lines above fail.
    #[test]
    fn requires_known_adjacent_justification_outside_exempt_paths() {
        let p = "vm/devices/x/src/lib.rs";
        let cfg = "#[cfg(target_arch = \"x86_64\")]";
        assert!(fails(p, cfg));
        assert!(fails(p, "let a = env!(\"CARGO_CFG_TARGET_ARCH\");"));
        assert!(!fails(
            p,
            &format!("{cfg} // xtask-fmt allow-target-arch sys-crate")
        ));
        assert!(!fails(
            p,
            &format!("// xtask-fmt allow-target-arch cpu-intrinsic\n{cfg}")
        ));
        assert!(fails(
            p,
            &format!("// xtask-fmt allow-target-arch cpu-intrinsic\n\n{cfg}")
        ));
        assert!(fails(
            p,
            &format!("{cfg} // xtask-fmt allow-target-arch because")
        ));
        assert!(fails(p, &format!("{cfg} // xtask-fmt allow-target-arch")));
        assert!(!fails(p, &format!("   // {cfg}")));
        assert!(!fails(p, "#[cfg(guest_arch = \"x86_64\")]"));
        for exempt in [
            "support/a/src/lib.rs",
            "opentmk/x/src/lib.rs",
            "vm/kvm/src/lib.rs",
        ] {
            assert!(!fails(exempt, cfg), "{exempt}");
        }
        assert!(fails("vm/kvmx/src/lib.rs", cfg));
    }
}
