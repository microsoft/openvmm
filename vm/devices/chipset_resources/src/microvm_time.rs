// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The microVM restore packet version 5 and the time sample of NVX time ABI
//! 2, as the guest reads them through the portb device, and the platform
//! resource that pairs each time sample's host UTC with the guest TSC.
//!
//! Every byte the guest reads is a port exit, so the records are compact and
//! readable four bytes at a time.

use mesh::MeshPayload;
use std::sync::Arc;
use thiserror::Error;
use vm_resource::CanResolveTo;
use vm_resource::ResourceKind;

/// The restore packet magic.
pub const RESTORE_PACKET_MAGIC: [u8; 3] = *b"OVR";
/// The restore packet version.
pub const RESTORE_PACKET_VERSION: u8 = 5;
/// The length of the restore packet header.
pub const RESTORE_PACKET_HEADER_LEN: usize = 24;
/// The length of one memory range record.
pub const RESTORE_RANGE_LEN: usize = 16;
/// The length of the restore entropy.
pub const RESTORE_ENTROPY_LEN: usize = 64;
/// The most memory ranges a packet can carry.
pub const MAX_RESTORE_RANGES: usize = u8::MAX as usize;
/// The online VP targets a packet can carry; 0 means no processor target.
pub const VALID_ONLINE_VP_COUNTS: [u8; 5] = [0, 1, 2, 4, 8];
/// The largest rate deviation magnitude: 250 ppm scaled by 2^16.
pub const MAX_RATE_DEVIATION: i32 = 250 * 65_536;

/// `D` came from the UTC delta.
pub const FLAG_DOWNTIME_UTC: u8 = 1 << 0;
/// An explicit RAM target was requested.
pub const FLAG_MEMORY_TARGET: u8 = 1 << 1;
/// Host input is gated until the guest acknowledges through port `0x605`.
pub const FLAG_ACK_REQUIRED: u8 = 1 << 2;
/// A test hook altered time ABI behavior.
pub const FLAG_TEST_HOOKS: u8 = 1 << 3;
const RESTORE_PACKET_FLAGS: u8 =
    FLAG_DOWNTIME_UTC | FLAG_MEMORY_TARGET | FLAG_ACK_REQUIRED | FLAG_TEST_HOOKS;

/// The portb selector of the restore packet.
pub const RESTORE_PACKET_SELECT: u8 = 0xa5;
/// The portb selector of the generation ID.
pub const GENERATION_ID_SELECT: u8 = 0xa6;
/// The portb selector that latches a time sample.
pub const TIME_SAMPLE_SELECT: u8 = 0xa7;
/// The port of the time-sample window.
pub const TIME_WINDOW_PORT: u16 = 0xeb;
/// The portb status bit advertising the time-sample window.
pub const STATUS_TIME_SAMPLE_AVAILABLE: u8 = 1 << 6;
/// The time sample version.
pub const TIME_SAMPLE_VERSION: u8 = 2;
/// The length of a time sample.
pub const TIME_SAMPLE_LEN: usize = 48;
/// The encoding of an unknown bound in a time sample.
pub const TIME_SAMPLE_UNKNOWN: u32 = u32::MAX;

/// A restore packet or time sample is malformed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RestorePacketError {
    /// The record is shorter than its header.
    #[error("record is {0} bytes, shorter than its header")]
    Truncated(usize),
    /// The magic is wrong.
    #[error("bad restore packet magic")]
    Magic,
    /// The version is not supported.
    #[error("unsupported version {0}")]
    Version(u8),
    /// Reserved flag bits or bytes are set.
    #[error("reserved bits are set")]
    Reserved,
    /// The online VP target is not 0, 1, 2, 4, or 8.
    #[error("invalid online VP target {0}")]
    OnlineVpCount(u8),
    /// Memory ranges without a memory target, or too many ranges.
    #[error("invalid memory range count {0}")]
    MemoryRanges(usize),
    /// The generation counter of a restore is zero.
    #[error("restore generation is zero")]
    Generation,
    /// The rate deviation is beyond the tolerance.
    #[error("rate deviation {0} is beyond the tolerance")]
    RateDeviation(i32),
    /// The host clock status of a time sample is unknown to this version.
    #[error("unknown host clock status {0}")]
    Status(u8),
    /// The record length does not match its counts.
    #[error("record is {actual} bytes, expected {expected}")]
    Length {
        /// The expected length.
        expected: usize,
        /// The actual length.
        actual: usize,
    },
}

