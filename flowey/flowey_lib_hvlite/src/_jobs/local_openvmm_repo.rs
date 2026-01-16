// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;
use std::path::PathBuf;

// use container directory as default when running in a container, 
// otherwise look for repo root in parent directories, or fall back to compile-time source tree path
const OPENVMM_TMK_REPO_PATH: &str = "/workspaces/openvmm";

fn is_openvmm_repo_root(path: &Path) -> bool {
    path.join("Cargo.toml").is_file()
        && path.join("flowey").is_dir()
        && path.join("openhcl").is_dir()
        && path.join("openvmm").is_dir()
}

pub fn get_openvmm_tmk_repo() -> anyhow::Result<PathBuf> {
    // 1. Allow an explicit override
    if let Ok(env_path) = std::env::var("OPENVMM_TMK_REPO_PATH") {
        return Ok(PathBuf::from(env_path));
    }

    // 2. Prefer the standard dev-container checkout path
    if Path::new("/.dockerenv").exists() || Path::new(OPENVMM_TMK_REPO_PATH).exists() {
        return Ok(PathBuf::from(OPENVMM_TMK_REPO_PATH));
    }

    // 3. Outside containers, walk up from the current directory to locate repo root
    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(repo_root) = current_dir
            .ancestors()
            .find(|path| is_openvmm_repo_root(path))
        {
            return Ok(repo_root.to_path_buf());
        }
    }

    // 4. Fallback to the source-tree location where this crate was compiled
    if let Some(repo_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| is_openvmm_repo_root(path))
    {
        return Ok(repo_root.to_path_buf());
    }

    anyhow::bail!(
        "unable to determine OpenVMM repo path; set OPENVMM_TMK_REPO_PATH explicitly"
    )
}