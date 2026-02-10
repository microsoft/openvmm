// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use inspect::Inspect;
use parking_lot::RwLock;
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
    inner: Arc<RwLock<MsiTargetInner>>,
}

struct MsiTargetInner {
    signal_msi: Arc<dyn SignalMsi>,
}

impl std::fmt::Debug for MsiTargetInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { signal_msi: _ } = self;
        f.debug_struct("MsiTargetInner").finish()
    }
}

impl MsiConnection {
    /// Creates a new disconnected MSI target connection.
    pub fn new() -> Self {
        Self {
            target: MsiTarget {
                inner: Arc::new(RwLock::new(MsiTargetInner {
                    signal_msi: Arc::new(DisconnectedMsiTarget),
                })),
            },
        }
    }

    /// Updates the MSI target to which this connection signals interrupts.
    pub fn connect(&self, signal_msi: Arc<dyn SignalMsi>) {
        let mut inner = self.target.inner.write();
        inner.signal_msi = signal_msi;
    }

    /// Returns the MSI target for this connection.
    pub fn target(&self) -> &MsiTarget {
        &self.target
    }
}

impl MsiTarget {
    /// Signals an MSI interrupt to this target from the specified RID.
    ///
    /// A single-RID device should use `0` as the RID.
    pub fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        let inner = self.inner.read();
        inner.signal_msi.signal_msi(rid, address, data);
    }
}
