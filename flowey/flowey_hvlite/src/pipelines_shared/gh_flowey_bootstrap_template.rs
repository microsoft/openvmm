// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`get_template_for_platform`]

use flowey::pipeline::prelude::FlowArch;
use flowey::pipeline::prelude::FlowPlatform;

const COMMON_TEMPLATE: &str = include_str!("gh_flowey_bootstrap_template.yml");
const LINUX_TEMPLATE: &str = include_str!("gh_flowey_bootstrap_template_linux.yml");
const WINDOWS_TEMPLATE: &str = include_str!("gh_flowey_bootstrap_template_windows.yml");

/// Get a bootstrap template with only the installer matching the job's platform
/// and architecture.
///
/// Use with [`Pipeline::gh_set_flowey_bootstrap_template_fn`].
///
/// [`Pipeline::gh_set_flowey_bootstrap_template_fn`]:
///     flowey::pipeline::prelude::Pipeline::gh_set_flowey_bootstrap_template_fn
pub fn get_template_for_platform(platform: FlowPlatform, arch: FlowArch) -> anyhow::Result<String> {
    let installer = match (platform, arch) {
        (FlowPlatform::Windows, FlowArch::X86_64) => windows_template("x86_64", "X64"),
        (FlowPlatform::Windows, FlowArch::Aarch64) => windows_template("aarch64", "ARM64"),
        (FlowPlatform::Linux(_), FlowArch::X86_64 | FlowArch::Aarch64) => LINUX_TEMPLATE.into(),
        (platform, arch) => {
            anyhow::bail!("unsupported bootstrap platform {platform} / arch {arch}")
        }
    };

    Ok(with_toolchain(format!("{installer}\n{COMMON_TEMPLATE}")))
}

fn windows_template(host_arch: &str, runner_arch: &str) -> String {
    WINDOWS_TEMPLATE
        .replace("{{RUSTUP_HOST_ARCH}}", host_arch)
        .replace("{{RUNNER_ARCH}}", runner_arch)
}

fn with_toolchain(template: String) -> String {
    template.replace(
        "{{RUSTUP_TOOLCHAIN}}",
        flowey_lib_hvlite::cfg_rustup_version::RUSTUP_TOOLCHAIN,
    )
}
