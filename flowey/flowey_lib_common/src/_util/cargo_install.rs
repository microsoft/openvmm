// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cache::CacheHit;
use flowey::node::prelude::*;

pub fn install_cached_cargo_binary(
    rt: &RustRuntimeServices<'_>,
    cache_dir: PathBuf,
    cache_hit: CacheHit,
    install_root: Option<PathBuf>,
    rust_toolchain: Option<String>,
    cargo_home: PathBuf,
    package: &str,
    version: &str,
    binary: &str,
) -> anyhow::Result<()> {
    let cached_binary = cache_dir.join(binary);

    let binary_path = if matches!(cache_hit, CacheHit::Hit) {
        anyhow::ensure!(
            cached_binary.is_file(),
            "cache entry for {package} is missing {}",
            cached_binary.display()
        );
        cached_binary
    } else {
        let install_root = install_root.unwrap_or_else(|| PathBuf::from("."));
        let run = |offline| {
            let rust_toolchain = rust_toolchain.as_ref().map(|s| format!("+{s}"));

            flowey::shell_cmd!(
                rt,
                "cargo {rust_toolchain...}
                    install
                    --locked
                    {offline...}
                    --root {install_root}
                    --target-dir {install_root}
                    --version {version}
                    {package}
                "
            )
            .run()
        };

        if run(Some("--offline")).is_err() {
            run(None)?;
        }

        let installed_binary = install_root.absolute()?.join("bin").join(binary);
        fs_err::rename(installed_binary, &cached_binary)?;
        cached_binary.absolute()?
    };

    fs_err::copy(binary_path, cargo_home.join("bin").join(binary))?;

    Ok(())
}
