// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for getting kernel stats from the `mshv_vtl` driver.

use thiserror::Error;

/// Per-CPU VTL transition counts, exposed by `mshv_vtl` as a sysfs attribute on
/// the `mshv_vtl_low` misc device.
const VTL_TRANSITIONS_PATH: &str = "/sys/class/misc/mshv_vtl_low/mshv_vtl_transitions";

/// Error returned by [`vp_stats`].
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum VpStatsError {
    #[error("failed to read {VTL_TRANSITIONS_PATH}")]
    Read(#[source] std::io::Error),
    #[error("stats are not utf-8")]
    NotUtf8(#[source] std::str::Utf8Error),
    #[error("stats are missing the expected header line")]
    MissingHeader,
    #[error("failed to parse stats line")]
    ParseLine,
}

/// The per-VP stats from the kernel.
#[derive(Debug, Clone)]
pub struct HclVpStats {
    /// The number of VTL transitions.
    ///
    /// Note that the kernel only counts transitions that it hands back to
    /// userspace or handles via the generic intercept path. On TDX every exit
    /// is either handled in the kernel or returned early, so this never
    /// increments; on SNP, exits handled entirely in the kernel are not
    /// counted.
    pub vtl_transitions: u64,
}

/// Gets the per-VP stats from the kernel, indexed by Linux CPU number.
///
/// The kernel only reports online CPUs, so entries for offline CPUs (e.g. ones
/// still managed by sidecar) are `None`.
pub fn vp_stats() -> Result<Vec<Option<HclVpStats>>, VpStatsError> {
    let data = std::fs::read(VTL_TRANSITIONS_PATH).map_err(VpStatsError::Read)?;
    let data = std::str::from_utf8(&data).map_err(VpStatsError::NotUtf8)?;

    // The kernel caps sysfs output at one page and silently truncates the last
    // line, so only consider newline-terminated lines. This means CPUs past the
    // cutoff are missing entirely, which happens somewhere north of 200 CPUs.
    let complete = &data[..data.rfind('\n').map_or(0, |i| i + 1)];
    let mut lines = complete.lines();
    if !lines.next().is_some_and(|l| l.starts_with("cpu#")) {
        return Err(VpStatsError::MissingHeader);
    }
    let mut stats = Vec::new();
    for line in lines {
        let (cpu, rest) = line.split_once(' ').ok_or(VpStatsError::ParseLine)?;
        let n: usize = cpu
            .strip_prefix("cpu")
            .ok_or(VpStatsError::ParseLine)?
            .parse()
            .map_err(|_| VpStatsError::ParseLine)?;

        let vtl_transitions = rest
            .split(' ')
            .next()
            .ok_or(VpStatsError::ParseLine)?
            .parse()
            .map_err(|_| VpStatsError::ParseLine)?;

        if stats.len() <= n {
            stats.resize_with(n + 1, || None);
        }
        stats[n] = Some(HclVpStats { vtl_transitions });
    }
    Ok(stats)
}
