// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrapper around `update-rootfs.py`

use crate::resolve_openvmm_deps::OpenvmmDepsArch;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct OpenhclInitrdOutput {
    pub initrd: PathBuf,
}

/// Extra parameters for building specialized initrd files.
#[derive(Default, Clone, Serialize, Deserialize, Debug)]
pub struct OpenhclInitrdExtraParams {
    /// additional layers to be included in the initrd
    pub extra_initrd_layers: Vec<PathBuf>,
    /// additional directories to be included in the initrd
    pub extra_initrd_directories: Vec<PathBuf>,
    /// Path to custom kernel modules. If not provided, uses modules under the
    /// kernel package path.
    pub custom_kernel_modules: Option<PathBuf>,
}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        /// include --interactive tools
        pub interactive: bool,
        /// Extra parameters for building specialized initrd files.
        pub extra_params: Option<OpenhclInitrdExtraParams>,
        /// Paths to rootfs.config files
        pub rootfs_config: Vec<ReadVar<PathBuf>>,
        /// Extra environment variables to set during the run (e.g: to
        /// interpolate paths into `rootfs.config`)
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Path to kernel package
        pub kernel_package_root: ReadVar<PathBuf>,
        /// Path to the openhcl bin to use
        pub bin_openhcl: ReadVar<PathBuf>,
        /// Output path of generated initrd
        pub initrd: WriteVar<OpenhclInitrdOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // ambient deps required by `update-rootfs.py`
        let platform = ctx.platform();
        let python_pkg = match platform {
            FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                FlowPlatformLinuxDistro::Fedora | FlowPlatformLinuxDistro::Ubuntu => "python3",
                FlowPlatformLinuxDistro::Arch => "python",
                FlowPlatformLinuxDistro::Unknown => anyhow::bail!("Unknown Linux distribution"),
            },
            _ => anyhow::bail!("Unsupported platform"),
        };
        let pydeps =
            ctx.reqv(
                |side_effect| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: [python_pkg].map(Into::into).into(),
                    done: side_effect,
                },
            );

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        for Request {
            arch,
            extra_params,
            rootfs_config,
            kernel_package_root,
            extra_env,
            bin_openhcl,
            initrd,
            interactive,
        } in requests
        {
            let OpenhclInitrdExtraParams {
                extra_initrd_layers,
                extra_initrd_directories,
                custom_kernel_modules,
            } = extra_params.unwrap_or_default();

            let openvmm_deps_arch = match arch {
                CommonArch::X86_64 => OpenvmmDepsArch::X86_64,
                CommonArch::Aarch64 => OpenvmmDepsArch::Aarch64,
            };

            let interactive_dep = if interactive {
                ctx.reqv(|v| {
                    crate::resolve_openvmm_deps::Request::GetOpenhclCpioDbgrd(openvmm_deps_arch, v)
                })
            } else {
                ctx.reqv(|v| {
                    crate::resolve_openvmm_deps::Request::GetOpenhclCpioShell(openvmm_deps_arch, v)
                })
            };

            if rootfs_config.is_empty() {
                anyhow::bail!("no rootfs files provided");
            }

            ctx.emit_rust_step("building openhcl initrd", |ctx| {
                pydeps.clone().claim(ctx);
                let interactive_dep = interactive_dep.claim(ctx);
                let rootfs_config = rootfs_config.claim(ctx);
                let extra_env = extra_env.claim(ctx);
                let bin_openhcl = bin_openhcl.claim(ctx);
                let openvmm_repo_path = openvmm_repo_path.clone().claim(ctx);
                let kernel_package_root = kernel_package_root.claim(ctx);
                let initrd = initrd.claim(ctx);
                move |rt| {
                    let interactive_dep = rt.read(interactive_dep);
                    let rootfs_config = rt.read(rootfs_config);
                    let extra_env = rt.read(extra_env);
                    let bin_openhcl = rt.read(bin_openhcl);
                    let openvmm_repo_path = rt.read(openvmm_repo_path);
                    let kernel_package_root = rt.read(kernel_package_root);

                    let sh = xshell::Shell::new()?;

                    let initrd_path = sh.current_dir().join("openhcl.cpio.gz");

                    let initrd_contents = {
                        let mut v = Vec::new();

                        if interactive {
                            // for busybox, gdbserver, and perf
                            v.push("--interactive".to_string());
                        } else {
                            // just a minimal shell
                            v.push("--min-interactive".to_string());
                        }

                        for dir in extra_initrd_layers {
                            v.push("--layer".into());
                            v.push(dir.display().to_string());
                        }

                        for dir in extra_initrd_directories {
                            v.push("--add-dir".into());
                            v.push(dir.display().to_string());
                        }

                        v
                    };

                    let kernel_modules =
                        custom_kernel_modules.unwrap_or(kernel_package_root.join("."));

                    let rootfs_py_arch = match arch {
                        CommonArch::X86_64 => "x86_64",
                        CommonArch::Aarch64 => "aarch64",
                    };

                    // FUTURE: to avoid making big changes to update-roots as
                    // part of the initial OSS workstream, stage the
                    // interactive-layer packages in the same folder structure
                    // as the closed-source openhcl-deps.
                    match arch {
                        CommonArch::X86_64 => {
                            sh.set_var("OPENVMM_DEPS_X64", interactive_dep.parent().unwrap());
                        }
                        CommonArch::Aarch64 => {
                            sh.set_var("OPENVMM_DEPS_AARCH64", interactive_dep.parent().unwrap());
                        }
                    }

                    for (k, v) in extra_env.into_iter().flatten() {
                        sh.set_var(k, v);
                    }

                    // FIXME: update-rootfs.py invokes git to obtain a version
                    // hash to stuff into the initrd.
                    sh.change_dir(openvmm_repo_path);

                    let rootfs_config = rootfs_config
                        .iter()
                        .flat_map(|x| ["--rootfs-config".as_ref(), x.as_os_str()]);

                    xshell::cmd!(
                        sh,
                        "python3 openhcl/update-rootfs.py
                            {bin_openhcl}
                            {initrd_path}
                            --arch {rootfs_py_arch}
                            --package-root {kernel_package_root}
                            --kernel-modules {kernel_modules}
                            {rootfs_config...}
                            {initrd_contents...}
                        "
                    )
                    .run()?;

                    rt.write(
                        initrd,
                        &OpenhclInitrdOutput {
                            initrd: initrd_path,
                        },
                    );

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
