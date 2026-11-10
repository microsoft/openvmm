// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor limit, weight and reserve on MSHV.

use anyhow::Context as _;
use hvdef::HvPartitionPropertyCode;
use mshv_ioctls::VmFd;
use std::path::Path;
use std::path::PathBuf;

/// The scheduler the host runs, which decides how processor settings apply.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MshvScheduler {
    /// The hypervisor schedules VPs; partition properties apply.
    Hypervisor,
    /// Linux schedules VP threads; cgroups apply.
    Root,
}

/// Parses `/sys/devices/system/cpu/hotplug/states`. mshv_root registers the
/// `mshv_root_sched` state only under the root scheduler (`root_scheduler_init`
/// in drivers/hv/mshv_root_main.c).
pub fn parse_scheduler(hotplug_states: &str) -> MshvScheduler {
    if hotplug_states
        .lines()
        .any(|l| l.trim_end().ends_with("mshv_root_sched"))
    {
        MshvScheduler::Root
    } else {
        MshvScheduler::Hypervisor
    }
}

/// Returns the scheduler the host runs.
pub fn host_scheduler() -> anyhow::Result<MshvScheduler> {
    let states = fs_err::read_to_string("/sys/devices/system/cpu/hotplug/states")?;
    Ok(parse_scheduler(&states))
}

/// Which processor settings this host can enforce.
pub fn processor_qos_support() -> virt::ProcessorQosSupport {
    match host_scheduler() {
        Ok(MshvScheduler::Hypervisor) => virt::ProcessorQosSupport {
            limit: true,
            weight: true,
            reserve: true,
        },
        Ok(MshvScheduler::Root) => {
            let w = fs_err::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|s| own_cgroup_dir(&s))
                .is_some_and(|d| cgroup_writable(&d));
            virt::ProcessorQosSupport {
                limit: w,
                weight: w,
                reserve: false,
            }
        }
        Err(_) => virt::ProcessorQosSupport::default(),
    }
}

pub(crate) fn mshv_prop(p: virt::HvCpuProp) -> (HvPartitionPropertyCode, u64) {
    match p {
        virt::HvCpuProp::Cap(v) => (HvPartitionPropertyCode::CpuCap, v.into()),
        virt::HvCpuProp::Reserve(v) => (HvPartitionPropertyCode::CpuReserve, v.into()),
        virt::HvCpuProp::Weight(v) => (HvPartitionPropertyCode::CpuWeight, v.into()),
    }
}

fn read_hv(vmfd: &VmFd) -> anyhow::Result<virt::HvCpuProps> {
    let get = |code: HvPartitionPropertyCode| -> anyhow::Result<u32> {
        let v = vmfd
            .get_partition_property(code.0)
            .with_context(|| format!("failed to read {code:?}"))?;
        Ok(v as u32)
    };
    Ok(virt::HvCpuProps {
        cap: get(HvPartitionPropertyCode::CpuCap)?,
        reserve: get(HvPartitionPropertyCode::CpuReserve)?,
        weight: get(HvPartitionPropertyCode::CpuWeight)?,
    })
}

fn write_hv(vmfd: &VmFd, from: virt::HvCpuProps, to: virt::HvCpuProps) -> anyhow::Result<()> {
    for prop in from.writes_to(to) {
        let (code, v) = mshv_prop(prop);
        vmfd.set_partition_property(code.0, v)
            .with_context(|| format!("failed to set {code:?} to {v}"))?;
    }
    Ok(())
}

/// Applies `qos` to the partition, through the mechanism the host scheduler
/// enforces. `restore` holds the cgroup's original values on the root
/// scheduler; the partition drops it when it goes away.
pub(crate) fn apply(
    vmfd: &VmFd,
    vp_count: u32,
    qos: &virt::ProcessorQos,
    restore: &parking_lot::Mutex<Option<CgroupRestore>>,
) -> anyhow::Result<()> {
    qos.validate()?;
    match host_scheduler()? {
        MshvScheduler::Hypervisor => {
            let old = read_hv(vmfd)?;
            let new = old.target(qos)?;
            if let Err(err) = write_hv(vmfd, old, new) {
                // Put back what was written so the partition keeps its old settings.
                if let Err(restore) = read_hv(vmfd).and_then(|cur| write_hv(vmfd, cur, old)) {
                    return Err(err.context(format!(
                        "processor settings are inconsistent, restore failed: {restore:#}"
                    )));
                }
                return Err(err);
            }
            let p = read_hv(vmfd)?;
            tracing::info!(
                cap = p.cap,
                reserve = p.reserve,
                weight = p.weight,
                "processor scheduler properties applied"
            );
            Ok(())
        }
        MshvScheduler::Root => {
            let dir = fs_err::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|s| own_cgroup_dir(&s));
            match dir {
                Some(dir) if cgroup_writable(&dir) => {
                    apply_cgroup(&dir, vp_count, qos, &mut restore.lock())
                }
                dir => anyhow::bail!(
                    "processor controls need a delegated cgroup v2 at {}",
                    dir.map_or("<unknown>".into(), |d| d.display().to_string())
                ),
            }
        }
    }
}

