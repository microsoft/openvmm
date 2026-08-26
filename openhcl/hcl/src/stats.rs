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
    #[error("failed to parse stats line")]
    ParseLine,
}

/// The per-VP stats from the kernel.
#[derive(Debug, Default, Clone)]
pub struct HclVpStats {
    /// The number of VTL transitions.
    pub vtl_transitions: u64,
}

/// Gets the per-VP stats from the kernel, indexed by Linux CPU number.
pub fn vp_stats() -> Result<Vec<HclVpStats>, VpStatsError> {
    let data = std::fs::read(VTL_TRANSITIONS_PATH).map_err(VpStatsError::Read)?;
    let data = std::str::from_utf8(&data).map_err(VpStatsError::NotUtf8)?;

    // The kernel caps sysfs output at one page and silently truncates the last
    // line, so only consider newline-terminated lines. This means CPUs past the
    // cutoff are missing entirely, which happens somewhere north of 200 CPUs.
    let complete = &data[..data.rfind('\n').map_or(0, |i| i + 1)];
    let mut stats = Vec::new();
    // Skip the header line.
    for line in complete.lines().skip(1) {
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
            stats.resize(n + 1, HclVpStats::default());
        }
        stats[n] = HclVpStats { vtl_transitions };
    }
    Ok(stats)
}
