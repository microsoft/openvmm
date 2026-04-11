// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`HelloWorldCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;

/// A minimal "hello world" pipeline to validate that the
/// `azurelinux3-amd64-dom0` image on `openvmm-gh-intel-westus3` is reachable.
#[derive(clap::Args)]
pub struct HelloWorldCli {
    #[clap(flatten)]
    local_run_args: Option<crate::pipelines_shared::cfg_common_params::LocalRunArgs>,
}

impl IntoPipeline for HelloWorldCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self { local_run_args } = self;

        let mut pipeline = Pipeline::new();

        // Trigger on PRs to main
        pipeline
            .gh_set_pr_triggers(GhPrTriggers {
                branches: vec!["main".into()],
                ..GhPrTriggers::new_draftable()
            })
            .gh_set_name("Hello World Dom0");

        let openvmm_repo_source = match backend_hint {
            PipelineBackendHint::Local => {
                RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()))
            }
            PipelineBackendHint::Github => RepoSource::GithubSelf,
            PipelineBackendHint::Ado => anyhow::bail!("unsupported backend: ADO"),
        };

        if let RepoSource::GithubSelf = &openvmm_repo_source {
            pipeline.gh_set_flowey_bootstrap_template(
                crate::pipelines_shared::gh_flowey_bootstrap_template::get_template(),
            );
        }

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            local_run_args,
        )?;

        pipeline.inject_all_jobs_with(move |job| {
            job.dep_on(&cfg_common_params)
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
                .dep_on(
                    |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                        hvlite_repo_source: openvmm_repo_source.clone(),
                    },
                )
                .gh_grant_permissions::<flowey_lib_common::git_checkout::Node>([(
                    GhPermission::Contents,
                    GhPermissionValue::Read,
                )])
                .gh_grant_permissions::<flowey_lib_common::gh_task_azure_login::Node>([(
                    GhPermission::IdToken,
                    GhPermissionValue::Write,
                )])
        });

        // Single hello-world job on the dom0 pool
        pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::AzureLinux),
                FlowArch::X86_64,
                "hello world [azurelinux3-amd64-dom0]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_intel_dom0_1es())
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::hello_world::Params {
                done: ctx.new_done_handle(),
            })
            .finish();

        Ok(pipeline)
    }
}
