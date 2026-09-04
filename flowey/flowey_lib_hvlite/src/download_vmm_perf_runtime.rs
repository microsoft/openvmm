// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the pinned VMM.Perf runtime package.

use crate::common::CommonArch;
use anyhow::Context as _;
use flowey::node::prelude::*;
use sha2::Digest as _;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

// Update the version and all hashes together when refreshing the archives
// published to the public VMM.Perf runtime source below.
const VMM_PERF_RUNTIME_VERSION: &str = "20260903.6";
const VMM_PERF_RUNTIME_LINUX_X64_SHA256: &str =
    "a29832e839a62aa7e7f0450a79c0acb763781b24d604b19a2b2a8591f62afbe0";
const VMM_PERF_RUNTIME_LINUX_ARM64_SHA256: &str =
    "852e992531463dc95d2a8493517f598bb485873d4944c2f14fda111f6b1ed0f3";
const VMM_PERF_RUNTIME_WINDOWS_X64_SHA256: &str =
    "f9b402af95d9db90c6b4a5e979fa4b06e9f2142748be8f9380b17aa8fae35ffd";

flowey_request! {
    pub enum Request {
        Get {
            arch: CommonArch,
            runtime_archive: WriteVar<PathBuf>,
        }
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut requests_by_arch = BTreeMap::<_, Vec<_>>::new();
        for Request::Get {
            arch,
            runtime_archive,
        } in requests
        {
            requests_by_arch
                .entry(arch)
                .or_default()
                .push(runtime_archive);
        }

        if requests_by_arch.is_empty() {
            return Ok(());
        }

        let azcopy = ctx.reqv(flowey_lib_common::download_azcopy::Request::GetAzCopy);
        let persistent_dir = ctx.persistent_dir();
        let platform = ctx.platform();

        for (arch, outputs) in requests_by_arch {
            let (filename, expected_sha256) = runtime_archive_info(platform, arch)?;
            let url = format!(
                "https://vmmperfartifactpublic.blob.core.windows.net/perfpackage/{VMM_PERF_RUNTIME_VERSION}/{filename}"
            );

            ctx.emit_rust_step(format!("download VMM.Perf runtime ({filename})"), |ctx| {
                let azcopy = azcopy.clone().claim(ctx);
                let persistent_dir = persistent_dir.clone().claim(ctx);
                let outputs = outputs.claim(ctx);
                move |rt| {
                    let cache_dir = if let Some(dir) = persistent_dir {
                        rt.read(dir)
                    } else {
                        rt.sh.current_dir()
                    }
                    .join("vmm-perf")
                    .join(VMM_PERF_RUNTIME_VERSION);
                    fs_err::create_dir_all(&cache_dir)?;
                    let archive = cache_dir.join(filename);
                    let azcopy = rt.read(azcopy);

                    if archive.exists()
                        && let Err(err) = verify_sha256(&archive, expected_sha256)
                    {
                        log::warn!(
                            "discarding invalid cached VMM.Perf runtime {}: {err:#}",
                            archive.display()
                        );
                        fs_err::remove_file(&archive).with_context(|| {
                            format!(
                                "failed to remove invalid cached VMM.Perf runtime {}",
                                archive.display()
                            )
                        })?;
                    }

                    if !archive.exists() {
                        flowey::shell_cmd!(
                            rt,
                            "{azcopy} copy
                                    {url}
                                    {archive}
                                    --overwrite ifSourceNewer
                                    --skip-version-check"
                        )
                        .run()?;
                    }

                    verify_sha256(&archive, expected_sha256).or_else(|err| {
                        fs_err::remove_file(&archive).with_context(|| {
                            format!(
                                "failed to remove VMM.Perf runtime with an invalid checksum: {}",
                                archive.display()
                            )
                        })?;
                        Err(err)
                    })?;

                    for output in outputs {
                        rt.write(output, &archive.absolute()?);
                    }
                    Ok(())
                }
            });
        }

        Ok(())
    }
}

fn runtime_archive_info(
    platform: FlowPlatform,
    arch: CommonArch,
) -> anyhow::Result<(&'static str, &'static str)> {
    match (platform, arch) {
        (FlowPlatform::Linux(_), CommonArch::X86_64) => Ok((
            "vmm-perf-linux-x64.tar.gz",
            VMM_PERF_RUNTIME_LINUX_X64_SHA256,
        )),
        (FlowPlatform::Linux(_), CommonArch::Aarch64) => Ok((
            "vmm-perf-linux-arm64.tar.gz",
            VMM_PERF_RUNTIME_LINUX_ARM64_SHA256,
        )),
        (FlowPlatform::Windows, CommonArch::X86_64) => {
            Ok(("vmm-perf-win-x64.zip", VMM_PERF_RUNTIME_WINDOWS_X64_SHA256))
        }
        _ => anyhow::bail!("no VMM.Perf runtime archive for {arch:?} on {platform:?}"),
    }
}

fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
    let mut file = fs_err::File::open(path)
        .with_context(|| format!("failed to open VMM.Perf runtime {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read VMM.Perf runtime {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    anyhow::ensure!(
        actual == expected,
        "VMM.Perf runtime SHA-256 mismatch for {}: expected {expected}, found {actual}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_sha256;

    #[test]
    fn verifies_runtime_sha256() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let archive = scratch.path().join("runtime.tar.gz");
        std::fs::write(&archive, [])?;

        verify_sha256(
            &archive,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )?;
        let error = verify_sha256(
            &archive,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256 mismatch"));
        Ok(())
    }
}