/// A guest RAM range in the restore packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub struct RestoreMemoryRange {
    /// The first guest physical address.
    pub gpa_start: u64,
    /// The length in bytes.
    pub length: u64,
}

/// The restore packet fields the controller knows before the worker starts.
#[derive(Debug, Clone, PartialEq, Eq, MeshPayload)]
pub struct RestorePacketBase {
    /// The online VP target: 0 for none, else 1, 2, 4, or 8.
    pub online_vp_count: u8,
    /// Whether an explicit RAM target was requested.
    pub memory_target: bool,
    /// Whether the guest must acknowledge through port `0x605`.
    pub ack_required: bool,
    /// The generation counter `g` of the restored process, at least 1.
    pub generation: u32,
    /// The RAM ranges to online, only with a memory target.
    pub ranges: Vec<RestoreMemoryRange>,
    /// The restore entropy; its first 16 bytes are the generation ID.
    pub entropy: [u8; RESTORE_ENTROPY_LEN],
}

impl RestorePacketBase {
    /// Checks the fields the packet encoding constrains.
    pub fn validate(&self) -> Result<(), RestorePacketError> {
        if !VALID_ONLINE_VP_COUNTS.contains(&self.online_vp_count) {
            return Err(RestorePacketError::OnlineVpCount(self.online_vp_count));
        }
        if self.ranges.len() > MAX_RESTORE_RANGES
            || (!self.memory_target && !self.ranges.is_empty())
        {
            return Err(RestorePacketError::MemoryRanges(self.ranges.len()));
        }
        if self.generation == 0 {
            return Err(RestorePacketError::Generation);
        }
        Ok(())
    }

    /// Returns the encoded length of the packet.
    pub fn encoded_len(&self) -> usize {
        RESTORE_PACKET_HEADER_LEN + RESTORE_RANGE_LEN * self.ranges.len() + RESTORE_ENTROPY_LEN
    }
}

/// The time fields of the restore packet, sealed by the worker before the
/// first restored VP runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub struct RestoreTimeRecord {
    /// The downtime `D`, in nanoseconds.
    pub downtime_ns: u64,
    /// Whether `D` came from the UTC delta.
    pub downtime_utc: bool,
    /// `(F_d - F_s) / F_s` in ppm scaled by 2^16.
    pub rate_deviation: i32,
    /// Whether a test hook is active.
    pub test_hooks: bool,
}

/// A complete restore packet version 5.
///
/// It carries no host UTC: the guest takes a time sample, which pairs host
/// UTC with its TSC, to set its clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePacketV5 {
    /// The controller's fields.
    pub base: RestorePacketBase,
    /// The worker's sealed time fields.
    pub time: RestoreTimeRecord,
}

impl RestorePacketV5 {
    /// Encodes the packet.
    pub fn encode(&self) -> Result<Vec<u8>, RestorePacketError> {
        self.base.validate()?;
        if self.time.rate_deviation.unsigned_abs() > MAX_RATE_DEVIATION.unsigned_abs() {
            return Err(RestorePacketError::RateDeviation(self.time.rate_deviation));
        }
        let mut flags = 0;
        for (set, flag) in [
            (self.time.downtime_utc, FLAG_DOWNTIME_UTC),
            (self.base.memory_target, FLAG_MEMORY_TARGET),
            (self.base.ack_required, FLAG_ACK_REQUIRED),
            (self.time.test_hooks, FLAG_TEST_HOOKS),
        ] {
            if set {
                flags |= flag;
            }
        }
        let mut bytes = Vec::with_capacity(self.base.encoded_len());
        bytes.extend_from_slice(&RESTORE_PACKET_MAGIC);
        bytes.extend_from_slice(&[
            RESTORE_PACKET_VERSION,
            flags,
            self.base.online_vp_count,
            self.base.ranges.len() as u8,
            0,
        ]);
        bytes.extend_from_slice(&self.base.generation.to_le_bytes());
        bytes.extend_from_slice(&self.time.rate_deviation.to_le_bytes());
        bytes.extend_from_slice(&self.time.downtime_ns.to_le_bytes());
        for range in &self.base.ranges {
            bytes.extend_from_slice(&range.gpa_start.to_le_bytes());
            bytes.extend_from_slice(&range.length.to_le_bytes());
        }
        bytes.extend_from_slice(&self.base.entropy);
        debug_assert_eq!(bytes.len(), self.base.encoded_len());
        Ok(bytes)
    }

