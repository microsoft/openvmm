// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An amalgamated configuration node that streamlines the process of resolving
//! the most common subset of shared configuration requests required by OpenVMM
//! pipelines.

use flowey::node::prelude::*;

#[derive(Clone, Serialize, Deserialize)]
pub struct LocalOnlyParams {
    /// Prompt the user before certain interesting operations (e.g:
    /// installing packages from apt)
    pub interactive: bool,
    /// Automatically install any necessary system dependencies / tools.
    pub auto_install: bool,
    /// Ignore the Rust version requirement, and use whatever toolchain the user
    /// currently has installed.
    pub ignore_rust_version: bool,
}

flowey_request! {
    #[derive(Clone)]
    pub struct Params {
        pub local_only: Option<LocalOnlyParams>,
        pub verbose: ReadVar<bool>,
        pub locked: bool,
        pub deny_warnings: bool,
        pub no_incremental: bool,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_cargo_config_deny_warnings::Node>();
        ctx.import::<crate::install_git_credential_manager::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::install_vmm_tests_deps::Node>();
        ctx.import::<flowey_lib_common::cfg_cargo_common_flags::Node>();
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::resolve_protoc::Node>();
        ctx.import::<flowey_lib_common::git_checkout::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::install_dotnet_cli::Node>();
        ctx.import::<flowey_lib_common::install_azure_cli::Node>();
        ctx.import::<flowey_lib_common::install_git::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::nuget_install_package::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_run::Node>();
        ctx.import::<flowey_lib_common::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            local_only,
            verbose,
            locked,
            deny_warnings,
            no_incremental,
        } = request;

        if matches!(ctx.backend(), FlowBackend::Github) {
            if local_only.is_some() {
                anyhow::bail!("can only set `local_only` params when using Local backend");
            }

            ctx.config(flowey_lib_common::install_rust::Config {
                auto_install: Some(true),
                ignore_version: Some(false),
                ..Default::default()
            });
            let token = ctx.get_gh_context_var().global().token();
            ctx.config(flowey_lib_common::use_gh_cli::Config {
                auth: Some(flowey_lib_common::use_gh_cli::GhCliAuth::AuthToken(
                    ConfigVar(token),
                )),
            });
        } else if matches!(ctx.backend(), FlowBackend::Ado) {
            if local_only.is_some() {
                anyhow::bail!("can only set `local_only` params when using Local backend");
            }

            ctx.config(flowey_lib_common::install_rust::Config {
                auto_install: Some(true),
                ignore_version: Some(false),
                ..Default::default()
            });
        } else if matches!(ctx.backend(), FlowBackend::Local) {
            let local_only =
                local_only.ok_or(anyhow::anyhow!("missing essential request: local_only"))?;

            let LocalOnlyParams {
                interactive,
                auto_install,
                ignore_rust_version,
            } = local_only;

            // wire up `interactive`
            {
                ctx.config(flowey_lib_common::install_dist_pkg::Config {
                    interactive: Some(interactive),
                    ..Default::default()
                });
                ctx.config(flowey_lib_common::use_gh_cli::Config {
                    auth: Some(flowey_lib_common::use_gh_cli::GhCliAuth::LocalOnlyInteractive),
                });
                ctx.config(flowey_lib_common::install_rust::Config {
                    ignore_version: Some(ignore_rust_version),
                    ..Default::default()
                });
            }

            // wire up auto_install
            {
                ctx.config(flowey_lib_common::install_rust::Config {
                    auto_install: Some(auto_install),
                    ..Default::default()
                });
                ctx.config(flowey_lib_common::install_dist_pkg::Config {
                    skip_update: Some(!auto_install),
                    ..Default::default()
                });
                ctx.config(flowey_lib_common::install_nodejs::Config {
                    auto_install: Some(auto_install),
                    ..Default::default()
                });
                ctx.config(flowey_lib_common::install_azure_cli::Config {
                    auto_install: Some(auto_install),
                    ..Default::default()
                });
                ctx.config(flowey_lib_common::install_git::Config {
                    auto_install: Some(auto_install),
                });
                ctx.config(flowey_lib_common::install_dotnet_cli::Config {
                    auto_install: Some(auto_install),
                    ..Default::default()
                });
                ctx.config(crate::install_vmm_tests_deps::Config {
                    auto_install: Some(auto_install),
                    selections: None,
                });
            }

            // FUTURE: if we ever spin up a openvmm setup utility - it might be
            // interesting to distribute a flowey-based tool that also clones
            // the repo.
            ctx.config(flowey_lib_common::git_checkout::Config {
                require_local_clones: Some(true),
            });
        } else {
            anyhow::bail!("unsupported backend")
        }

        ctx.config(flowey_lib_common::cfg_cargo_common_flags::Config {
            locked: Some(locked),
            verbose: Some(ConfigVar(verbose)),
            no_incremental: Some(no_incremental),
        });

        ctx.config(crate::init_openvmm_cargo_config_deny_warnings::Config {
            deny_warnings: Some(deny_warnings),
        });

        Ok(())
    }
}
