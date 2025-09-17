// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`get_template`]

/// Get our internal copilot setup steps template.
///
/// This template provides the steps needed to set up the OpenVMM development
/// environment for GitHub Copilot's Coding Agent.
pub fn get_template() -> String {
    let template = include_str!("copilot_setup_steps_template.yml").to_string();

    template
        .replace(
            "{{RUSTUP_TOOLCHAIN}}",
            flowey_lib_hvlite::_jobs::cfg_versions::RUSTUP_TOOLCHAIN,
        )
        .replace(
            "{{NEXTEST_VERSION}}",
            flowey_lib_hvlite::_jobs::cfg_versions::NEXTEST,
        )
}
