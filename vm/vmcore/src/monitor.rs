// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for supporting hypervisor monitor pages.

#![forbid(unsafe_code)]

use hvdef::HV_PAGE_SIZE;
use hvdef::HvMonitorPage;
use hvdef::HvMonitorPageSmall;
use inspect::Inspect;
use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// Four groups of 32 bits.
const MAX_MONITORS: usize = 128;
const INVALID_MONITOR_GPA: u64 = u64::MAX;
const INVALID_CONNECTION_ID: u32 = !0;

/// The ID used for signaling a monitored interrupt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MonitorId(pub u8);

impl MonitorId {
    /// An invalid monitor ID value.
    pub const INVALID: MonitorId = MonitorId(u8::MAX);
}

/// Holds information about the monitor page and registered monitors.
#[derive(Debug)]
pub struct MonitorPage {
    gpa: AtomicU64,
    monitors: Arc<MonitorList>,
}

impl Inspect for MonitorPage {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        if let Some(gpa) = self.gpa() {
            resp.hex("gpa", gpa);
        }
        resp.field("monitors", &self.monitors);
    }
}

#[derive(Debug)]
struct MonitorList([AtomicU32; MAX_MONITORS]);

impl MonitorList {
    fn new() -> Self {
        Self([INVALID_CONNECTION_ID; MAX_MONITORS].map(Into::into))
    }

    fn set(&self, monitor_id: MonitorId, connection_id: Option<u32>) {
        let old_connection_id = self.0[monitor_id.0 as usize].swap(
            connection_id.unwrap_or(INVALID_CONNECTION_ID),
            Ordering::Relaxed,
        );
        assert!(
            old_connection_id == INVALID_CONNECTION_ID || connection_id.is_none(),
            "requested monitor ID {} already in use",
            monitor_id.0
        );
    }

    fn get(&self, monitor_id: MonitorId) -> Option<u32> {
        let connection_id = self.0[monitor_id.0 as usize].load(Ordering::Relaxed);
        if connection_id != INVALID_CONNECTION_ID {
            Some(connection_id)
        } else {
            None
        }
    }
}

impl Inspect for MonitorList {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp: inspect::Response<'_> = req.respond();
        for monitor_id in 0..MAX_MONITORS {
            if let Some(connection_id) = self.get(MonitorId(monitor_id as u8)) {
                resp.hex(&monitor_id.to_string(), connection_id);
            }
        }
    }
}

impl MonitorPage {
    /// Creates a new `MonitorPage`.
    pub fn new() -> Self {
        Self {
            gpa: AtomicU64::new(INVALID_MONITOR_GPA),
            monitors: Arc::new(MonitorList::new()),
        }
    }

    /// Sets the GPA of the monitor page currently in use.
    pub fn set_gpa(&self, gpa: Option<u64>) -> Option<u64> {
        assert!(gpa.is_none_or(|gpa| gpa % HV_PAGE_SIZE == 0));
        let old = self
            .gpa
            .swap(gpa.unwrap_or(INVALID_MONITOR_GPA), Ordering::Relaxed);

        (old != INVALID_MONITOR_GPA).then_some(old)
    }

    /// Gets the current GPA of the monitor page, or None if no monitor page is in use.
    pub fn gpa(&self) -> Option<u64> {
        let gpa = self.gpa.load(Ordering::Relaxed);
        (gpa != INVALID_MONITOR_GPA).then_some(gpa)
    }

    /// Registers a monitored interrupt, optionally using a pre-existing ID. The returned struct
    /// will unregister the ID when dropped.
    ///
    /// # Panics
    ///
    /// Panics if monitor_id is already in use.
    pub fn register_monitor(
        &self,
        monitor_id: MonitorId,
        connection_id: u32,
    ) -> Box<dyn Sync + Send> {
        self.monitors.set(monitor_id, Some(connection_id));

        tracing::trace!(monitor_id = monitor_id.0, "registered monitor");
        Box::new(RegisteredMonitor {
            monitors: self.monitors.clone(),
            monitor_id,
        })
    }

    /// Sets one bit within the monitor page, returning the connection ID to
    /// signal.
    pub fn write_bit(&self, page_bit: u32) -> Option<u32> {
        const TRIGGER_GROUP_OFFSET: u32 = offset_of!(HvMonitorPage, trigger_group) as u32 * 8;
        let trigger_bit = page_bit.checked_sub(TRIGGER_GROUP_OFFSET)?;
        let group = trigger_bit / 64;
        let trigger = trigger_bit % 64;
        if group >= 4 || trigger >= 32 {
            return None;
        }
        let monitor_id = group * 32 + trigger;
        if let Some(connection_id) = self.monitors.get(MonitorId(monitor_id as u8)) {
            Some(connection_id)
        } else {
            tracelimit::warn_ratelimited!(monitor_id, "monitor write for unknown id");
            None
        }
    }

