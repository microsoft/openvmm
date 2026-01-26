// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use inspect::Inspect;
use parking_lot::RwLock;
use std::ops::Range;
use std::sync::Arc;

/// An object that can signal MSI interrupts.
pub trait SignalMsi: Send + Sync {
    /// Signals a message-signaled interrupt at the specified address with the specified data.
    ///
    /// `rid` is the requester ID of the PCI device sending the interrupt.
    fn signal_msi(&self, rid: u32, address: u64, data: u32);
}

struct DisconnectedMsiTarget;

impl SignalMsi for DisconnectedMsiTarget {
    fn signal_msi(&self, _rid: u32, _address: u64, _data: u32) {
        tracelimit::warn_ratelimited!("dropped MSI interrupt to disconnected target");
    }
}

/// A connection between a device and an MSI target.
#[derive(Debug)]
pub struct MsiConnection {
    target: MsiTarget,
}

/// An MSI target that can be used to signal MSI interrupts.
#[derive(Inspect, Debug, Clone)]
#[inspect(skip)]
pub struct MsiTarget {
    rid_range: Range<u32>,
    inner: Arc<RwLock<MsiTargetInner>>,
}

struct MsiTargetInner {
    rid_offset: u32,
    signal_msi: Arc<dyn SignalMsi>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            rid_offset,
            signal_msi: _,
        } = self;
        f.debug_struct("MsiTargetInner")
            .field("rid_offset", rid_offset)
            .finish()
    }
}

impl MsiConnection {
    /// Creates a new disconnected MSI target connection with the specified
    /// number of RIDs.
    pub fn new(rid_count: u32) -> Self {
        Self {
            target: MsiTarget {
                rid_range: 0..rid_count,
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    rid_offset: 0,
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                })),
            },
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    ///
    /// `rid_offset` is added to the RID of each interrupt signaled through
    /// this connection.
    pub fn connect(&self, rid_offset: u32, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.target.inner.write();
        inner.rid_offset = rid_offset;
        inner.signal_msi = signal_msi;
    }

    /// Returns the MSI target for this connection.
    pub fn target(&self) -> &MsiTarget {
        &self.target
    }
}

impl MsiTarget {
    /// Returns the number of RIDs that this target can address.
    pub fn rid_count(&self) -> u32 {
        self.rid_range.end - self.rid_range.start
    }

    /// Returns a subtarget addressing the specified range of RIDs. RID zero of
    /// the subtarget corresponds to `rids.start` of this target.
    ///
    /// # Panics
    /// Panics if `rids.end` is greater than the number of RIDs addressable by
    /// this target or if `rids.start` is greater than `rids.end`.
    pub fn subtarget(&self, rids: Range<u32>) -> MsiTarget {
        assert!(rids.start <= rids.end && rids.end <= self.rid_count());
        MsiTarget {
            rid_range: (self.rid_range.start + rids.start)..(self.rid_range.start + rids.end),
            inner: self.inner.clone(),
        }
    }

    /// Signals an MSI interrupt to this target from the specified RID.
    ///
    /// A single-RID device should use `0` as the RID.
    pub fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        let inner = self.inner.read();
        if let Some(abs_rid) = self.rid(rid) {
            inner
                .signal_msi
                .signal_msi(abs_rid + inner.rid_offset, address, data);
        } else {
            tracelimit::warn_ratelimited!(
                rid,
                count = self.rid_count(),
                "dropped MSI interrupt with invalid rid"
            );
        }
    }

    fn rid(&self, rid: u32) -> Option<u32> {
        let this = self.rid_range.start.checked_add(rid)?;
        if this >= self.rid_range.end {
            return None;
        }
        Some(this)
    }
}
