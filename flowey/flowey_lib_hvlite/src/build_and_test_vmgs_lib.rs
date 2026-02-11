// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and tests `vmgs_lib` library.
//!
//! Tests are windows only at the moment.

use crate::run_cargo_build::CargoBuildOutput;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum VmgsLibOutput {
    LinuxDynamicLib {
        #[serde(rename = "libvmgs_lib.so")]
        so: PathBuf,
    },
    WindowsDynamicLib {
        #[serde(rename = "vmgs_lib.dll")]
        dll: PathBuf,
        #[serde(rename = "vmgs_lib.dll.lib")]
        dll_lib: PathBuf,
        #[serde(rename = "vmgs_lib.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for VmgsLibOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub vmgs_lib: WriteVar<VmgsLibOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            vmgs_lib,
        } = request;

        let pre_build_deps =
            [
                ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec!["libssl-dev".into()],
                    done: v,
                }),
            ]
            .to_vec();

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "vmgs_lib".into(),
            out_name: "vmgs_lib".into(),
            crate_type: CargoCrateType::DynamicLib,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps,
            output: v,
        });

        let built_vmgs_lib = ctx.emit_minor_rust_stepv("check built vmgs_lib", |ctx| {
            let output = output.claim(ctx);
            move |rt| match rt.read(output) {
                CargoBuildOutput::LinuxDynamicLib { so } => VmgsLibOutput::LinuxDynamicLib { so },
                CargoBuildOutput::WindowsDynamicLib { dll, dll_lib, pdb } => {
                    VmgsLibOutput::WindowsDynamicLib { dll, dll_lib, pdb }
                }
                _ => unreachable!(),
            }
        });

        // given how simple the test is for vmgs_lib, it's fine to just
        // do it "inline" with the compilation
        //
        // if we ever decide to do more involved testing for this lib,
        // this should get split out into a separate step

        // TODO: figure out how to test vmgs_lib on other architectures.
        // Currently x86 only
        let did_test = if matches!(
            &target.as_triple().architecture,
            target_lexicon::Architecture::X86_64
        ) && matches!(ctx.arch(), FlowArch::X86_64)
        {
            let clang_installed =
                ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec!["clang".into()],
                    done: v,
                });

            let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

            if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                ctx.emit_rust_step("test vmgs_lib", |ctx| {
                    clang_installed.claim(ctx);

                    let built_vmgs_lib = built_vmgs_lib.clone().claim(ctx);
                    let openvmm_repo_path = openvmm_repo_path.claim(ctx);

                    move |rt| {
                        let VmgsLibOutput::LinuxDynamicLib { so } = rt.read(built_vmgs_lib) else {
                            unreachable!()
                        };

                        let so_dir = so.parent().unwrap();

                        let openvmm_repo_path = rt.read(openvmm_repo_path);

                        let vmgs_testlib_c =
                            openvmm_repo_path.join("vm/vmgs/vmgs_lib/examples/vmgs_testlib.c");

                        if which::which("clang").is_ok() {
                            flowey::shell_cmd!(
                                rt,
                                "clang {vmgs_testlib_c} {so} -rpath {so_dir} -o ./vmgs_lib_test"
                            )
                            .run()?;
                            flowey::shell_cmd!(rt, "./vmgs_lib_test").run()?;
                        } else {
                            log::warn!("skipping vmgs_lib test (could not find clang)");
                        }

                        Ok(())
                    }
                })
            } else if matches!(ctx.platform(), FlowPlatform::Windows) {
                // HACK: clang-on-bash-on-windows-on-ADO is... wild. This
                // works, but it's undoubtedly suboptimal, and someone who
                // actually _understands_ how clang is set up in this
                // context could do a wildly better job here.
                ctx.emit_rust_step("test vmgs_lib", |ctx| {
                    clang_installed.claim(ctx);

                    let built_vmgs_lib = built_vmgs_lib.clone().claim(ctx);
                    let openvmm_repo_path = openvmm_repo_path.claim(ctx);

                    move |rt| {
                        // TODO: figure out how to test cross-compile windows in wsl2
                        if flowey_lib_common::_util::running_in_wsl(rt)
                            && matches!(
                                &target.as_triple().operating_system,
                                target_lexicon::OperatingSystem::Windows
                            )
                        {
                            log::warn!("unimplemented: skip testing windows vmgs_lib via WSL2");
                            return Ok(());
                        }

                        let openvmm_repo_path = rt.read(openvmm_repo_path);

                        let VmgsLibOutput::WindowsDynamicLib {
                            dll,
                            dll_lib,
                            pdb: _,
                        } = rt.read(built_vmgs_lib)
                        else {
                            unreachable!()
                        };

                        if which::which("clang").is_err() {
                            log::warn!("skipping vmgs_lib test (could not find clang)");
                            return Ok(());
                        }

                        let vmgs_testlib_c =
                            openvmm_repo_path.join("vm/vmgs/vmgs_lib/examples/vmgs_testlib.c");

                        // make sure to copy the dll import lib over as well!
                        fs_err::copy(dll_lib, "vmgs_lib.dll.lib")?;
                        fs_err::copy(dll, "vmgs_lib.dll")?;

                        flowey::shell_cmd!(
                            rt,
                            "clang {vmgs_testlib_c} -o vmgs_lib_test.exe -l vmgs_lib.dll"
                        )
                        .run()?;
                        flowey::shell_cmd!(rt, "./vmgs_lib_test.exe").run()?;

                        Ok(())
                    }
                })
            } else {
                anyhow::bail!("unsupported platform")
            }
        } else {
            ReadVar::from_static(()).into_side_effect()
        };

        ctx.emit_minor_rust_step("report built vmgs_lib", |ctx| {
            did_test.claim(ctx);
            let built_vmgs_lib = built_vmgs_lib.claim(ctx);
            let vmgs_lib = vmgs_lib.claim(ctx);
            move |rt| {
                let built_vmgs_lib = rt.read(built_vmgs_lib);
                rt.write(vmgs_lib, &built_vmgs_lib);
            }
        });

        Ok(())
    }
}
