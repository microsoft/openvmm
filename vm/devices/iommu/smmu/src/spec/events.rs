// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 event queue entry definitions.
//!
//! Event queue entries are 32 bytes (256 bits). The event type is in bits `[7:0]`
//! of the first dword.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum! {
    /// Event queue record types.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum EventId: u8 {
        /// Unsupported upstream transaction.
        F_UUT = 0x01,
        /// StreamID out of range.
        C_BAD_STREAMID = 0x02,
        /// STE fetch external abort.
        F_STE_FETCH = 0x03,
        /// Bad STE configuration.
        C_BAD_STE = 0x04,
        /// Bad ATS translation request.
        F_BAD_ATS_TREQ = 0x05,
        /// Stream disabled.
        F_STREAM_DISABLED = 0x06,
        /// ATS translated traffic forbidden.
        F_TRANSL_FORBIDDEN = 0x07,
        /// Bad SubstreamID.
        C_BAD_SUBSTREAMID = 0x08,
        /// CD fetch external abort.
        F_CD_FETCH = 0x09,
        /// Bad CD configuration.
        C_BAD_CD = 0x0A,
        /// Translation table walk external abort.
        F_WALK_EABT = 0x0B,
        /// Translation fault.
        F_TRANSLATION = 0x10,
        /// Address size fault.
        F_ADDR_SIZE = 0x11,
        /// Access flag fault.
        F_ACCESS = 0x12,
        /// Permission fault.
        F_PERMISSION = 0x13,
        /// TLB conflict.
        F_TLB_CONFLICT = 0x14,
    }
}

/// Event queue entry (32 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EvtEntry {
    /// Event type and SubstreamID info.
    pub header: EvtHeader,
    /// StreamID of the faulting device.
    pub sid: u32,
    /// Fault flags (RnW, S2, CLASS, etc.).
    pub flags: EvtFlags,
    /// Reserved / STAG.
    pub _stag: u32,
    /// Faulting input address (64-bit).
    pub input_addr: u64,
    /// Fetch address or reserved (64-bit).
    pub _fetch_addr: u64,
}

/// Event entry header (first 32 bits).
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EvtHeader {
    /// Event type.
    #[bits(8)]
    pub event_id: u8,
    #[bits(2)]
    _reserved0: u32,
    /// SubstreamID valid.
    pub ssv: bool,
    #[bits(1)]
    _reserved1: u32,
    /// SubstreamID (upper bits).
    #[bits(20)]
    pub ssid: u32,
}

/// Event entry flags (third 32-bit word).
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EvtFlags {
    /// Privileged/Unprivileged.
    pub pnu: bool,
    /// Instruction/Data.
    pub ind: bool,
    /// Read (true) / Write (false).
    pub rnw: bool,
    /// Stage 2 fault (false = S1 fault).
    pub s2: bool,
    /// Fault class.
    #[bits(2)]
    pub class: u8,
    #[bits(26)]
    _reserved: u32,
}

impl EvtEntry {
    /// Size of an event queue entry in bytes.
    pub const SIZE: usize = 32;

    /// Creates a new zeroed event entry.
    pub fn new() -> Self {
        Self {
            header: EvtHeader::new(),
            sid: 0,
            flags: EvtFlags::new(),
            _stag: 0,
            input_addr: 0,
            _fetch_addr: 0,
        }
    }

    /// Returns the event type.
    pub fn event_id(&self) -> EventId {
        EventId(self.header.event_id())
    }

    /// Creates a translation fault event.
    pub fn translation_fault(sid: u32, iova: u64, write: bool) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::F_TRANSLATION.0),
            sid,
            flags: EvtFlags::new().with_rnw(!write),
            input_addr: iova,
            ..Self::new()
        }
    }

    /// Creates a permission fault event.
    pub fn permission_fault(sid: u32, iova: u64, write: bool) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::F_PERMISSION.0),
            sid,
            flags: EvtFlags::new().with_rnw(!write),
            input_addr: iova,
            ..Self::new()
        }
    }

    /// Creates an access flag fault event.
    pub fn access_fault(sid: u32, iova: u64, write: bool) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::F_ACCESS.0),
            sid,
            flags: EvtFlags::new().with_rnw(!write),
            input_addr: iova,
            ..Self::new()
        }
    }

    /// Creates an address size fault event.
    pub fn addr_size_fault(sid: u32, iova: u64, write: bool) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::F_ADDR_SIZE.0),
            sid,
            flags: EvtFlags::new().with_rnw(!write),
            input_addr: iova,
            ..Self::new()
        }
    }

    /// Creates a bad STE event.
    pub fn bad_ste(sid: u32) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::C_BAD_STE.0),
            sid,
            ..Self::new()
        }
    }

    /// Creates a bad CD event.
    pub fn bad_cd(sid: u32) -> Self {
        Self {
            header: EvtHeader::new().with_event_id(EventId::C_BAD_CD.0),
            sid,
            ..Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_ids() {
        assert_eq!(EventId::F_UUT.0, 0x01);
        assert_eq!(EventId::C_BAD_STREAMID.0, 0x02);
        assert_eq!(EventId::C_BAD_STE.0, 0x04);
        assert_eq!(EventId::C_BAD_CD.0, 0x0A);
        assert_eq!(EventId::F_TRANSLATION.0, 0x10);
        assert_eq!(EventId::F_ADDR_SIZE.0, 0x11);
        assert_eq!(EventId::F_ACCESS.0, 0x12);
        assert_eq!(EventId::F_PERMISSION.0, 0x13);
    }

    #[test]
    fn test_evt_entry_size() {
        assert_eq!(size_of::<EvtEntry>(), 32);
    }

    #[test]
    fn test_evt_entry_translation_fault() {
        let evt = EvtEntry::translation_fault(0x42, 0x1000_2000, true);
        assert_eq!(evt.event_id(), EventId::F_TRANSLATION);
        assert_eq!(evt.sid, 0x42);
        assert_eq!(evt.input_addr, 0x1000_2000);
        // write → RnW = false (not-read)
        assert!(!evt.flags.rnw());
    }

    #[test]
    fn test_evt_entry_permission_fault() {
        let evt = EvtEntry::permission_fault(0x10, 0xFFFF_0000, false);
        assert_eq!(evt.event_id(), EventId::F_PERMISSION);
        assert_eq!(evt.sid, 0x10);
        assert_eq!(evt.input_addr, 0xFFFF_0000);
        // read → RnW = true
        assert!(evt.flags.rnw());
    }

    #[test]
    fn test_evt_entry_bad_ste() {
        let evt = EvtEntry::bad_ste(0x100);
        assert_eq!(evt.event_id(), EventId::C_BAD_STE);
        assert_eq!(evt.sid, 0x100);
    }

    #[test]
    fn test_evt_entry_access_fault() {
        let evt = EvtEntry::access_fault(5, 0xDEAD_BEEF_0000, true);
        assert_eq!(evt.event_id(), EventId::F_ACCESS);
        assert_eq!(evt.sid, 5);
        assert_eq!(evt.input_addr, 0xDEAD_BEEF_0000);
    }
}
