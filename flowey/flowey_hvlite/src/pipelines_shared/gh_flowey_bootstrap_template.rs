// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`get_template`]

/// Get our internal flowey bootstrap template.
///
/// See [`Pipeline::gh_set_flowey_bootstrap_template`]
///
/// [`Pipeline::gh_set_flowey_bootstrap_template`]:
///     flowey::pipeline::prelude::Pipeline::gh_set_flowey_bootstrap_template
pub fn get_template() -> String {
    let template = include_str!("gh_flowey_bootstrap_template.yml").to_string();

    template.replace(
        "{{RUSTUP_TOOLCHAIN}}",
        flowey_lib_hvlite::cfg_rustup_version::RUSTUP_TOOLCHAIN,
    )
}
