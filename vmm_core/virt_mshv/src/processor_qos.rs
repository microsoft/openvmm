// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor limit, weight and reserve on MSHV.

use anyhow::Context as _;
use hvdef::HvPartitionPropertyCode;
use mshv_ioctls::VmFd;

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
        Ok(MshvScheduler::Root) | Err(_) => virt::ProcessorQosSupport::default(),
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
/// enforces.
pub(crate) fn apply(vmfd: &VmFd, _vp_count: u32, qos: &virt::ProcessorQos) -> anyhow::Result<()> {
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
            anyhow::bail!("processor limit/weight/reserve are not supported on the root scheduler")
        }
    }
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
}
