// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Returns well-formed `cargo build` invocations for building crates
//! specifically in the the hvlite repo.
//!
//! Uses the generic [`flowey_lib_common::run_cargo_build`] helper under
//! the hood, but fine-tunes the exposed API to the HvLite repo.

use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;
use std::collections::BTreeMap;

/// In the HvLite repo, we use a custom step to strip debug info from linux
/// binaries
///
/// We cannot use rustc's split DWARF option because Azure Watson does not
/// support split DWARF debuginfo.
#[derive(Serialize, Deserialize)]
pub enum CargoBuildOutput {
    WindowsBin {
        exe: PathBuf,
        pdb: PathBuf,
    },
    ElfBin {
        bin: PathBuf,
        dbg: Option<PathBuf>,
    },
    LinuxStaticLib {
        a: PathBuf,
    },
    LinuxDynamicLib {
        so: PathBuf,
    },
    WindowsStaticLib {
        lib: PathBuf,
        pdb: PathBuf,
    },
    WindowsDynamicLib {
        dll: PathBuf,
        dll_lib: PathBuf,
        pdb: PathBuf,
    },
    UefiBin {
        efi: PathBuf,
        pdb: PathBuf,
    },
}

impl CargoBuildOutput {
    pub fn from_base_cargo_build_output(
        base: flowey_lib_common::run_cargo_build::CargoBuildOutput,
        elf_dbg: Option<PathBuf>,
    ) -> Self {
        use flowey_lib_common::run_cargo_build::CargoBuildOutput as Base;

        match base {
            Base::WindowsBin { exe, pdb } => Self::WindowsBin { exe, pdb },
            Base::LinuxStaticLib { a } => Self::LinuxStaticLib { a },
            Base::LinuxDynamicLib { so } => Self::LinuxDynamicLib { so },
            Base::WindowsStaticLib { lib, pdb } => Self::WindowsStaticLib { lib, pdb },
            Base::WindowsDynamicLib { dll, dll_lib, pdb } => {
                Self::WindowsDynamicLib { dll, dll_lib, pdb }
            }
            Base::UefiBin { efi, pdb } => Self::UefiBin { efi, pdb },

            Base::ElfBin { bin } => Self::ElfBin { bin, dbg: elf_dbg },
        }
    }
}

/// Outside of a few binaries / libraries that are intimately tied to one
/// particular architecture / platform, most things in the hvlite tree run on a
/// common subset of supported target triples + build profiles.
pub mod common {
    use serde::Deserialize;
    use serde::Serialize;

