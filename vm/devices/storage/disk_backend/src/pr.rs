// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent reservation support.

use crate::DiskError;
use inspect::Inspect;

/// Trait implemented by disks that support SCSI-style persistent reservations.
#[async_trait::async_trait]
pub trait PersistentReservation: Sync {
    /// Returns the disk's capabilities.
    fn capabilities(&self) -> ReservationCapabilities;

    /// Returns a report of the current registration and reservation state.
    async fn report(&self) -> Result<ReservationReport, DiskError>;

    /// Updates the registration for this client.
    ///
    /// If the current key does not match `old_key`, then fails with
    /// [`DiskError::ReservationConflict`]. If `old_key` is `None`, then update
    /// the registration regardless.
    ///
    /// If `new_key` is 0, then remove the registration.
    ///
    /// `ptpl` provides an optional new state for "persist through power loss".
    async fn register(
        &self,
        current_key: Option<u64>,
        new_key: u64,
        ptpl: Option<bool>,
    ) -> Result<(), DiskError>;

    /// Creates a reservation for this client with type `reservation_type`.
    ///
    /// Fails with [`DiskError::ReservationConflict`] if there is a key mismatch.
    async fn reserve(&self, key: u64, reservation_type: ReservationType) -> Result<(), DiskError>;

    /// Releases the reservation for this client with type `reservation_type`.
    ///
    /// Fails with [`DiskError::ReservationConflict`] if there is a key or type
    /// mismatch.
    async fn release(&self, key: u64, reservation_type: ReservationType) -> Result<(), DiskError>;

    /// Clears any reservation and registration for this client.
    ///
    /// Fails with [`DiskError::ReservationConflict`] if there is a key mismatch.
    async fn clear(&self, key: u64) -> Result<(), DiskError>;

    /// Preempts an existing reservation. See the SCSI spec for the precise
    /// behavior of this.
    async fn preempt(
        &self,
        current_key: u64,
        preempt_key: u64,
        reservation_type: ReservationType,
        abort: bool,
    ) -> Result<(), DiskError>;
}

/// Capabilities returned by [`PersistentReservation::capabilities`].
///
/// These bits correspond to values in [`ReservationType`].
#[expect(missing_docs)] // TODO
pub struct ReservationCapabilities {
    pub write_exclusive: bool,
    pub exclusive_access: bool,
    pub write_exclusive_registrants_only: bool,
    pub exclusive_access_registrants_only: bool,
    pub write_exclusive_all_registrants: bool,
    pub exclusive_access_all_registrants: bool,
    pub persist_through_power_loss: bool,
}

/// The reservation type.
///
/// These are defined in the SCSI spec.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
#[expect(missing_docs)] // TODO
pub enum ReservationType {
    WriteExclusive,
    ExclusiveAccess,
    WriteExclusiveRegistrantsOnly,
    ExclusiveAccessRegistrantsOnly,
    WriteExclusiveAllRegistrants,
    ExclusiveAccessAllRegistrants,
}

/// A registered controller.
#[derive(Debug, Clone)]
pub struct RegisteredController {
    /// The registration key.
    pub key: u64,
    /// The host ID of the client.
    pub host_id: Vec<u8>,
    /// The controller ID within the host.
    pub controller_id: u16,
    /// If true, the controller holds the reservation.
    pub holds_reservation: bool,
}

/// The report returned by [`PersistentReservation::report`].
#[derive(Debug, Clone)]
pub struct ReservationReport {
    /// A counter that increases every time a registration changes.
    pub generation: u32,
    /// The current reservation type for the disk.
    pub reservation_type: Option<ReservationType>,
    /// The persist through power loss state.
    pub persist_through_power_loss: bool,
    /// The registered controllers.
    pub controllers: Vec<RegisteredController>,
}
