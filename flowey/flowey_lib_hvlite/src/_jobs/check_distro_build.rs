// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure `openvmm` still builds the way a Linux distribution package builds
//! it.
//!
//! This configuration does not use the repository's `.packages/`
//! provisioning. Every native dependency comes from a distribution package,
//! and the uploaded vendor archive is consumed exactly the way a packager would
//! consume it.
//!
//! The build runs against a `git archive` export of HEAD rather than the
//! checkout, both because that is the tree a packager gets from the tag source
//! archive and because the checkout may be a developer's working tree.

use crate::assemble_openvmm_vendor_release::{
    CARGO_CONFIG_FILE, VendorReleaseOutput, read_vendor_identity, resolve_identity,
};
use flowey::node::prelude::*;
use std::io::Write;

fn append_vendor_config(config_path: &Path, vendor_config_path: &Path) -> anyhow::Result<()> {
    let existing = fs_err::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let existing = existing
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    if existing.get("source").is_some() {
        anyhow::bail!(
            "{} already defines [source]; refusing to overwrite existing source configuration",
            config_path.display()
        );
    }

    let vendor_config = fs_err::read(vendor_config_path)
        .with_context(|| format!("failed to read {}", vendor_config_path.display()))?;
    let mut config = fs_err::OpenOptions::new()
        .append(true)
        .open(config_path)
        .with_context(|| format!("failed to open {}", config_path.display()))?;
    config.write_all(b"\n")?;
    config.write_all(&vendor_config)?;
    Ok(())
}

flowey_request! {
    pub struct Request {
        pub release: ReadVar<VendorReleaseOutput>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release, done } = request;

        let target = target_lexicon::triple!("x86_64-unknown-linux-gnu");
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let rust_toolchain = ctx.reqv(flowey_lib_common::install_rust::Request::GetRustupToolchain);

        // Do not depend on `install_openvmm_rust_build_essential`: it provisions
        // `protoc` out of `.packages/`, which is what this job exists to avoid.
        let mut deps = vec![ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled)];

        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            deps.push(
                ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![
                        "build-essential".into(),
                        "linux-libc-dev".into(),
                        "libssl-dev".into(),
                        "pkg-config".into(),
                        "protobuf-compiler".into(),
                    ],
                    done: v,
                }),
            );
        }

        ctx.req(flowey_lib_common::install_rust::Request::InstallTargetTriple(target.clone()));

        ctx.emit_rust_step("build openvmm in a distribution configuration", |ctx| {
            done.claim(ctx);
            deps.claim(ctx);
            let release = release.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let rust_toolchain = rust_toolchain.claim(ctx);
            move |rt| {
                let release = rt.read(release);
                let openvmm_repo_path = rt.read(openvmm_repo_path);
                let rust_toolchain = rt.read(rust_toolchain);
                let identity = read_vendor_identity(&release.assets)?;

                rt.sh.change_dir(&openvmm_repo_path);
                let checkout_identity = resolve_identity(rt)?;
                if checkout_identity != identity {
                    anyhow::bail!(
                        "vendor archive identity {:?} does not match checkout {:?}",
                        identity,
                        checkout_identity
                    );
                }

                let archive = release.assets.join(identity.archive_name());

                let build_root = std::env::current_dir()?;

                // Build an export of the commit rather than the checkout
                // itself. This is the tree a packager actually gets from the
                // tag source archive, and on the local backend the checkout is
                // the developer's working tree, which this job would otherwise
                // fill with a vendored crate tree and a modified tracked
                // `.cargo/config.toml`.
                let source_dir = build_root.join("distro-build-source");
                if source_dir.exists() {
                    fs_err::remove_dir_all(&source_dir)?;
                }
                fs_err::create_dir_all(&source_dir)?;

                let source_tar = build_root.join("distro-build-source.tar");
                rt.sh.change_dir(&openvmm_repo_path);
                flowey::shell_cmd!(rt, "git archive --format=tar -o {source_tar} HEAD").run()?;
                flowey::shell_cmd!(rt, "tar -xf {source_tar} -C {source_dir}").run()?;
                fs_err::remove_file(&source_tar)?;

                flowey::shell_cmd!(rt, "tar -xzf {archive} -C {source_dir}").run()?;

                let vendor_dir = source_dir.join("vendor");
                let cargo_config = source_dir.join(CARGO_CONFIG_FILE);

                if !vendor_dir.is_dir() {
                    anyhow::bail!("vendor archive did not extract {}", vendor_dir.display());
                }

                if !cargo_config.is_file() {
                    anyhow::bail!("vendor archive did not extract {}", cargo_config.display());
                }

                let cargo_config_toml = source_dir.join(".cargo").join("config.toml");
                append_vendor_config(&cargo_config_toml, &cargo_config)?;

                let cargo_home = build_root.join("distro-cargo-home");
                if cargo_home.exists() {
                    fs_err::remove_dir_all(&cargo_home)?;
                }
                fs_err::create_dir_all(&cargo_home)?;

                let cargo_target_dir = build_root.join("distro-cargo-target");
                if cargo_target_dir.exists() {
                    fs_err::remove_dir_all(&cargo_target_dir)?;
                }
                fs_err::create_dir_all(&cargo_target_dir)?;

                // `.cargo/config.toml` does not force its `PROTOC` value, so an
                // inherited value redirects the build to the system compiler.
                let protoc = which::which("protoc")
                    .context("could not find the distribution-provided protoc")?;

                let target = target.to_string();
                let cargo = if let Some(rust_toolchain) = &rust_toolchain {
                    flowey::shell_cmd!(rt, "rustup run {rust_toolchain} cargo")
                } else {
                    flowey::shell_cmd!(rt, "cargo")
                };

                rt.sh.change_dir(&source_dir);
                cargo
                    .args([
                        "build",
                        "--release",
                        "--locked",
                        "--offline",
                        "-p",
                        "openvmm",
                        "--target",
                        &target,
                    ])
                    .env("PROTOC", protoc)
                    .env("OPENSSL_NO_VENDOR", "1")
                    .env("CARGO_HOME", cargo_home)
                    .env("CARGO_TARGET_DIR", cargo_target_dir)
                    // Debug info is not needed for this validation artifact and
                    // is the binding constraint on runner disk.
                    .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
                    .env("CARGO_INCREMENTAL", "0")
                    .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_vendor_config_without_overwriting_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let vendor_config = dir.path().join("cargo_config");
        let existing = "[build]\ntarget-dir = \"target\"\n";
        let replacement = "[source.crates-io]\nreplace-with = \"vendored-sources\"\n";

        fs_err::write(&config, existing).unwrap();
        fs_err::write(&vendor_config, replacement).unwrap();
        append_vendor_config(&config, &vendor_config).unwrap();

        assert_eq!(
            fs_err::read_to_string(config).unwrap(),
            format!("{existing}\n{replacement}")
        );
    }

    #[test]
    fn rejects_existing_source_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let vendor_config = dir.path().join("cargo_config");

        fs_err::write(&config, "[source.crates-io]\nreplace-with = \"other\"\n").unwrap();
        fs_err::write(
            &vendor_config,
            "[source.vendored-sources]\ndirectory = \"vendor\"\n",
        )
        .unwrap();

        assert!(append_vendor_config(&config, &vendor_config).is_err());
    }
}