    /// Vocabulary type for artifacts that only get built using the two most
    /// common cargo build profiles (i.e: `release` vs. `debug`).
    ///
    /// More specialized artifacts should use the
    /// [`BuildProfile`](super::BuildProfile) type, which enumerates _all_ build
    /// profiles defined in HvLite's `Cargo.toml` file.
    #[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub enum CommonProfile {
        Release,
        Debug,
    }

    impl CommonProfile {
        pub fn from_release(release: bool) -> Self {
            match release {
                true => Self::Release,
                false => Self::Debug,
            }
        }

        pub fn to_release(self) -> bool {
            match self {
                Self::Release => true,
                Self::Debug => false,
            }
        }
    }

    impl From<CommonProfile> for super::BuildProfile {
        fn from(value: CommonProfile) -> Self {
            match value {
                CommonProfile::Release => super::BuildProfile::Release,
                CommonProfile::Debug => super::BuildProfile::Debug,
            }
        }
    }

    /// Vocabulary type for artifacts that only get built for the most common
    /// actively-supported architectures in the hvlite tree.
    #[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub enum CommonArch {
        X86_64,
        Aarch64,
    }

    impl CommonArch {
        pub fn as_arch(&self) -> target_lexicon::Architecture {
            match self {
                CommonArch::X86_64 => target_lexicon::Architecture::X86_64,
                CommonArch::Aarch64 => target_lexicon::Architecture::Aarch64(
                    target_lexicon::Aarch64Architecture::Aarch64,
                ),
            }
        }

        pub fn from_triple(triple: &target_lexicon::Triple) -> Option<Self> {
            let arch = match triple.architecture {
                target_lexicon::Architecture::Aarch64(
                    target_lexicon::Aarch64Architecture::Aarch64,
                ) => Self::Aarch64,
                target_lexicon::Architecture::X86_64 => Self::X86_64,
                _ => return None,
            };
            Some(arch)
        }
    }

    /// Vocabulary type for artifacts that only get built for the most common
    /// actively-supported platforms in the hvlite tree.
    #[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub enum CommonPlatform {
        WindowsMsvc,
        LinuxGnu,
        LinuxMusl,
        MacOs,
    }

    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
    pub enum CommonTriple {
        Common {
            arch: CommonArch,
            platform: CommonPlatform,
        },
        Custom(target_lexicon::Triple),
    }

    impl CommonTriple {
        pub const X86_64_WINDOWS_MSVC: Self = Self::Common {
            arch: CommonArch::X86_64,
            platform: CommonPlatform::WindowsMsvc,
        };
        pub const X86_64_LINUX_GNU: Self = Self::Common {
            arch: CommonArch::X86_64,
            platform: CommonPlatform::LinuxGnu,
        };
        pub const X86_64_LINUX_MUSL: Self = Self::Common {
            arch: CommonArch::X86_64,
            platform: CommonPlatform::LinuxMusl,
        };
        pub const AARCH64_WINDOWS_MSVC: Self = Self::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::WindowsMsvc,
        };
        pub const AARCH64_LINUX_GNU: Self = Self::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxGnu,
        };
        pub const AARCH64_LINUX_MUSL: Self = Self::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxMusl,
        };
    }

    impl std::fmt::Debug for CommonTriple {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.as_triple(), f)
        }
    }

    impl std::fmt::Display for CommonTriple {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&self.as_triple(), f)
        }
    }

    impl PartialOrd for CommonTriple {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for CommonTriple {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.as_triple()
                .to_string()
                .cmp(&other.as_triple().to_string())
        }
    }

    impl CommonTriple {
        pub fn as_triple(&self) -> target_lexicon::Triple {
            match self {
                CommonTriple::Common { arch, platform } => match platform {
                    CommonPlatform::WindowsMsvc => target_lexicon::Triple {
                        architecture: arch.as_arch(),
                        vendor: target_lexicon::Vendor::Pc,
                        operating_system: target_lexicon::OperatingSystem::Windows,
                        environment: target_lexicon::Environment::Msvc,
                        binary_format: target_lexicon::BinaryFormat::Coff,
                    },
                    CommonPlatform::LinuxGnu => target_lexicon::Triple {
                        architecture: arch.as_arch(),
                        vendor: target_lexicon::Vendor::Unknown,
                        operating_system: target_lexicon::OperatingSystem::Linux,
                        environment: target_lexicon::Environment::Gnu,
                        binary_format: target_lexicon::BinaryFormat::Elf,
                    },
                    CommonPlatform::LinuxMusl => target_lexicon::Triple {
                        architecture: arch.as_arch(),
                        vendor: target_lexicon::Vendor::Unknown,
                        operating_system: target_lexicon::OperatingSystem::Linux,
                        environment: target_lexicon::Environment::Musl,
                        binary_format: target_lexicon::BinaryFormat::Elf,
                    },
                    CommonPlatform::MacOs => target_lexicon::Triple {
                        architecture: arch.as_arch(),
                        vendor: target_lexicon::Vendor::Apple,
                        operating_system: target_lexicon::OperatingSystem::Darwin(None),
                        environment: target_lexicon::Environment::Unknown,
                        binary_format: target_lexicon::BinaryFormat::Macho,
                    },
                },
                CommonTriple::Custom(t) => t.clone(),
            }
        }

        pub fn common_arch(&self) -> Option<CommonArch> {
            match self {
                CommonTriple::Common { arch, .. } => Some(*arch),
                CommonTriple::Custom(target) => CommonArch::from_triple(target),
            }
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuildProfile {
    Debug,
    Release,
    UnderhillShip,
    BootDev,
    BootRelease,
    Light,
}

flowey_request! {
    pub struct Request {
        pub crate_name: String,
        pub out_name: String,
        pub profile: BuildProfile, // lock to only hvlite build profiles
        pub features: CargoFeatureSet,
        pub crate_type: CargoCrateType,
        pub target: target_lexicon::Triple,
        /// If supported by the target, build without split debuginfo.
        pub no_split_dbg_info: bool,
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Wait for specified side-effects to resolve before running cargo-run.
        ///
        /// (e.g: to allow for some ambient packages / dependencies to get
        /// installed).
        pub pre_build_deps: Vec<ReadVar<SideEffect>>,
        /// Resulting build output
        pub output: WriteVar<CargoBuildOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::run_split_debug_info::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<flowey_lib_common::run_cargo_build::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let base_pre_build_deps =
            [ctx.reqv(crate::install_openvmm_rust_build_essential::Request)].to_vec();

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        for Request {
            crate_name,
            out_name,
            profile,
            features,
            crate_type,
            mut target,
            no_split_dbg_info,
            extra_env,
            pre_build_deps: user_pre_build_deps,
            output,
        } in requests
        {
            let mut pre_build_deps = base_pre_build_deps.clone();
            pre_build_deps.extend(user_pre_build_deps);

            // FIXME: because we set `CC_{arch}_unknown_linux_musl` in our cargo env,
            // we end up compiling _every_ musl artifact using the openhcl musl
            // toolchain.
            //
            // it's not super clear how to fix this in a clean way without breaking the
            // dev-ex of anyone using rust-analyzer though...
            let sysroot_arch = match target.architecture {
                target_lexicon::Architecture::Aarch64(_) => {
                    crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch::Aarch64
                }
                target_lexicon::Architecture::X86_64 => {
                    crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch::X64
                }
                arch => anyhow::bail!("unsupported arch {arch}"),
            };

            if matches!(target.environment, target_lexicon::Environment::Musl) {
                pre_build_deps.push(
                    ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch: sysroot_arch,
                        path: v,
                    })
                    .into_side_effect(),
                );
            }

            let injected_env = ctx.reqv(|v| crate::init_cross_build::Request {
                target: target.clone(),
                injected_env: v,
            });

            let extra_env = if let Some(extra_env) = extra_env {
                extra_env
                    .zip(ctx, injected_env)
                    .map(ctx, move |(mut a, b)| {
                        a.extend(b);
                        a
                    })
            } else {
                injected_env
            };

            let mut config = Vec::new();

            // If the target vendor is specified as `minimal_rt`, then this is
            // our custom target triple for the minimal_rt toolchain. Include the appropriate
            // config file.
            let passed_target = if target.vendor.as_str() == "minimal_rt" {
                config.push(format!(
                    "openhcl/minimal_rt/{arch}-config.toml",
                    arch = target.architecture.into_str()
                ));
                if target.architecture == target_lexicon::Architecture::X86_64 {
                    // x86-64 doesn't actually use a custom target currently,
                    // since the x86_64-unknown-none target is stage 2 and has
                    // reasonable defaults.
                    target.vendor = target_lexicon::Vendor::Unknown;
                    Some(target.clone())
                } else {
                    // We are building the target from source, so don't try to
                    // install it via rustup. But do make sure the rust-src
                    // component is available.
                    ctx.req(flowey_lib_common::install_rust::Request::InstallComponent(
                        "rust-src".into(),
                    ));
                    None
                }
            } else {
                Some(target.clone())
            };

            let base_output = ctx.reqv(|v| flowey_lib_common::run_cargo_build::Request {
                in_folder: openvmm_repo_path.clone(),
                crate_name,
                out_name,
                profile: match profile {
                    BuildProfile::Debug => CargoBuildProfile::Debug,
                    BuildProfile::Release => CargoBuildProfile::Release,
                    BuildProfile::UnderhillShip => {
                        CargoBuildProfile::Custom("underhill-ship".into())
                    }
                    BuildProfile::BootDev => CargoBuildProfile::Custom("boot-dev".into()),
                    BuildProfile::BootRelease => CargoBuildProfile::Custom("boot-release".into()),
                    BuildProfile::Light => CargoBuildProfile::Custom("light".into()),
                },
                features,
                output_kind: crate_type,
                target: passed_target,
                extra_env: Some(extra_env),
                config,
                pre_build_deps,
                output: v,
            });

            if !no_split_dbg_info
                && matches!(
                    (crate_type, target.operating_system),
                    (
                        CargoCrateType::Bin,
                        target_lexicon::OperatingSystem::Linux
                            | target_lexicon::OperatingSystem::None_
                    )
                )
            {
                let elf_bin = base_output.clone().map(ctx, |o| match o {
                    flowey_lib_common::run_cargo_build::CargoBuildOutput::ElfBin { bin } => bin,
                    _ => unreachable!(),
                });

                let (out_bin, write_out_bin) = ctx.new_var();
                let (out_dbg, write_out_dbg) = ctx.new_var();

                ctx.req(crate::run_split_debug_info::Request {
                    arch: match target.architecture {
                        target_lexicon::Architecture::Aarch64(_) => common::CommonArch::Aarch64,
                        target_lexicon::Architecture::X86_64 => common::CommonArch::X86_64,
                        _ => anyhow::bail!("cannot split linux dbginfo on specified arch"),
                    },
                    in_bin: elf_bin,
                    out_bin: write_out_bin,
                    out_dbg_info: write_out_dbg,
                });

                ctx.emit_minor_rust_step("reporting split debug info", |ctx| {
                    let out_bin = out_bin.claim(ctx);
                    let out_dbg = out_dbg.claim(ctx);
                    let base_output = base_output.claim(ctx);
                    let output = output.claim(ctx);

                    move |rt| {
                        let mut fixed = CargoBuildOutput::from_base_cargo_build_output(
                            rt.read(base_output),
                            Some(rt.read(out_dbg)),
                        );
                        let CargoBuildOutput::ElfBin { bin, .. } = &mut fixed else {
                            unreachable!()
                        };
                        *bin = rt.read(out_bin);
                        rt.write(output, &fixed);
                    }
                });
            } else {
                base_output.write_into(ctx, output, |o| {
                    CargoBuildOutput::from_base_cargo_build_output(o, None)
                });
            }
        }

        Ok(())
    }
}
