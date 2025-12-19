// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized build-script logic to support `cfg(guest_arch = ...)` directives
//! throughout the OpenVMM codebase.
//!
//! By default, `cfg(guest_arch = ...)` will be the same as Rust built-in
//! `cfg(target_arch = ...)`, but this can be overwritten by setting the
//! `OPENVMM_GUEST_TARGET=...` env var at compile time.
//!
//! OpenVMM code should not be written to assume that `guest_arch` and
//! `target_arch` will always be equal!
//!
//! At some point in the future, OpenVMM may integrate a full-blown CPU emulator
//! `virt_` backend (akin to what QEMU does), which would allow it to run guest
//! VMs with a _different_ architecture from the host machine.
//!
//! e.g: Aarch64 on x86, x86 on Aarch64, or even exotic things, like RISC-V on
//! x86 (assuming someone cares enough to put in the work there + add
//! appropriate CI coverage!).

#![expect(missing_docs)]
#![forbid(unsafe_code)]

#[derive(Copy, Clone, PartialEq)]
enum GuestArch {
    X86_64,
    Aarch64,
}

impl GuestArch {
    fn all() -> [Self; 2] {
        [GuestArch::X86_64, GuestArch::Aarch64]
    }

    fn as_str(&self) -> &'static str {
        match *self {
            GuestArch::X86_64 => "x86_64",
            GuestArch::Aarch64 => "aarch64",
        }
    }

    fn host_arch() -> Self {
        // xtask-fmt allow-target-arch oneoff-guest-arch-impl
        std::env::var("CARGO_CFG_TARGET_ARCH")
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl std::str::FromStr for GuestArch {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let arch = match s {
            "x86_64" => GuestArch::X86_64,
            "aarch64" => GuestArch::Aarch64,
            _ => return Err(format!("unknown guest arch '{s}'")),
        };
        Ok(arch)
    }
}

pub fn emit_guest_arch() {
    println!("cargo:rerun-if-env-changed=OPENVMM_GUEST_TARGET");
    let host_arch = GuestArch::host_arch();
    let arch = {
        if let Ok(s) = std::env::var("OPENVMM_GUEST_TARGET") {
            s.parse().unwrap()
        } else {
            host_arch
        }
    };

    println!("cargo:rustc-cfg=guest_arch=\"{}\"", arch.as_str());

    if host_arch == arch {
        println!("cargo:rustc-cfg=guest_is_native");
    }

    let possible_arches = GuestArch::all()
        .map(|a| format!("\"{}\"", a.as_str()))
        .join(",");
    println!(
        "cargo:rustc-check-cfg=cfg(guest_arch, values({}))",
        possible_arches
    );
    println!("cargo:rustc-check-cfg=cfg(guest_is_native)");
}
