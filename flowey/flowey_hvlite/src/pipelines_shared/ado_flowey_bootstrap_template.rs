// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`get_template_ado`]

use flowey::node::prelude::AdoResourcesRepositoryId;

/// Get our internal flowey bootstrap template.
///
/// See [`Pipeline::ado_set_flowey_bootstrap_template`]
///
/// [`Pipeline::ado_set_flowey_bootstrap_template`]:
///     flowey::pipeline::prelude::Pipeline::ado_set_flowey_bootstrap_template
pub fn get_template_ado(ado_hvlite_repo_resource_id: &AdoResourcesRepositoryId) -> String {
    // to be clear: these replaces are totally custom to this particular
    // bootstrap template. flowey knows nothing of these replacements.
    include_str!("ado_flowey_bootstrap_template.yml")
        .to_string()
        // not actually used today, but will be used the moment pipelines live
        // in a repo that doesn't also contain all flowey source code as well.
        .replace(
            "{{FLOWEY_ADO_REPO_ID}}",
            ado_hvlite_repo_resource_id.dangerous_get_raw_id(),
        )
        .replace(
            "{{RUSTUP_TOOLCHAIN}}",
            flowey_lib_hvlite::_jobs::cfg_versions::RUSTUP_TOOLCHAIN,
        )
}
