// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;
use flowey::pipeline::prelude::*;

// Set these two branches when backporting, then run `cargo xflowey regen`.
const MIRROR_BRANCH: &str = "main";
const SUBMODULE_TARGET: &str = "main";

pub(super) fn pipeline(backend: PipelineBackendHint) -> anyhow::Result<Pipeline> {
    anyhow::ensure!(matches!(backend, PipelineBackendHint::Ado), "ADO only");
    let mut pipeline = Pipeline::new();
    pipeline
        .ado_set_ci_triggers(AdoCiTriggers {
            branches: vec![MIRROR_BRANCH.into()],
            batch: true,
            ..Default::default()
        })
        .ado_set_flowey_bootstrap_template_fn(|platform, arch| {
            crate::pipelines_shared::ado_flowey_bootstrap_template::get_template_for_platform(
                platform,
                arch,
                &AdoResourcesRepositoryId::new_self(),
            )
        });
    pipeline
        .new_job(
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
            FlowArch::X86_64,
            "Queue mirror refresh",
        )
        .ado_override_job_id("QueueMirror")
        .ado_set_pool_with_demands("Azure Pipelines", vec![])
        .dep_on(|ctx| Request {
            done: ctx.new_done_handle(),
        })
        .finish();

    // Flowey doesn't model hosted vmImage or arbitrary ADO job conditions.
    pipeline.ado_post_process_yaml(|mut yaml| {
        let job = &mut yaml["jobs"][0];
        job["pool"] = serde_yaml::Value::Null;
        job["pool"]["vmImage"] = "ubuntu-latest".into();
        job["condition"] = format!(
            "and(succeeded(), in(variables['Build.Reason'], 'IndividualCI', 'BatchedCI'), eq(variables['Build.Repository.Name'], 'microsoft/openvmm'), eq(variables['Build.SourceBranch'], 'refs/heads/{MIRROR_BRANCH}'))"
        ).into();
        yaml
    });
    Ok(pipeline)
}

flowey_request! {
    pub struct Request {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "templateParameters": {
                "branchToMirror": MIRROR_BRANCH,
                "branchToUpdateSubmodule": SUBMODULE_TARGET,
                "updateSubmodule": true,
            },
        });
        ctx.emit_ado_step("Queue downstream mirror pipeline", |ctx| {
            request.done.claim(ctx);
            move |_| format!(r#"
- pwsh: |
    $ErrorActionPreference = 'Stop'
    $run = Invoke-RestMethod -Method Post `
        -Uri 'https://dev.azure.com/microsoft/HyperVCloud/_apis/pipelines/149443/runs?api-version=7.1' `
        -Headers @{{ Authorization = "Bearer $env:SYSTEM_ACCESSTOKEN" }} `
        -MaximumRedirection 0 -ContentType 'application/json' -Body $env:MIRROR_REQUEST
    Write-Host "Queued mirror refresh $($run.id)"
  env:
    MIRROR_REQUEST: >-
      {body}
    SYSTEM_ACCESSTOKEN: $(System.AccessToken)
"#)
        });
        Ok(())
    }
}