/// Returns the cgroup v2 directory from `/proc/self/cgroup` contents.
pub(crate) fn own_cgroup_dir(proc_self_cgroup: &str) -> Option<PathBuf> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(path.trim().trim_start_matches('/')))
}

/// Returns the `cpu.max` line for a per-VP limit across `vp_count` VPs.
pub(crate) fn cpu_max_line(limit: u32, vp_count: u32) -> anyhow::Result<String> {
    const PERIOD_US: u64 = 100000;
    if limit >= 100000 {
        return Ok(format!("max {PERIOD_US}"));
    }
    // Per-VP fraction of one LP, times VPs, in microseconds per period.
    let quota = u64::from(limit) * u64::from(vp_count);
    // The kernel rejects quotas below 1 ms.
    anyhow::ensure!(
        quota >= 1000,
        "processor limit too small for cgroup cpu.max ({quota} us per {PERIOD_US} us)"
    );
    Ok(format!("{quota} {PERIOD_US}"))
}

/// Returns whether `dir` has writable `cpu.max` and `cpu.weight`.
pub(crate) fn cgroup_writable(dir: &Path) -> bool {
    ["cpu.max", "cpu.weight"].iter().all(|f| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(f))
            .is_ok()
    })
}

/// The original `cpu.max` and `cpu.weight` of a cgroup, written back on drop
/// so a VM's settings don't outlive it.
#[derive(Debug)]
pub struct CgroupRestore {
    dir: PathBuf,
    max: String,
    weight: String,
}

impl CgroupRestore {
    fn capture(dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            dir: dir.to_owned(),
            max: fs_err::read_to_string(dir.join("cpu.max"))?
                .trim()
                .to_owned(),
            weight: fs_err::read_to_string(dir.join("cpu.weight"))?
                .trim()
                .to_owned(),
        })
    }
}

impl Drop for CgroupRestore {
    fn drop(&mut self) {
        for (file, value) in [("cpu.max", &self.max), ("cpu.weight", &self.weight)] {
            if let Err(err) = fs_err::write(self.dir.join(file), value) {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "failed to restore cgroup processor settings"
                );
            }
        }
    }
}

