// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure the OpenVMM repo is `clippy` clean.

use crate::common::CommonArch;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;
use flowey_lib_common::run_cargo_clippy::CargoPackage;

flowey_request! {
    pub struct Request {
        pub target: target_lexicon::Triple,
        pub profile: CommonProfile,
        pub done: WriteVar<SideEffect>,
        pub also_check_misc_nostd_crates: bool,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::run_cargo_clippy::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            done,
            also_check_misc_nostd_crates,
        } = request;

        let flowey_platform = ctx.platform();
        let flowey_arch = ctx.arch();

        let sysroot_arch = CommonArch::from_architecture(target.architecture)?;
        let (boot_target, uefi_target) = match sysroot_arch {
            CommonArch::X86_64 => ("x86_64-unknown-none", "x86_64-unknown-uefi"),
            CommonArch::Aarch64 => ("aarch64-unknown-linux-musl", "aarch64-unknown-uefi"),
        };

        let mut pre_build_deps = Vec::new();

        // FIXME: this will go away once we have a dedicated cargo .config.toml
        // for the openhcl _bin_. until we have that, we are building _every_
        // musl target using the openhcl toolchain...

        if matches!(target.environment, target_lexicon::Environment::Musl) {
            pre_build_deps.push(
                ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                    arch: sysroot_arch,
                    path: v,
                })
                .into_side_effect(),
            );
        }

        ctx.req(flowey_lib_common::install_rust::Request::InstallTargetTriple(target.clone()));
        if also_check_misc_nostd_crates {
            ctx.req(
                flowey_lib_common::install_rust::Request::InstallTargetTriple(
                    target_lexicon::triple!(uefi_target),
                ),
            );
            ctx.req(
                flowey_lib_common::install_rust::Request::InstallTargetTriple(
                    target_lexicon::triple!(boot_target),
                ),
            );
        }

        // TODO: install build tools for other platforms
        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            pre_build_deps.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![
                        "libssl-dev".into(),
                        "pkg-config".into(),
                        "build-essential".into(),
                    ],
                    done: v,
                }
            }));
        }

        pre_build_deps.push(ctx.reqv(crate::install_openvmm_rust_build_essential::Request));

        // Cross compiling for MacOS isn't supported, but clippy still works
        // with no additional dependencies
        if !matches!(
            target.operating_system,
            target_lexicon::OperatingSystem::Darwin(_)
        ) {
            pre_build_deps.push(
                ctx.reqv(|v| crate::init_cross_build::Request {
                    target: target.clone(),
                    injected_env: v,
                })
                .into_side_effect(),
            );
        }

        let xtask_target = CommonTriple::Common {
            arch: flowey_arch.try_into()?,
            platform: flowey_platform.try_into()?,
        };

        let xtask = ctx.reqv(|v| crate::build_xtask::Request {
            target: xtask_target,
            xtask: v,
        });

        let profile = match profile {
            CommonProfile::Release => CargoBuildProfile::Release,
            CommonProfile::Debug => CargoBuildProfile::Debug,
        };

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let exclude = ctx.emit_rust_stepv("determine clippy exclusions", |ctx| {
            let xtask = xtask.claim(ctx);
            let repo_path = openvmm_repo_path.clone().claim(ctx);
            move |rt| {
                let xtask = rt.read(xtask);
                let repo_path = rt.read(repo_path);

                // guest_test_uefi is uefi-only, and is handled separately below
                // crypto is handled separately in order to deal with its non-additive features
                let mut exclude = vec!["guest_test_uefi".into(), "crypto".into()];

                // packages depending on libfuzzer-sys are currently x86 only
                if !(matches!(target.architecture, target_lexicon::Architecture::X86_64)
                    && matches!(flowey_arch, FlowArch::X86_64))
                {
                    let xtask_bin = match xtask {
                        crate::build_xtask::XtaskOutput::LinuxBin { bin, dbg: _ } => bin,
                        crate::build_xtask::XtaskOutput::WindowsBin { exe, pdb: _ } => exe,
                    };

                    rt.sh.change_dir(repo_path);
                    let output =
                        flowey::shell_cmd!(rt, "{xtask_bin} fuzz list --crates").output()?;
                    let output = String::from_utf8(output.stdout)?;

                    let fuzz_crates = output.trim().split('\n').map(|s| s.to_owned());
                    exclude.extend(fuzz_crates);
                }

                // packages requiring crypto or openssl support won't cross compile for macos
                if matches!(
                    target.operating_system,
                    target_lexicon::OperatingSystem::Darwin(_)
                ) {
                    exclude.extend(["openssl_kdf", "vmgs_lib", "disk_crypt"].map(|x| x.into()));
                }

                Ok(Some(exclude))
            }
        });

        // On windows & mac, we can't build with all features, as many crates
        // require openSSL for crypto, which isn't supported in CI yet.
        let features = if matches!(
            target.operating_system,
            target_lexicon::OperatingSystem::Windows | target_lexicon::OperatingSystem::Darwin(_)
        ) {
            CargoFeatureSet::None
        } else {
            CargoFeatureSet::All
        };

        let mut reqs = vec![ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
            in_folder: openvmm_repo_path.clone(),
            package: CargoPackage::Workspace,
            profile: profile.clone(),
            features: features.clone(),
            target: target.clone(),
            extra_env: None,
            exclude,
            keep_going: true,
            all_targets: true,
            pre_build_deps: pre_build_deps.clone(),
            done: v,
        })];

        // crypto has non-additive features, we need to ensure full coverage of different backends.
        // Always test the 'native' no-feature backends.
        reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
            in_folder: openvmm_repo_path.clone(),
            package: CargoPackage::Crate("crypto".into()),
            profile: profile.clone(),
            features: CargoFeatureSet::None,
            target: target.clone(),
            extra_env: None,
            exclude: ReadVar::from_static(None),
            keep_going: true,
            all_targets: true,
            pre_build_deps: pre_build_deps.clone(),
            done: v,
        }));

        // Always test the pure rust backend.
        reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
            in_folder: openvmm_repo_path.clone(),
            package: CargoPackage::Crate("crypto".into()),
            profile: profile.clone(),
            features: CargoFeatureSet::Specific(vec!["rust".into()]),
            target: target.clone(),
            extra_env: None,
            exclude: ReadVar::from_static(None),
            keep_going: true,
            all_targets: true,
            pre_build_deps: pre_build_deps.clone(),
            done: v,
        }));

        // Then on linux test the openssl & symcrypt backends, and ensure that --all-features works properly.
        // We could test openssl on non-linux targets too, but setting up builds for them is a pain.
        if matches!(
            target.operating_system,
            target_lexicon::OperatingSystem::Linux
        ) {
            reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
                in_folder: openvmm_repo_path.clone(),
                package: CargoPackage::Crate("crypto".into()),
                profile: profile.clone(),
                features: CargoFeatureSet::Specific(vec!["openssl".into()]),
                target: target.clone(),
                extra_env: None,
                exclude: ReadVar::from_static(None),
                keep_going: true,
                all_targets: true,
                pre_build_deps: pre_build_deps.clone(),
                done: v,
            }));
            // Only test the symcrypt backend on musl targets with our prebuilt lib
            if matches!(target.environment, target_lexicon::Environment::Musl) {
                reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
                    in_folder: openvmm_repo_path.clone(),
                    package: CargoPackage::Crate("crypto".into()),
                    profile: profile.clone(),
                    features: CargoFeatureSet::Specific(vec!["symcrypt".into()]),
                    target: target.clone(),
                    extra_env: None,
                    exclude: ReadVar::from_static(None),
                    keep_going: true,
                    all_targets: true,
                    pre_build_deps: pre_build_deps.clone(),
                    done: v,
                }));
            }
            reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
                in_folder: openvmm_repo_path.clone(),
                package: CargoPackage::Crate("crypto".into()),
                profile: profile.clone(),
                features: CargoFeatureSet::All,
                target: target.clone(),
                extra_env: None,
                exclude: ReadVar::from_static(None),
                keep_going: true,
                all_targets: true,
                pre_build_deps: pre_build_deps.clone(),
                done: v,
            }));
        }

        if also_check_misc_nostd_crates {
            // don't pass --all-targets, since that pulls in a std dependency
            reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
                in_folder: openvmm_repo_path.clone(),
                package: CargoPackage::Crate("openhcl_boot".into()),
                profile: profile.clone(),
                features: CargoFeatureSet::All,
                target: target_lexicon::triple!(boot_target),
                extra_env: Some(vec![("MINIMAL_RT_BUILD".into(), "1".into())]),
                exclude: ReadVar::from_static(None),
                keep_going: true,
                all_targets: false,
                pre_build_deps: pre_build_deps.clone(),
                done: v,
            }));

            // don't pass --all-targets, since that pulls in a std dependency
            reqs.push(ctx.reqv(|v| flowey_lib_common::run_cargo_clippy::Request {
                in_folder: openvmm_repo_path.clone(),
                package: CargoPackage::Crate("guest_test_uefi".into()),
                profile: profile.clone(),
                features: CargoFeatureSet::All,
                target: target_lexicon::triple!(uefi_target),
                extra_env: None,
                exclude: ReadVar::from_static(None),
                keep_going: true,
                all_targets: false,
                pre_build_deps: pre_build_deps.clone(),
                done: v,
            }));
        }

        ctx.emit_side_effect_step(reqs, [done]);

        Ok(())
    }
}
