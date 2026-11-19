// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::pipeline::prelude::*;
use std::collections::BTreeMap;

#[derive(clap::Args)]
pub struct RefreshMirrorCli;

impl IntoPipeline for RefreshMirrorCli {
    fn into_pipeline(self, backend: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        anyhow::ensure!(matches!(backend, PipelineBackendHint::Ado), "ADO only");

        let routes = BTreeMap::from([
            ("main", "main"),
            ("release/2411", "staging/1.5.2411"),
            ("release/2505", "staging/1.6.2505"),
            ("release/1.7.2511", "staging/1.7.2511"),
            ("release/1.8.2607", "staging/1.8.2607"),
        ]);
        let mut yaml: serde_yaml::Value = serde_yaml::from_str(include_str!("refresh_mirror.yml"))?;
        yaml["trigger"]["branches"]["include"] =
            serde_yaml::to_value(routes.keys().collect::<Vec<_>>())?;
        yaml["jobs"][0]["steps"][1]["env"]["MIRROR_ROUTES"] =
            serde_json::to_string(&routes)?.into();
        let mut pipeline = Pipeline::new();
        pipeline.ado_post_process_yaml(move |_| yaml);
        Ok(pipeline)
    }
}