    /// Decodes and validates a packet.
    pub fn decode(bytes: &[u8]) -> Result<Self, RestorePacketError> {
        let header: &[u8; RESTORE_PACKET_HEADER_LEN] = bytes
            .get(..RESTORE_PACKET_HEADER_LEN)
            .and_then(|header| header.try_into().ok())
            .ok_or(RestorePacketError::Truncated(bytes.len()))?;
        if header[..3] != RESTORE_PACKET_MAGIC {
            return Err(RestorePacketError::Magic);
        }
        if header[3] != RESTORE_PACKET_VERSION {
            return Err(RestorePacketError::Version(header[3]));
        }
        let flags = header[4];
        if flags & !RESTORE_PACKET_FLAGS != 0 || header[7] != 0 {
            return Err(RestorePacketError::Reserved);
        }
        let range_count = usize::from(header[6]);
        let expected =
            RESTORE_PACKET_HEADER_LEN + RESTORE_RANGE_LEN * range_count + RESTORE_ENTROPY_LEN;
        if bytes.len() != expected {
            return Err(RestorePacketError::Length {
                expected,
                actual: bytes.len(),
            });
        }
        let u32_at =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let u64_at =
            |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let ranges = (0..range_count)
            .map(|i| {
                let offset = RESTORE_PACKET_HEADER_LEN + RESTORE_RANGE_LEN * i;
                RestoreMemoryRange {
                    gpa_start: u64_at(offset),
                    length: u64_at(offset + 8),
                }
            })
            .collect();
        let entropy_offset = expected - RESTORE_ENTROPY_LEN;
        let packet = Self {
            base: RestorePacketBase {
                online_vp_count: header[5],
                memory_target: flags & FLAG_MEMORY_TARGET != 0,
                ack_required: flags & FLAG_ACK_REQUIRED != 0,
                generation: u32_at(8),
                ranges,
                entropy: bytes[entropy_offset..].try_into().unwrap(),
            },
            time: RestoreTimeRecord {
                downtime_ns: u64_at(16),
                downtime_utc: flags & FLAG_DOWNTIME_UTC != 0,
                rate_deviation: u32_at(12) as i32,
                test_hooks: flags & FLAG_TEST_HOOKS != 0,
            },
        };
        packet.base.validate()?;
        if packet.time.rate_deviation.unsigned_abs() > MAX_RATE_DEVIATION.unsigned_abs() {
            return Err(RestorePacketError::RateDeviation(
                packet.time.rate_deviation,
            ));
        }
        Ok(packet)
    }
}

/// The status of host UTC in a time sample, as the host's time service
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClockStatus {
    /// The host does not report its clock's status.
    Unknown,
    /// A time service synchronizes host UTC with a reference.
    Synchronized,
    /// No time service synchronizes host UTC, which runs on its own.
    FreeRunning,
    /// The host reports a fault of its clock.
    Unreliable,
}

impl HostClockStatus {
    /// Returns the encoding of the status.
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Synchronized => 1,
            Self::FreeRunning => 2,
            Self::Unreliable => 3,
        }
    }

    /// Returns the status that `byte` encodes, if any.
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Unknown),
            1 => Some(Self::Synchronized),
            2 => Some(Self::FreeRunning),
            3 => Some(Self::Unreliable),
            _ => None,
        }
    }
}

