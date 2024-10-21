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
        let index = self.bitmap.trailing_ones();
        if index == u128::BITS {
            return None;
        }

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