/// Applies limit and weight to the cgroup at `dir`. These cover the whole
/// cgroup, device threads included. The first call saves the original values
/// in `restore`.
pub(crate) fn apply_cgroup(
    dir: &Path,
    vp_count: u32,
    qos: &virt::ProcessorQos,
    restore: &mut Option<CgroupRestore>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        qos.reserve.unwrap_or(0) == 0,
        "processor reserve has no equivalent under the root scheduler"
    );
    let max = qos.limit.map(|l| cpu_max_line(l, vp_count)).transpose()?;
    if restore.is_none() && (max.is_some() || qos.weight.is_some()) {
        *restore = Some(CgroupRestore::capture(dir)?);
    }
    let weight_path = dir.join("cpu.weight");
    let old_weight = match qos.weight {
        Some(w) => {
            let old = fs_err::read_to_string(&weight_path)?;
            fs_err::write(&weight_path, w.to_string())?;
            Some(old)
        }
        None => None,
    };
    if let Some(max) = max {
        if let Err(err) = fs_err::write(dir.join("cpu.max"), &max) {
            if let Some(old) = old_weight {
                fs_err::write(&weight_path, old.trim())
                    .context("processor settings are inconsistent, restoring cpu.weight failed")?;
            }
            return Err(err.into());
        }
    }
    tracing::info!(cgroup = %dir.display(), ?qos, "processor cgroup settings applied");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::MshvScheduler;
    use super::mshv_prop;
    use super::parse_scheduler;
    use hvdef::HvPartitionPropertyCode;
    use virt::HvCpuProp;

    #[test]
    fn hv_writes_map_to_mshv_property_codes() {
        assert_eq!(
            mshv_prop(HvCpuProp::Cap(32768)),
            (HvPartitionPropertyCode::CpuCap, 32768)
        );
        assert_eq!(
            mshv_prop(HvCpuProp::Reserve(6553)),
            (HvPartitionPropertyCode::CpuReserve, 6553)
        );
        assert_eq!(
            mshv_prop(HvCpuProp::Weight(200)),
            (HvPartitionPropertyCode::CpuWeight, 200)
        );
    }

    // Excerpts of /sys/devices/system/cpu/hotplug/states captured on an Azure
    // Linux dom0 (kernel 6.18.34.mshv4) under each scheduler.
    const CORE: &str = "201: virtio/net:online\n202: mshv_synic\n203: x86/mce:online\n";
    const ROOT: &str =
        "201: virtio/net:online\n202: mshv_synic\n203: mshv_root_sched\n204: x86/mce:online\n";

    #[test]
    fn parse_scheduler_detects_root() {
        assert_eq!(parse_scheduler(ROOT), MshvScheduler::Root);
        assert_eq!(parse_scheduler(CORE), MshvScheduler::Hypervisor);
        assert_eq!(parse_scheduler(""), MshvScheduler::Hypervisor);
    }

    #[test]
    fn own_cgroup_dir_parses_v2_line() {
        assert_eq!(
            super::own_cgroup_dir("0::/system.slice/ovm.scope\n"),
            Some(std::path::PathBuf::from(
                "/sys/fs/cgroup/system.slice/ovm.scope"
            ))
        );
        assert_eq!(super::own_cgroup_dir("4:cpu:/x\n"), None);
    }

    #[test]
    fn cpu_max_line_rejects_below_minimum() {
        assert_eq!(super::cpu_max_line(50000, 4).unwrap(), "200000 100000");
        assert_eq!(super::cpu_max_line(100000, 4).unwrap(), "max 100000");
        let err = super::cpu_max_line(100, 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("too small for cgroup cpu.max"),
            "{err:#}"
        );
    }

    #[test]
    fn cgroup_not_writable_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!super::cgroup_writable(dir.path()));
        std::fs::write(dir.path().join("cpu.max"), "max 100000\n").unwrap();
        std::fs::write(dir.path().join("cpu.weight"), "100\n").unwrap();
        assert!(super::cgroup_writable(dir.path()));
    }

    #[test]
    fn cgroup_apply_restores_weight_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpu.weight"), "100").unwrap();
        // A read-only cpu.max makes that write fail.
        let max = dir.path().join("cpu.max");
        std::fs::write(&max, "max 100000").unwrap();
        let mut perm = std::fs::metadata(&max).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&max, perm).unwrap();
        let qos = virt::ProcessorQos {
            limit: Some(50000),
            weight: Some(300),
            reserve: None,
        };
        assert!(super::apply_cgroup(dir.path(), 4, &qos, &mut None).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.weight")).unwrap(),
            "100"
        );
    }

    #[test]
    fn cgroup_rejects_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let qos = virt::ProcessorQos {
            reserve: Some(1000),
            ..Default::default()
        };
        let err = super::apply_cgroup(dir.path(), 4, &qos, &mut None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no equivalent under the root scheduler"),
            "{err:#}"
        );
    }

    #[test]
    fn cgroup_restore_puts_back_original_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let read = |f: &str| {
            std::fs::read_to_string(dir.path().join(f))
                .unwrap()
                .trim()
                .to_string()
        };
        std::fs::write(dir.path().join("cpu.max"), "300000 100000\n").unwrap();
        std::fs::write(dir.path().join("cpu.weight"), "50\n").unwrap();
        let mut restore = None;
        let qos = virt::ProcessorQos {
            limit: Some(50000),
            weight: Some(200),
            reserve: None,
        };
        super::apply_cgroup(dir.path(), 4, &qos, &mut restore).unwrap();
        assert_eq!(read("cpu.max"), "200000 100000");
        assert_eq!(read("cpu.weight"), "200");
        // A later update must not replace the values saved first.
        let qos = virt::ProcessorQos {
            limit: Some(100000),
            ..Default::default()
        };
        super::apply_cgroup(dir.path(), 4, &qos, &mut restore).unwrap();
        assert_eq!(read("cpu.max"), "max 100000");
        drop(restore);
        assert_eq!(read("cpu.max"), "300000 100000");
        assert_eq!(read("cpu.weight"), "50");
    }
}