/// The host's fields of a time sample: host UTC paired with the guest TSC,
/// and the discipline of host UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTimeFields {
    /// Host UTC at the latch, in nanoseconds since the Unix epoch.
    pub utc_ns: u64,
    /// The guest TSC at the instant of `utc_ns`.
    pub tsc: u64,
    /// The most the instants of `tsc` and `utc_ns` can differ, in
    /// nanoseconds.
    pub pairing_ns: u32,
    /// The status of host UTC.
    pub status: HostClockStatus,
    /// A counter that changes when host UTC steps, the host resumes from a
    /// suspension, or the guest TSC's offset from the host TSC changes.
    pub disruption: u32,
    /// How fast host UTC runs against the guest TSC counted at the declared
    /// rate `F`, minus one, in ppm scaled by 2^16.
    pub correction: i64,
    /// A bound on the error of `correction`, in ppm scaled by 2^16, if
    /// known.
    pub correction_bound: Option<u32>,
    /// The time service's bound on the error of host UTC, in microseconds,
    /// if it reports one.
    pub max_error_us: Option<u32>,
}

/// A time sample version 2, latched by selector [`TIME_SAMPLE_SELECT`] and
/// read from port [`TIME_WINDOW_PORT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSample {
    /// Whether a test hook is active.
    pub test_hooks: bool,
    /// The generation counter `g` of this VM process.
    pub generation: u32,
    /// The host's fields.
    pub host: HostTimeFields,
}

/// Encodes an optional bound, saturating a known one below
/// [`TIME_SAMPLE_UNKNOWN`].
fn encode_bound(bound: Option<u32>) -> u32 {
    bound.map_or(TIME_SAMPLE_UNKNOWN, |bound| {
        bound.min(TIME_SAMPLE_UNKNOWN - 1)
    })
}

/// Decodes an optional bound.
fn decode_bound(value: u32) -> Option<u32> {
    (value != TIME_SAMPLE_UNKNOWN).then_some(value)
}

impl TimeSample {
    /// Encodes the sample.
    pub fn encode(&self) -> [u8; TIME_SAMPLE_LEN] {
        let host = &self.host;
        let mut bytes = [0; TIME_SAMPLE_LEN];
        bytes[0] = TIME_SAMPLE_VERSION;
        bytes[1] = if self.test_hooks { FLAG_TEST_HOOKS } else { 0 };
        bytes[2] = host.status.to_byte();
        bytes[4..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..16].copy_from_slice(&host.utc_ns.to_le_bytes());
        bytes[16..24].copy_from_slice(&host.tsc.to_le_bytes());
        bytes[24..28].copy_from_slice(&host.pairing_ns.to_le_bytes());
        bytes[28..32].copy_from_slice(&host.disruption.to_le_bytes());
        bytes[32..40].copy_from_slice(&host.correction.to_le_bytes());
        bytes[40..44].copy_from_slice(&encode_bound(host.correction_bound).to_le_bytes());
        bytes[44..48].copy_from_slice(&encode_bound(host.max_error_us).to_le_bytes());
        bytes
    }

    /// Decodes and validates a sample.
    pub fn decode(bytes: &[u8]) -> Result<Self, RestorePacketError> {
        let bytes: &[u8; TIME_SAMPLE_LEN] =
            bytes.try_into().map_err(|_| RestorePacketError::Length {
                expected: TIME_SAMPLE_LEN,
                actual: bytes.len(),
            })?;
        if bytes[0] != TIME_SAMPLE_VERSION {
            return Err(RestorePacketError::Version(bytes[0]));
        }
        if bytes[1] & !FLAG_TEST_HOOKS != 0 || bytes[3] != 0 {
            return Err(RestorePacketError::Reserved);
        }
        let status =
            HostClockStatus::from_byte(bytes[2]).ok_or(RestorePacketError::Status(bytes[2]))?;
        let u32_at =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let u64_at =
            |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        Ok(Self {
            test_hooks: bytes[1] & FLAG_TEST_HOOKS != 0,
            generation: u32_at(4),
            host: HostTimeFields {
                utc_ns: u64_at(8),
                tsc: u64_at(16),
                pairing_ns: u32_at(24),
                status,
                disruption: u32_at(28),
                correction: u64_at(32) as i64,
                correction_bound: decode_bound(u32_at(40)),
                max_error_us: decode_bound(u32_at(44)),
            },
        })
    }
}

