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
    /// The canonical encoding of the effective guest CPUID.
    #[mesh(4)]
    pub effective_cpuid: Vec<u8>,
    /// The 32-byte digest of `effective_cpuid`.
    #[mesh(5)]
    pub effective_cpuid_sha256: Vec<u8>,
    /// CPUID.1:EAX of the capture host, for diagnostics.
    #[mesh(6)]
    pub capture_cpu_signature: u32,
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
}

/// Time ABI parameters of every microVM worker.
#[derive(Debug, MeshPayload)]
pub struct TimeAbiParameters {
    /// The CPU profile: a pinned profile ID or `auto`. On restore it must
    /// match the snapshot's profile.
    pub cpu_profile: String,
    /// The generation counter of this VM process.
    pub generation: u32,
    /// The active test hooks.
    pub hooks: TimeAbiTestHooks,
}
