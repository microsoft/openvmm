// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor limit, weight and reserve on MSHV.

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

#[cfg(test)]
mod tests {
    use super::MshvScheduler;
    use super::parse_scheduler;

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