/// The platform's source of the host's fields of time samples, which pairs
/// host UTC with the guest TSC.
pub trait TimeSampleSource: Send + Sync {
    /// Pairs host UTC with the guest TSC now and reads the discipline of
    /// host UTC, or returns `None` if the host cannot, which leaves the time
    /// window empty.
    fn sample(&self) -> Option<HostTimeFields>;
}

/// The resource kind of the platform's [`TimeSampleSource`], which the
/// microVM portb device resolves from
/// [`PlatformResource`](vm_resource::PlatformResource).
pub enum TimeSampleSourceKind {}

impl ResourceKind for TimeSampleSourceKind {
    const NAME: &'static str = "microvm_time_sample_source";
}

impl CanResolveTo<ResolvedTimeSampleSource> for TimeSampleSourceKind {
    type Input<'a> = ();
}

/// A resolved [`TimeSampleSourceKind`] resource.
pub struct ResolvedTimeSampleSource(pub Arc<dyn TimeSampleSource>);

#[cfg(test)]
mod tests {
    use super::*;

    fn entropy() -> [u8; RESTORE_ENTROPY_LEN] {
        std::array::from_fn(|i| i as u8)
    }

    fn packet_without_ranges() -> RestorePacketV5 {
        RestorePacketV5 {
            base: RestorePacketBase {
                online_vp_count: 2,
                memory_target: false,
                ack_required: true,
                generation: 7,
                ranges: Vec::new(),
                entropy: entropy(),
            },
            time: RestoreTimeRecord {
                downtime_ns: 0x0102_0304_0506_0708,
                downtime_utc: true,
                rate_deviation: -2,
                test_hooks: false,
            },
        }
    }

    fn packet_with_ranges() -> RestorePacketV5 {
        RestorePacketV5 {
            base: RestorePacketBase {
                online_vp_count: 0,
                memory_target: true,
                ack_required: false,
                generation: 1,
                ranges: vec![
                    RestoreMemoryRange {
                        gpa_start: 0x1_0000_0000,
                        length: 0x800_0000,
                    },
                    RestoreMemoryRange {
                        gpa_start: 0x2_0000_0000,
                        length: 0x1000_0000,
                    },
                ],
                entropy: entropy(),
            },
            time: RestoreTimeRecord {
                downtime_ns: 30_000_000_000,
                downtime_utc: false,
                rate_deviation: MAX_RATE_DEVIATION,
                test_hooks: true,
            },
        }
    }

