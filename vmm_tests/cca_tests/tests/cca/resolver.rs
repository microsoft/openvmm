// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the CCA tests

use crate::artifacts;
use anyhow::Context;
use petri::ErasedArtifactHandle;
use petri_artifacts_vmm_test::artifacts::tmks;
use std::path::PathBuf;

/// An implementation of [`petri_artifacts_core::ResolveTestArtifact`]
/// that resolves artifacts to various "known paths" within the context of
/// the CCA tests in the OpenVMM repository.
pub struct OpenvmmCcaKnownPathsTestArtifactResolver;

impl OpenvmmCcaKnownPathsTestArtifactResolver {
    /// Creates a new resolver for a test with the given name.
    pub fn new() -> Self {
        Self {}
    }
}

impl petri_artifacts_core::ResolveTestArtifact for OpenvmmCcaKnownPathsTestArtifactResolver {
    #[rustfmt::skip]
    fn resolve(&self, id: ErasedArtifactHandle) -> anyhow::Result<PathBuf> {

        match id {

            _ if id == tmks::TMK_VMM_LINUX_AARCH64_MUSL =>
                env_path(OPENVMM_CCA_TMK_VMM_ENV_VAR, "TMK_VMM_LINUX_AARCH64_MUSL"),

            _ if id == tmks::SIMPLE_TMK_AARCH64 =>
                env_path(OPENVMM_CCA_SIMPLE_TMK_ENV_VAR, "SIMPLE_TMK_AARCH64"),

            _ if id == artifacts::SHRINKWRAP => cca_shrinkwrap_path(),
            _ if id == artifacts::VENV => cca_venv_path(),
            _ if id == artifacts::ROOTFS => cca_package_path("rootfs.ext2", "CCA emulation rootfs"),
            _ if id == artifacts::E2FSCK => cca_buildroot_host_sbin_path("e2fsck", "CCA buildroot host e2fsck"),
            _ if id == artifacts::RESIZE2FS => cca_buildroot_host_sbin_path("resize2fs", "CCA buildroot host resize2fs"),
            _ if id == artifacts::GUEST_DISK => cca_package_path("guest-disk.img", "CCA guest disk"),
            _ if id == artifacts::PLANE0_LINUX_IMAGE => cca_plane0_linux_image_path(),
            _ if id == artifacts::KVMTOOL_EFI => cca_package_path("KVMTOOL_EFI.fd", "CCA kvmtool EFI firmware"),
            _ if id == artifacts::LKVM => cca_package_path("lkvm", "CCA lkvm"),

            _ => anyhow::bail!("no support for given artifact type"),
        }
    }
}

const OPENVMM_CCA_TEST_ROOT_ENV_VAR: &str = "OPENVMM_CCA_TEST_ROOT";
const OPENVMM_CCA_TMK_VMM_ENV_VAR: &str = "OPENVMM_CCA_TMK_VMM";
const OPENVMM_CCA_SIMPLE_TMK_ENV_VAR: &str = "OPENVMM_CCA_SIMPLE_TMK";

fn get_path(base: PathBuf, file: &str, description: &str) -> anyhow::Result<PathBuf> {
    let path = base.join(file);
    if !path.exists() {
        anyhow::bail!(
            "{} not found at {}, try running `cargo xflowey cca-tests --install-emu`",
            description,
            path.display()
        )
    }
    Ok(path)
}

fn env_path(name: &str, description: &str) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("env var {name} not set for {description}"))?;
    if !path.exists() {
        anyhow::bail!("{} ({}) not found at {}", description, name, path.display())
    }
    Ok(path)
}

fn cca_test_root() -> PathBuf {
    std::env::var_os(OPENVMM_CCA_TEST_ROOT_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/cca-test"))
}

fn cca_home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

fn cca_shrinkwrap_path() -> anyhow::Result<PathBuf> {
    get_path(
        cca_test_root().join("shrinkwrap"),
        "shrinkwrap/shrinkwrap",
        "CCA shrinkwrap executable",
    )
}

fn cca_venv_path() -> anyhow::Result<PathBuf> {
    get_path(
        cca_test_root().join("shrinkwrap"),
        "venv",
        "CCA shrinkwrap virtual environment",
    )
}

fn cca_plane0_linux_image_path() -> anyhow::Result<PathBuf> {
    get_path(
        cca_test_root().join("plane0-linux/arch/arm64/boot"),
        "Image",
        "CCA Plane0 Linux image",
    )
}

fn cca_package_path(file_name: &'static str, description: &'static str) -> anyhow::Result<PathBuf> {
    get_path(
        cca_home_dir()?.join(".shrinkwrap/package/cca-3world"),
        file_name,
        description,
    )
}

fn cca_buildroot_host_sbin_path(
    file_name: &'static str,
    description: &'static str,
) -> anyhow::Result<PathBuf> {
    get_path(
        cca_home_dir()?.join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin"),
        file_name,
        description,
    )
}
