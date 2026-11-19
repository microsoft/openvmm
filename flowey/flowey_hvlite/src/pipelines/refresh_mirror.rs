// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::pipeline::prelude::*;

pub(super) fn pipeline(backend: PipelineBackendHint) -> anyhow::Result<Pipeline> {
    anyhow::ensure!(matches!(backend, PipelineBackendHint::Ado), "ADO only");
    let yaml = serde_yaml::from_str(include_str!("refresh_mirror.yml"))?;
    let mut pipeline = Pipeline::new();
    pipeline.ado_post_process_yaml(move |_| yaml);
    Ok(pipeline)
}