    /// Check if the specified write is wholly inside the monitor page, and signal the associated
    /// interrupt if it is.
    pub fn check_write(&self, gpa: u64, bytes: &[u8], mut signal: impl FnMut(u32)) -> bool {
        let page_gpa = self.gpa.load(Ordering::Relaxed);
        if page_gpa != gpa & !(HV_PAGE_SIZE - 1) {
            return false;
        }

        if gpa + bytes.len() as u64 > page_gpa + size_of::<HvMonitorPageSmall>() as u64 {
            tracelimit::warn_ratelimited!(gpa, "write to unused portion of monitor page");
            // Still return true because no further action should be taken.
            return true;
        }

        let mut page = HvMonitorPageSmall::new_zeroed();
        let offset = (gpa - page_gpa) as usize;
        page.as_mut_bytes()[offset..offset + bytes.len()].copy_from_slice(bytes);
        for (group_index, group) in page.trigger_group.iter().enumerate() {
            let mut value = group.pending;
            while value != 0 {
                let index = value.trailing_zeros();
                value &= !(1 << index);
                let monitor_id = group_index * 32 + (index as usize);
                if let Some(connection_id) = &self.monitors.get(MonitorId(monitor_id as u8)) {
                    signal(*connection_id);
                } else {
                    tracelimit::warn_ratelimited!(monitor_id, "monitor write for unknown id");
                }
            }
        }

        true
    }

    /// Checks if a read if inside the monitor page, and fills the buffer if it is.
    /// N.B. This is used to handle cases where the instruction emulator needs to read the monitor
    ///      page. The guest should have read access to the page so doesn't need to go through this
    ///      path.
    pub fn check_read(&self, gpa: u64, bytes: &mut [u8]) -> bool {
        let page_gpa = self.gpa.load(Ordering::Relaxed);
        if page_gpa != gpa & !(HV_PAGE_SIZE - 1) {
            return false;
        }

        // Since this implementation does not use the distinct armed and pending states, always
        // returning a zero-filled buffer is sufficient.
        bytes.fill(0);
        true
    }
}

// Represents a registered monitor ID, which will be unregistered when the struct is dropped.
struct RegisteredMonitor {
    monitors: Arc<MonitorList>,
    monitor_id: MonitorId,
}

impl Drop for RegisteredMonitor {
    fn drop(&mut self) {
        tracing::trace!(monitor_id = self.monitor_id.0, "unregistered monitor");
        self.monitors.set(self.monitor_id, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn test_set_gpa() {
        let monitor = MonitorPage::new();
        assert!(monitor.set_gpa(Some(0x123f000)).is_none());
        assert_eq!(monitor.set_gpa(None), Some(0x123f000));
        assert!(monitor.set_gpa(None).is_none());
    }

    #[test]
    fn test_write() {
        let monitor = MonitorPage::new();
        monitor.set_gpa(Some(HV_PAGE_SIZE));
        let _reg1 = monitor.register_monitor(MonitorId(5), 42);
        let _reg1 = monitor.register_monitor(MonitorId(7), 47);
        let _reg1 = monitor.register_monitor(MonitorId(9), 49);
        let _reg2 = monitor.register_monitor(MonitorId(127), 500);
        let mut page = HvMonitorPageSmall::new_zeroed();
        page.trigger_group[0].pending = 1 << 5;

        // Write outside of monitor page.
        assert!(
            !monitor.check_write(HV_PAGE_SIZE * 2, page.as_bytes(), |_| panic!(
                "Should not be called."
            ))
        );

        assert!(
            !monitor.check_write(HV_PAGE_SIZE - 1, page.as_bytes(), |_| panic!(
                "Should not be called."
            ))
        );

        // Write to monitor page.
        let mut triggered = Vec::new();
        assert!(monitor.check_write(HV_PAGE_SIZE, page.as_bytes(), |id| triggered.push(id)));
        assert_eq!(triggered, vec![42]);

        // Write multiple IDs, no call for unknown ID, other data ignored.
        page.trigger_state.set_group_enable(2);
        page.trigger_group[0].pending = (1 << 5) | (1 << 6) | (1 << 7);
        page.trigger_group[3].pending = 1 << 31;
        triggered.clear();
        assert!(monitor.check_write(HV_PAGE_SIZE, page.as_bytes(), |id| triggered.push(id)));
        assert_eq!(triggered, vec![42, 47, 500]);

        // Partial write
        let pending = 1 << 9;
        triggered.clear();
        assert!(monitor.check_write(
            HV_PAGE_SIZE + offset_of!(HvMonitorPageSmall, trigger_group) as u64,
            pending.as_bytes(),
            |id| triggered.push(id),
        ));

        assert_eq!(triggered, vec![49]);
    }
}
