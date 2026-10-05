// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI payloads shared by the controller and the VM worker, including
//! the snapshot manifest's time contract and CPU profile record.

use chipset_resources::microvm_time::RestoreTimeRecord;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use virt::time_abi::CaptureTimeRecord;
use virt::time_abi::HostClockKind;
use virt::time_abi::HostIdentity;
use virt::time_abi::HostTimeSample;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiTestHooks;

/// The manifest's time contract, recorded at the capture anchor.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotTimeContract {
    /// The time ABI version: 1.
    #[mesh(1)]
    pub time_abi_version: u32,
    /// `F_s`, the declared TSC rate of the snapshot lineage, in Hz.
    #[mesh(2)]
    pub tsc_frequency_hz: u64,
    /// The TSC rate tolerance: 250.
    #[mesh(3)]
    pub tsc_tolerance_ppm: u32,
    /// `L_s`, the LAPIC timer rate, in Hz.
    #[mesh(4)]
    pub apic_frequency_hz: u64,
    /// `T_c`, VP 0's TSC at the capture anchor.
    #[mesh(5)]
    pub capture_tsc: u64,
    /// Host UTC at the capture anchor, in nanoseconds since the Unix epoch.
    #[mesh(6)]
    pub capture_utc_ns: u64,
    /// Host monotonic time at the capture anchor, in nanoseconds.
    #[mesh(7)]
    pub capture_monotonic_ns: u64,
    /// The host monotonic clock: `linux-boottime` or
    /// `windows-interrupt-time`.
    #[mesh(8)]
    pub host_clock: String,
    /// The 16-byte host identity.
    #[mesh(9)]
    pub host_id: Vec<u8>,
    /// The 16-byte host boot identity.
    #[mesh(10)]
    pub host_boot_id: Vec<u8>,
    /// The generation counter of the captured VM process.
    #[mesh(11)]
    pub capture_generation: u32,
}

impl SnapshotTimeContract {
    /// Returns the capture record: the capture anchor and the capture host's
    /// identities (`E_MANIFEST_TIME` if an identity or the clock is
    /// malformed).
    pub fn capture_record(&self) -> Result<CaptureTimeRecord, TimeAbiError> {
        let identity = |bytes: &[u8], description: &str| {
            <[u8; 16]>::try_from(bytes).map_err(|_| {
                TimeAbiError::new(
                    TimeAbiCode::ManifestTime,
                    format!("{description} is {} bytes, not 16", bytes.len()),
                )
            })
        };
        Ok(CaptureTimeRecord {
            tsc: self.capture_tsc,
            sample: HostTimeSample {
                utc_ns: self.capture_utc_ns,
                monotonic_ns: self.capture_monotonic_ns,
            },
            identity: HostIdentity {
                host_id: identity(&self.host_id, "host identity")?,
                boot_id: identity(&self.host_boot_id, "host boot identity")?,
                clock: HostClockKind::from_manifest(&self.host_clock).ok_or_else(|| {
                    TimeAbiError::new(
                        TimeAbiCode::ManifestTime,
                        format!("host clock '{}' is unknown", self.host_clock),
                    )
                })?,
            },
        })
    }
}

/// The manifest's CPU profile record.
///
/// Capture writes it from constants and a cheap binary encoding, and restore
/// checks it by comparison, so neither encodes a document nor computes a
/// digest: `sha256` and `profile` are the pinned profile's precomputed digest
/// and canonical encoding, and `effective_cpuid` is
/// [`encode_effective_cpuid`] of the partition's effective CPUID. A host
/// profile (`--cpu-profile host`) has no constants: capture records the
/// digest and encoding computed when the VM worker selected it, and restore
/// decodes the document and checks it against the digest.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotCpuProfile {
    /// The profile ID.
    #[mesh(1)]
    pub id: String,
    /// The 32-byte profile digest.
    #[mesh(2)]
    pub sha256: Vec<u8>,
    /// The canonical profile encoding.
    #[mesh(3)]
    pub profile: Vec<u8>,
    /// The effective guest CPUID, as [`encode_effective_cpuid`] writes it.
    #[mesh(4)]
    pub effective_cpuid: Vec<u8>,
    /// CPUID.1:EAX of the capture host, for diagnostics.
    #[mesh(6)]
    pub capture_cpu_signature: u32,
}

/// The size of one entry of [`SnapshotCpuProfile::effective_cpuid`].
pub const EFFECTIVE_CPUID_ENTRY_BYTES: usize = 44;

/// The most entries [`SnapshotCpuProfile::effective_cpuid`] may hold.
pub const MAX_EFFECTIVE_CPUID_ENTRIES: usize = 1024;

/// One entry of the effective CPUID record: a CPUID result and the mask of
/// the bits it defines, for one leaf and subleaf, or for every subleaf of the
/// leaf when `index` is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveCpuidEntry {
    /// The leaf.
    pub function: u32,
    /// The subleaf, or `None` for every subleaf.
    pub index: Option<u32>,
    /// `EAX`, `EBX`, `ECX`, and `EDX`.
    pub result: [u32; 4],
    /// The bits of each register that `result` defines.
    pub mask: [u32; 4],
}

