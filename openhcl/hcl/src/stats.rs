// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for getting kernel stats from /proc/mshv.

use std::io::BufRead;
use std::io::BufReader;
use thiserror::Error;

/// Error returned by [`vp_stats`].
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum VpStatsError {
    #[error("failed to read /proc/mshv")]
    Read(#[source] std::io::Error),
    #[error("failed to parse stats string")]
    ParseString(#[source] std::io::Error),
    #[error("failed to parse stats line")]
    ParseLine,
    #[error("a cpu was missing from the stats file")]
    MissingCpu,
}

/// The per-VP stats from the kernel.
#[derive(Debug)]
pub struct HclVpStats {
    /// The number of VTL transitions.
    pub vtl_transitions: u64,
}

/// Gets the per-VP stats from the kernel.
pub fn vp_stats() -> Result<Vec<HclVpStats>, VpStatsError> {
    let v = std::fs::read("/proc/mshv").map_err(VpStatsError::Read)?;
    // Skip the first two lines (version and header).
    let lines = BufReader::new(v.as_slice()).lines().skip(2);
    let mut stats = Vec::new();
    for line in lines {
        let line = line.map_err(VpStatsError::ParseString)?;
        let (cpu, rest) = line.split_once(' ').ok_or(VpStatsError::ParseLine)?;
        let n: usize = cpu
            .strip_prefix("cpu")
            .ok_or(VpStatsError::ParseLine)?
            .parse()
            .map_err(|_| VpStatsError::ParseLine)?;

        if n != stats.len() {
            return Err(VpStatsError::MissingCpu);
        }
        let vtl_transitions = rest
            .split(' ')
            .next()
            .ok_or(VpStatsError::ParseLine)?
            .parse()
            .map_err(|_| VpStatsError::ParseLine)?;

        stats.push(HclVpStats { vtl_transitions })
    }
    Ok(stats)
}