    #[test]
    fn golden_packet_without_ranges() {
        let mut expected = vec![
            b'O', b'V', b'R', 5, 0x05, 2, 0, 0, // header
            7, 0, 0, 0, // generation
            0xfe, 0xff, 0xff, 0xff, // rate deviation
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // downtime
        ];
        expected.extend(entropy());
        let packet = packet_without_ranges();
        let bytes = packet.encode().unwrap();
        assert_eq!(bytes.len(), 88);
        assert_eq!(bytes, expected);
        assert_eq!(RestorePacketV5::decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn golden_packet_with_ranges() {
        let mut expected = vec![
            b'O', b'V', b'R', 5, 0x0a, 0, 2, 0, // header
            1, 0, 0, 0, // generation
            0x00, 0x00, 0xfa, 0x00, // rate deviation 250 * 2^16
            0x00, 0xac, 0x23, 0xfc, 0x06, 0x00, 0x00, 0x00, // downtime 30 s
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // range 0 start
            0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, // range 0 length
            0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // range 1 start
            0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, // range 1 length
        ];
        expected.extend(entropy());
        let packet = packet_with_ranges();
        let bytes = packet.encode().unwrap();
        assert_eq!(bytes.len(), 120);
        assert_eq!(bytes, expected);
        assert_eq!(RestorePacketV5::decode(&bytes).unwrap(), packet);
    }

    #[test]
    fn decode_rejects_malformed_packets() {
        let good = packet_with_ranges().encode().unwrap();
        let mutate = |offset: usize, value: u8| {
            let mut bytes = good.clone();
            bytes[offset] = value;
            RestorePacketV5::decode(&bytes).unwrap_err()
        };
        assert_eq!(mutate(0, b'X'), RestorePacketError::Magic);
        assert_eq!(mutate(3, 4), RestorePacketError::Version(4));
        assert_eq!(mutate(4, 0x1a), RestorePacketError::Reserved);
        assert_eq!(mutate(7, 1), RestorePacketError::Reserved);
        assert_eq!(mutate(5, 3), RestorePacketError::OnlineVpCount(3));
        assert_eq!(mutate(4, 0x08), RestorePacketError::MemoryRanges(2));
        assert!(matches!(mutate(6, 3), RestorePacketError::Length { .. }));
        assert_eq!(mutate(8, 0), RestorePacketError::Generation);
        assert!(matches!(
            mutate(14, 0xfb),
            RestorePacketError::RateDeviation(_)
        ));
        assert_eq!(
            RestorePacketV5::decode(&good[..23]).unwrap_err(),
            RestorePacketError::Truncated(23)
        );
        assert!(matches!(
            RestorePacketV5::decode(&good[..good.len() - 1]).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
    }

    #[test]
    fn encode_rejects_invalid_fields() {
        let mut packet = packet_without_ranges();
        packet.base.online_vp_count = 3;
        assert_eq!(
            packet.encode().unwrap_err(),
            RestorePacketError::OnlineVpCount(3)
        );

        let mut packet = packet_with_ranges();
        packet.base.memory_target = false;
        assert_eq!(
            packet.encode().unwrap_err(),
            RestorePacketError::MemoryRanges(2)
        );

        let mut packet = packet_without_ranges();
        packet.base.generation = 0;
        assert_eq!(packet.encode().unwrap_err(), RestorePacketError::Generation);

        let mut packet = packet_without_ranges();
        packet.time.rate_deviation = -MAX_RATE_DEVIATION - 1;
        assert!(matches!(
            packet.encode().unwrap_err(),
            RestorePacketError::RateDeviation(_)
        ));
    }

    fn sample() -> TimeSample {
        TimeSample {
            test_hooks: true,
            generation: 3,
            host: HostTimeFields {
                utc_ns: 0x0102_0304_0506_0708,
                tsc: 0x1112_1314_1516_1718,
                pairing_ns: 0x2122_2324,
                status: HostClockStatus::Synchronized,
                disruption: 0x3132_3334,
                correction: -2,
                correction_bound: Some(66),
                max_error_us: None,
            },
        }
    }

    #[test]
    fn golden_time_sample() {
        let sample = sample();
        let bytes = sample.encode();
        assert_eq!(
            bytes,
            [
                2, 0x08, 1, 0, // version, flags, status, reserved
                3, 0, 0, 0, // generation
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // UTC
                0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, // TSC
                0x24, 0x23, 0x22, 0x21, // pairing
                0x34, 0x33, 0x32, 0x31, // disruption
                0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // correction
                66, 0, 0, 0, // correction bound
                0xff, 0xff, 0xff, 0xff, // maximum error, unknown
            ]
        );
        assert_eq!(TimeSample::decode(&bytes).unwrap(), sample);

        let mut bad = bytes;
        bad[0] = 1;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Version(1)
        );
        let mut bad = bytes;
        bad[1] = 0x01;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Reserved
        );
        let mut bad = bytes;
        bad[3] = 1;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Reserved
        );
        let mut bad = bytes;
        bad[2] = 4;
        assert_eq!(
            TimeSample::decode(&bad).unwrap_err(),
            RestorePacketError::Status(4)
        );
        assert!(matches!(
            TimeSample::decode(&bytes[..47]).unwrap_err(),
            RestorePacketError::Length { .. }
        ));
    }

    #[test]
    fn known_bounds_never_encode_as_unknown() {
        let mut sample = sample();
        sample.host.correction_bound = Some(u32::MAX);
        sample.host.max_error_us = Some(0);
        let decoded = TimeSample::decode(&sample.encode()).unwrap();
        assert_eq!(decoded.host.correction_bound, Some(u32::MAX - 1));
        assert_eq!(decoded.host.max_error_us, Some(0));
        for status in [
            HostClockStatus::Unknown,
            HostClockStatus::Synchronized,
            HostClockStatus::FreeRunning,
            HostClockStatus::Unreliable,
        ] {
            assert_eq!(HostClockStatus::from_byte(status.to_byte()), Some(status));
        }
    }
}
