// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writes and checks the host CPU fingerprint for `--cpu-fingerprint`.

use anyhow::Context;
use cpu_profile::ProfileError;
use cpu_profile::ProfileErrorCode;
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
/// of new generations can still be fingerprinted. A backend that can
/// configure a probe partition from the profile, as WHP can, is checked for
/// `E_CPU_UNLISTED` on that partition, as a cold boot checks its own.
pub(crate) fn write(path: &Path, hypervisor: Option<&str>) -> anyhow::Result<()> {
    let backend = openvmm_helpers::hypervisor::fingerprint_backend(hypervisor)
        .context("failed to fingerprint the hypervisor backend")?;
    let backend_fingerprint = backend
        .cpu_fingerprint()
        .context("failed to fingerprint the hypervisor backend")?;
    let host = HostIdentity::collect().context("failed to identify the host")?;
    let fingerprint = CpuFingerprint::new(
        ToolIdentity {
            name: "openvmm".to_owned(),
            version: openvmm_build_info::get().version().to_owned(),
        },
        host,
        backend_fingerprint,
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

    let check = cpu_profile::check_fingerprint_with(&fingerprint, |profile| {
        backend.profile_unlisted_cpuid(profile).map_err(|err| {
            ProfileError::new(
                ProfileErrorCode::ProfileUnsupported,
                format!(
                    "cannot configure a probe partition from CPU profile {}: {err:#}",
                    profile.id()
                ),
            )
        })
    });
    eprintln!("{}", check.summary_line(&fingerprint));
    Ok(check.result?)
}
