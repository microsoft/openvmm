// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use flowey_core::pipeline::IntoPipeline;
use std::path::Path;

mod cli;
mod flow_resolver;
mod pipeline_resolver;
mod var_db;

/// Entrypoint into generic flowey infrastructure.
pub fn flowey_main<ProjectPipelines: clap::Subcommand + IntoPipeline>(
    flowey_crate: &str,
    repo_root: &Path,
) -> ! {
    if let Err(e) = cli::cli_main::<ProjectPipelines>(flowey_crate, repo_root) {
        log::error!("Error: {:#}", e);
        std::process::exit(-1);
    } else {
        std::process::exit(0)
    }
}

/// Check if we're running inside WSL (Windows Subsystem for Linux).
pub fn running_in_wsl() -> bool {
    let Ok(output) = std::process::Command::new("wslpath")
        .args(["-aw", "/"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).starts_with(r"\\wsl.localhost")
}

/// Check if a path is on a Windows-accessible filesystem in WSL (DrvFs mount).
///
/// DrvFs mounts are Windows drives mounted into WSL, which use the 9p filesystem
/// with `aname=drvfs` in the mount options. This function checks `/proc/mounts`
/// to find all DrvFs mount points and determines if the given path is under
/// one of them.
///
/// This handles:
/// - Default automount paths (e.g., /mnt/c/, /mnt/d/)
/// - Custom automount roots configured via wsl.conf
/// - Manually mounted Windows drives via fstab or mount command
///
/// Returns `false` if not running in WSL or if the check fails.
pub fn is_wsl_windows_path(path: &Path) -> bool {
    let path = match std::path::absolute(path) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Parse /proc/mounts to find DrvFs mounts
    let mounts = match std::fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(_) => return false,
    };

    let drvfs_mount_points: Vec<String> = mounts
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let mount_point = parts[1];
                let fs_type = parts[2];
                let mount_options = parts[3];
                // DrvFs mounts use 9p filesystem with aname=drvfs in the options
                if fs_type == "9p" && mount_options.contains("aname=drvfs") {
                    return Some(mount_point.to_string());
                }
            }
            None
        })
        .collect();

    let path_str = path.to_string_lossy();

    // Check if the path is under any DrvFs mount point
    for mount_point in &drvfs_mount_points {
        let mount_point_normalized = mount_point.trim_end_matches('/');
        if path_str == mount_point_normalized
            || path_str.starts_with(&format!("{}/", mount_point_normalized))
        {
            return true;
        }
    }

    false
}
