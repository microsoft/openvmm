// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use inspect::Inspect;
use parking_lot::RwLock;
use std::ops::Range;
use std::sync::Arc;

pub trait SignalMsi: Send + Sync {
    fn signal_msi(&self, rid: u32, address: u64, data: u32);
}

struct DisconnectedMsiTarget;

impl SignalMsi for DisconnectedMsiTarget {
    fn signal_msi(&self, _rid: u32, _address: u64, _data: u32) {
        tracelimit::warn_ratelimited!("dropped MSI interrupt to disconnected target");
    }
}

pub struct MsiTargetControl {
    target: MsiTarget,
}

#[derive(Inspect, Debug, Clone)]
#[inspect(skip)]
pub struct MsiTarget {
    rids: Range<u32>,
    inner: Arc<RwLock<MsiTargetInner>>,
}

struct MsiTargetInner {
    rids: Range<u32>,
    signal_msi: Arc<dyn SignalMsi>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            rids,
            signal_msi: _,
        } = self;
        f.debug_struct("MsiTargetInner")
            .field("rids", rids)
            .finish()
    }
}

impl MsiTargetControl {
    pub fn new(rid_count: u32) -> Self {
        Self {
            target: MsiTarget {
                rids: 0..rid_count,
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    rids: 0..rid_count,
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                })),
            },
        }
    }

    pub fn connect(&self, rid_offset: u32, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.target.inner.write();
        inner.rids = rid_offset..(rid_offset + inner.rids.len() as u32);
        inner.signal_msi = signal_msi;
    }

    pub fn target(&self) -> &MsiTarget {
        &self.target
    }
}

impl MsiTarget {
    pub fn rid_count(&self) -> u32 {
        self.rids.end - self.rids.start
    }

    pub fn subtarget(&self, rids: Range<u32>) -> MsiTarget {
        assert!(rids.start >= 0 && rids.end <= self.rids.end - self.rids.start);
        MsiTarget {
            rids: (self.rids.start + rids.start)..(self.rids.start + rids.end),
            inner: self.inner.clone(),
        }
    }

    pub fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        let inner = self.inner.read();
        if let Some(abs_rid) = self.rid(&inner, rid) {
            inner.signal_msi.signal_msi(abs_rid, address, data);
        } else {
            tracelimit::warn_ratelimited!(
                rid,
                count = inner.rids.len(),
                "dropped MSI interrupt with invalid rid"
            );
        }
    }

    fn rid(&self, inner: &MsiTargetInner, rid: u32) -> Option<u32> {
        let this = self.rids.start.checked_add(rid)?;
        if this >= self.rids.end {
            return None;
        }
        let that = this.checked_sub(inner.rids.start)?;
        if that >= inner.rids.len() as u32 {
            return None;
        }
        Some(that)
    }
}
