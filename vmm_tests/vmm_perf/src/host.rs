// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context as _;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
pub(crate) const MIN_WORK_DIR_BYTES: u64 = 30 * GIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostCapacity {
    pub(crate) logical_processors: usize,
    pub(crate) available_memory_bytes: u64,
}

pub(crate) struct HostEnvironment {
    logical_processors: usize,
    owner: Option<(String, String)>,
}

impl HostEnvironment {
    pub(crate) fn detect() -> anyhow::Result<Self> {
        let owner = owner_for_restore()?;
        validate_host_environment(owner.is_some())?;
        Ok(Self {
            logical_processors: logical_processor_count()?,
            owner,
        })
    }

    pub(crate) fn default_hypervisor_backend(&self) -> anyhow::Result<&'static str> {
        native_hypervisor_backend()
    }

    pub(crate) fn capacity(&self) -> anyhow::Result<HostCapacity> {
        Ok(HostCapacity {
            logical_processors: self.logical_processors,
            available_memory_bytes: available_memory_bytes()?,
        })
    }

    pub(crate) fn validate_parameters(
        &self,
        parameters: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        validate_requested_capacity(parameters, self.capacity()?)
    }

    pub(crate) fn validate_work_dir(&self, work_dir: &Path) -> anyhow::Result<()> {
        validate_work_dir_capacity(
            work_dir,
            available_disk_bytes(work_dir)?,
            MIN_WORK_DIR_BYTES,
        )
    }

    pub(crate) fn restore_ownership(&self, paths: &[&Path]) -> anyhow::Result<()> {
        let Some((uid, gid)) = &self.owner else {
            return Ok(());
        };

        #[cfg(target_os = "linux")]
        {
            let status = Command::new("sudo")
                .args(["-n", "chown", "-R", "--"])
                .arg(format!("{uid}:{gid}"))
                .args(paths)
                .status()
                .context("failed to launch ownership restoration for VMM.Perf files")?;
            anyhow::ensure!(
                status.success(),
                "failed to restore VMM.Perf file ownership to {uid}:{gid}"
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (uid, gid, paths);
        Ok(())
    }
}

pub(crate) fn platform_command(program: &Path, env: &[(&str, &Path)]) -> anyhow::Result<Command> {
    #[cfg(target_os = "linux")]
    {
        if running_as_root()? {
            let mut command = Command::new(program);
            command.envs(env.iter().map(|(name, value)| (*name, *value)));
            return Ok(command);
        }

        let mut command = Command::new("sudo");
        command.args(["-n", "env"]);
        for (name, value) in env {
            command.arg(format!("{name}={}", value.display()));
        }
        command.arg(program);
        Ok(command)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut command = Command::new(program);
        command.envs(env.iter().map(|(name, value)| (*name, *value)));
        Ok(command)
    }
}

fn logical_processor_count() -> anyhow::Result<usize> {
    Ok(std::thread::available_parallelism()
        .context("failed to determine available host processor count")?
        .get())
}

#[cfg(target_os = "linux")]
fn available_memory_bytes() -> anyhow::Result<u64> {
    let meminfo =
        fs_err::read_to_string("/proc/meminfo").context("failed to read /proc/meminfo")?;
    let available_kib = meminfo
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")
                .and_then(|value| value.trim().strip_suffix("kB"))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .context("MemAvailable was missing or invalid in /proc/meminfo")?;
    available_kib
        .checked_mul(1024)
        .context("available host memory size overflowed")
}

#[cfg(target_os = "windows")]
fn available_memory_bytes() -> anyhow::Result<u64> {
    let available = sysinfo::System::new_all().available_memory();
    anyhow::ensure!(
        available > 0,
        "failed to determine available Windows host memory"
    );
    Ok(available)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn available_memory_bytes() -> anyhow::Result<u64> {
    anyhow::bail!("VMM.Perf supports Linux and Windows hosts only")
}

pub(crate) fn validate_requested_capacity(
    parameters: &BTreeMap<String, String>,
    capacity: HostCapacity,
) -> anyhow::Result<()> {
    if let Some(cpu_count) = parameters.get("CpuCount") {
        let cpu_count: usize = cpu_count
            .parse()
            .context("VMM.Perf CpuCount must be a positive integer")?;
        anyhow::ensure!(cpu_count > 0, "VMM.Perf CpuCount must be greater than zero");
        anyhow::ensure!(
            cpu_count <= capacity.logical_processors,
            "VMM.Perf requested {cpu_count} CPUs, but the host has only {} available logical processors",
            capacity.logical_processors
        );
    }

    if let Some(memory_mb) = parameters.get("MemoryMB") {
        let memory_mb: u64 = memory_mb
            .parse()
            .context("VMM.Perf MemoryMB must be a positive integer")?;
        anyhow::ensure!(memory_mb > 0, "VMM.Perf MemoryMB must be greater than zero");
        let requested_memory_bytes = memory_mb
            .checked_mul(MIB)
            .context("VMM.Perf MemoryMB is too large")?;
        anyhow::ensure!(
            requested_memory_bytes <= capacity.available_memory_bytes,
            "VMM.Perf requested {memory_mb} MB of memory, but the host has only {} MB available",
            capacity.available_memory_bytes / MIB
        );
    }

    Ok(())
}

pub(crate) fn validate_work_dir_capacity(
    work_dir: &Path,
    available_bytes: u64,
    required_bytes: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        available_bytes >= required_bytes,
        "VMM.Perf WorkDir {} has only {} GiB available; at least {} GiB is required",
        work_dir.display(),
        available_bytes / GIB,
        required_bytes / GIB
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn available_disk_bytes(path: &Path) -> anyhow::Result<u64> {
    let output = Command::new("df")
        .args(["-Pk", "--"])
        .arg(path)
        .output()
        .with_context(|| {
            format!(
                "failed to query available space for VMM.Perf WorkDir {}",
                path.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "failed to query available space for VMM.Perf WorkDir {}",
        path.display()
    );
    let output = String::from_utf8(output.stdout).context("df output was not valid UTF-8")?;
    let fields: Vec<_> = output
        .lines()
        .last()
        .context("df produced no output")?
        .split_whitespace()
        .collect();
    let available_kib = fields
        .get(3)
        .context("unexpected df output")?
        .parse::<u64>()
        .context("failed to parse available disk space from df")?;
    available_kib
        .checked_mul(1024)
        .context("available disk space overflowed")
}

#[cfg(target_os = "windows")]
fn available_disk_bytes(path: &Path) -> anyhow::Result<u64> {
    use sysinfo::Disks;

    let normalized_path = normalize_windows_path(path);
    let disks = Disks::new_with_refreshed_list();
    let disk = disks
        .iter()
        .filter_map(|disk| {
            let mount = normalize_windows_path(disk.mount_point());
            path_is_on_mount(&normalized_path, &mount).then_some((mount.len(), disk))
        })
        .max_by_key(|(mount_len, _)| *mount_len)
        .map(|(_, disk)| disk)
        .with_context(|| {
            format!(
                "failed to identify the Windows volume for VMM.Perf WorkDir {}",
                path.display()
            )
        })?;
    Ok(disk.available_space())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn available_disk_bytes(_path: &Path) -> anyhow::Result<u64> {
    anyhow::bail!("VMM.Perf supports Linux and Windows hosts only")
}

fn native_hypervisor_backend() -> anyhow::Result<&'static str> {
    #[cfg(target_os = "linux")]
    {
        let has_kvm = Path::new("/dev/kvm").exists();
        let has_mshv = Path::new("/dev/mshv").exists();
        match (has_kvm, has_mshv) {
            (true, false) => Ok("kvm"),
            (false, true) => Ok("mshv"),
            (false, false) => anyhow::bail!(
                "no native hypervisor device found; expected /dev/kvm or /dev/mshv, or set HypervisorBackend explicitly"
            ),
            (true, true) => anyhow::bail!(
                "both /dev/kvm and /dev/mshv are available; set HypervisorBackend explicitly"
            ),
        }
    }

    #[cfg(target_os = "windows")]
    {
        Ok("whp")
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        anyhow::bail!("VMM.Perf supports Linux and Windows hosts only")
    }
}

fn validate_host_environment(needs_sudo: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if needs_sudo {
            let status = Command::new("sudo")
                .args(["-n", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to check passwordless sudo")?;
            anyhow::ensure!(
                status.success(),
                "Linux VMM.Perf profiles require passwordless sudo"
            );
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = needs_sudo;
        let hypervisor_present = whp::capabilities::hypervisor_present()
            .context("failed to query Windows Hypervisor Platform availability")?;
        anyhow::ensure!(
            hypervisor_present,
            "Windows Hypervisor Platform is unavailable; enable the HypervisorPlatform feature and ensure the Windows hypervisor is running"
        );
        drop(
            whp::PartitionConfig::new()
                .context("failed to create a Windows Hypervisor Platform partition")?,
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = needs_sudo;

    Ok(())
}

#[cfg(target_os = "windows")]
fn normalize_windows_path(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        path = format!(r"\\{rest}");
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        path = rest.to_owned();
    }
    path.make_ascii_lowercase();
    path
}

#[cfg(target_os = "windows")]
fn path_is_on_mount(path: &str, mount: &str) -> bool {
    path.starts_with(mount)
        && (mount.ends_with('\\')
            || path.len() == mount.len()
            || path.as_bytes().get(mount.len()) == Some(&b'\\'))
}

#[cfg(target_os = "linux")]
fn owner_for_restore() -> anyhow::Result<Option<(String, String)>> {
    if running_as_root()? {
        Ok(None)
    } else {
        Ok(Some((
            current_id("-u", "user")?,
            current_id("-g", "group")?,
        )))
    }
}

#[cfg(target_os = "windows")]
fn owner_for_restore() -> anyhow::Result<Option<(String, String)>> {
    Ok(None)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn owner_for_restore() -> anyhow::Result<Option<(String, String)>> {
    anyhow::bail!("VMM.Perf supports Linux and Windows hosts only")
}

#[cfg(target_os = "linux")]
fn running_as_root() -> anyhow::Result<bool> {
    Ok(current_id("-u", "user")? == "0")
}

#[cfg(target_os = "linux")]
fn current_id(flag: &str, description: &str) -> anyhow::Result<String> {
    let output = Command::new("id")
        .arg(flag)
        .output()
        .with_context(|| format!("failed to query current {description} ID"))?;
    anyhow::ensure!(
        output.status.success(),
        "failed to query current {description} ID"
    );
    let id = String::from_utf8(output.stdout)
        .with_context(|| format!("current {description} ID was not valid UTF-8"))?;
    Ok(id.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::HostCapacity;
    use super::MIN_WORK_DIR_BYTES;
    use super::validate_requested_capacity;
    use super::validate_work_dir_capacity;
    use std::collections::BTreeMap;
    use std::path::Path;
    use test_with_tracing::test;

    #[test]
    fn validates_requested_capacity_within_host_limits() -> anyhow::Result<()> {
        validate_requested_capacity(
            &BTreeMap::from([
                ("CpuCount".into(), "4".into()),
                ("MemoryMB".into(), "4096".into()),
            ]),
            HostCapacity {
                logical_processors: 8,
                available_memory_bytes: 8 * 1024 * 1024 * 1024,
            },
        )?;
        Ok(())
    }

    #[test]
    fn rejects_requested_capacity_above_host_limits() {
        let error = validate_requested_capacity(
            &BTreeMap::from([("CpuCount".into(), "9".into())]),
            HostCapacity {
                logical_processors: 8,
                available_memory_bytes: 8 * 1024 * 1024 * 1024,
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("requested 9 CPUs"));
    }

    #[test]
    fn rejects_small_work_dir_capacity() {
        let error = validate_work_dir_capacity(
            Path::new("work"),
            MIN_WORK_DIR_BYTES - 1,
            MIN_WORK_DIR_BYTES,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("at least 30 GiB is required"));
    }
}