/// Encodes the effective CPUID record: for each entry, in order, eleven
/// little-endian `u32` values, namely the leaf, 1 and the subleaf (or 0 and 0
/// for every subleaf), the four registers, and their four masks.
pub fn encode_effective_cpuid(entries: impl IntoIterator<Item = EffectiveCpuidEntry>) -> Vec<u8> {
    let entries = entries.into_iter();
    let mut out = Vec::with_capacity(entries.size_hint().0 * EFFECTIVE_CPUID_ENTRY_BYTES);
    for entry in entries {
        let (has_index, index) = match entry.index {
            Some(index) => (1, index),
            None => (0, 0),
        };
        for value in [entry.function, has_index, index]
            .into_iter()
            .chain(entry.result)
            .chain(entry.mask)
        {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// Decodes an effective CPUID record that [`encode_effective_cpuid`] wrote,
/// with between 1 and [`MAX_EFFECTIVE_CPUID_ENTRIES`] entries, or describes
/// why it is malformed.
pub fn decode_effective_cpuid(bytes: &[u8]) -> Result<Vec<EffectiveCpuidEntry>, String> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(EFFECTIVE_CPUID_ENTRY_BYTES) {
        return Err(format!(
            "{} bytes is not a whole number of {EFFECTIVE_CPUID_ENTRY_BYTES}-byte entries",
            bytes.len()
        ));
    }
    let count = bytes.len() / EFFECTIVE_CPUID_ENTRY_BYTES;
    if count > MAX_EFFECTIVE_CPUID_ENTRIES {
        return Err(format!(
            "{count} entries is more than {MAX_EFFECTIVE_CPUID_ENTRIES}"
        ));
    }
    bytes
        .as_chunks::<EFFECTIVE_CPUID_ENTRY_BYTES>()
        .0
        .iter()
        .map(|entry| {
            let word = |i: usize| u32::from_le_bytes(entry[4 * i..4 * i + 4].try_into().unwrap());
            let index = match (word(1), word(2)) {
                (1, index) => Some(index),
                (0, 0) => None,
                _ => {
                    return Err(format!(
                        "the entry for leaf {:#x} has an invalid subleaf",
                        word(0)
                    ));
                }
            };
            Ok(EffectiveCpuidEntry {
                function: word(0),
                index,
                result: [word(3), word(4), word(5), word(6)],
                mask: [word(7), word(8), word(9), word(10)],
            })
        })
        .collect()
}

/// The time ABI records the worker returns at capture.
#[derive(Debug, MeshPayload)]
pub struct TimeCapture {
    /// The time contract.
    pub time: SnapshotTimeContract,
    /// The CPU profile record.
    pub cpu_profile: SnapshotCpuProfile,
}

/// The time ABI inputs of a restoring worker, validated by the controller.
#[derive(Debug, MeshPayload)]
pub struct RestoreTimeInput {
    /// The snapshot's time contract.
    pub contract: SnapshotTimeContract,
    /// The snapshot's CPU profile record.
    pub cpu_profile: SnapshotCpuProfile,
    /// The destination host identity used by the controller's downtime
    /// preflight.
    pub destination: HostIdentity,
    /// Receives the sealed time fields of the restore packet.
    pub restore_record: mesh::OneshotSender<RestoreTimeRecord>,
    /// With profiling enabled, notified when the guest first selects the
    /// restore packet; it divides the guest's resume from its repair in the
    /// restore profile.
    pub packet_selected: Option<mesh::OneshotReceiver<()>>,
}

/// Time ABI parameters of every microVM worker.
#[derive(Debug, MeshPayload)]
pub struct TimeAbiParameters {
    /// The CPU profile: a pinned profile ID, `auto`, or `host`. On restore it
    /// is the snapshot's profile ID.
    pub cpu_profile: String,
    /// The generation counter of this VM process.
    pub generation: u32,
    /// The active test hooks.
    pub hooks: TimeAbiTestHooks,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_cpuid_records_round_trip() {
        let entries = [
            EffectiveCpuidEntry {
                function: 0,
                index: None,
                result: [0x16, 1, 2, 3],
                mask: [!0; 4],
            },
            EffectiveCpuidEntry {
                function: 0xb,
                index: Some(2),
                result: [0, 0, 2, 7],
                mask: [!0, !0, !0, 0],
            },
        ];
        let bytes = encode_effective_cpuid(entries);
        assert_eq!(bytes.len(), 2 * EFFECTIVE_CPUID_ENTRY_BYTES);
        assert_eq!(&bytes[44..56], &[0xb, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(decode_effective_cpuid(&bytes).unwrap(), entries);
    }

    #[test]
    fn malformed_effective_cpuid_records_fail() {
        let entry = EffectiveCpuidEntry {
            function: 1,
            index: None,
            result: [0; 4],
            mask: [0; 4],
        };
        let bytes = encode_effective_cpuid([entry]);
        assert!(decode_effective_cpuid(&[]).is_err());
        assert!(decode_effective_cpuid(&bytes[1..]).is_err());

        // A subleaf-independent entry must record subleaf 0, and the flag
        // is 0 or 1.
        let mut stray_index = bytes.clone();
        stray_index[8] = 1;
        assert!(decode_effective_cpuid(&stray_index).is_err());
        let mut bad_flag = bytes.clone();
        bad_flag[4] = 2;
        assert!(decode_effective_cpuid(&bad_flag).is_err());

        let full = encode_effective_cpuid(vec![entry; MAX_EFFECTIVE_CPUID_ENTRIES]);
        assert_eq!(
            decode_effective_cpuid(&full).unwrap().len(),
            MAX_EFFECTIVE_CPUID_ENTRIES
        );
        let over = encode_effective_cpuid(vec![entry; MAX_EFFECTIVE_CPUID_ENTRIES + 1]);
        assert!(decode_effective_cpuid(&over).is_err());
    }
}
