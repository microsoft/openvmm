// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use vmcore::monitor::MonitorId;

#[derive(Default)]
pub struct AssignedMonitors {
    bitmap: u128,
}

impl AssignedMonitors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn assign_monitor(&mut self) -> Option<MonitorId> {
        let index = (!self.bitmap).lowest_one()?;

        self.bitmap |= 1 << index;
        Some(MonitorId(index as u8))
    }

    pub fn claim_monitor(&mut self, monitor_id: MonitorId) -> bool {
        let bit = 1 << monitor_id.0;
        if self.bitmap & bit != 0 {
            return false;
        }

        self.bitmap |= bit;
        true
    }

    pub fn release_monitor(&mut self, monitor_id: MonitorId) {
        self.bitmap &= !(1 << monitor_id.0);
    }

    pub fn bitmap(&self) -> u128 {
        self.bitmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn assign_monitor_reuses_lowest_free_bit() {
        let mut monitors = AssignedMonitors { bitmap: u128::MAX };
        assert_eq!(monitors.assign_monitor(), None);
        for index in [127, 63, 0] {
            monitors.release_monitor(MonitorId(index));
        }
        for index in [0, 63, 127] {
            assert_eq!(monitors.assign_monitor(), Some(MonitorId(index)));
        }
        assert_eq!(monitors.assign_monitor(), None);
    }

    #[test]
    fn test_assign_release() {
        let mut monitors = AssignedMonitors::new();
        for i in 0..128 {
            let id = monitors.assign_monitor();
            assert_eq!(id, Some(MonitorId(i as u8)));
        }

        assert_eq!(monitors.assign_monitor(), None);

        monitors.release_monitor(MonitorId(10));
        let id = monitors.assign_monitor();
        assert_eq!(id, Some(MonitorId(10)));
    }

    #[test]
    fn test_claim() {
        let mut monitors = AssignedMonitors::new();
        assert!(monitors.claim_monitor(MonitorId(5)));
        assert!(!monitors.claim_monitor(MonitorId(5)));
        monitors.release_monitor(MonitorId(5));
        assert!(monitors.claim_monitor(MonitorId(5)));

        assert_eq!(monitors.assign_monitor(), Some(MonitorId(0)));
        assert!(!monitors.claim_monitor(MonitorId(0)));
    }
}
