// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writes and checks the host CPU fingerprint for `--cpu-fingerprint`.

use anyhow::Context;
use cpu_profile::fingerprint::CpuFingerprint;
use cpu_profile::fingerprint::ToolIdentity;
use cpu_profile::host::HostIdentity;
use std::io::Write;
use std::path::Path;

/// Fingerprints this host for the backend selected by `hypervisor` (or the
/// first available one), writes the fingerprint to `path`, or to stdout for
/// `-`, and checks it against the CPU profile of the host's generation.
///
/// The check prints one `NVX-CPU-PROFILE:` line to stderr and fails with its
/// code (`E_PROFILE_HOST_UNKNOWN`, `E_PROFILE_UNSUPPORTED`, or, for MSHV and
/// WHP, `E_CPU_UNLISTED`) after the fingerprint is written, so hosts
/// of new generations can still be fingerprinted.
pub(crate) fn write(path: &Path, hypervisor: Option<&str>) -> anyhow::Result<()> {
    let backend = openvmm_helpers::hypervisor::cpu_fingerprint(hypervisor)
        .context("failed to fingerprint the hypervisor backend")?;
    let host = HostIdentity::collect().context("failed to identify the host")?;
    let fingerprint = CpuFingerprint::new(
        ToolIdentity {
            name: "openvmm".to_owned(),
            version: openvmm_build_info::get().version().to_owned(),
        },
        host,
        backend,
    );
    let json = fingerprint.to_json();
    if path == Path::new("-") {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(json.as_bytes())
            .and_then(|()| stdout.flush())
            .context("failed to write the CPU fingerprint to stdout")?;
    } else {
        fs_err::write(path, json).context("failed to write the CPU fingerprint")?;
    }
    tracing::info!(
        backend = fingerprint.backend.name,
        surface_digest = fingerprint.surface_digest,
        digest = fingerprint.digest,
        "wrote CPU fingerprint"
    );

    let check = cpu_profile::check_fingerprint(&fingerprint);
    eprintln!("{}", check.summary_line(&fingerprint));
    Ok(check.result?)
}
