// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for microVM chipset devices.

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

/// Scratch handling requested at a microVM snapshot boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub enum MicrovmSnapshotScratchPolicy {
    /// Scratch is unmounted and restore must attach a fresh image.
    Fresh,
    /// Scratch is mounted and must be paired with VM state.
    Paired,
}

/// Worker-local request used to establish the exact post-OUT snapshot boundary.
#[derive(MeshPayload)]
pub struct MicrovmSnapshotBoundaryRequest {
    /// Whether capture must pair the current scratch image.
    pub scratch_policy: MicrovmSnapshotScratchPolicy,
    /// Signals the device after all vCPUs have stopped.
    pub release_write: mesh::OneshotSender<()>,
    /// Completes after the deferred PMIO write has completed.
    pub write_completed: mesh::OneshotReceiver<()>,
    /// Maximum time allowed to gate host input before stopping vCPUs.
    pub input_gate_timeout: std::time::Duration,
    /// Completed only after the final transaction outcome is known.
    pub transaction_complete: mesh::rpc::Rpc<(), ()>,
}

/// How the microVM portb device completes an output drain request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub enum MicrovmPortbDrain {
    /// Write and flush the accepted output, keeping the endpoint open, before
    /// a snapshot capture that may still roll back.
    Flush,
    /// Write and flush the accepted output, then close the endpoint so a host
    /// relay sees EOF, before the VM process exits.
    Close,
}

/// The microVM bidirectional portb console at ports `0xe9` and `0xea`, with
/// the NVX time ABI time-sample window at `0xeb`.
#[derive(MeshPayload)]
pub struct MicrovmPortbHandle {
    /// Host serial endpoint used for raw input and output.
    pub io: Resource<SerialBackendHandle>,
    /// Fresh generation ID for this microVM instance.
    pub generation_id: [u8; 16],
    /// Requests to deliver accepted output before a snapshot capture or before
    /// terminating the VM process.
    pub output_drain: Option<mesh::Receiver<mesh::rpc::FailableRpc<MicrovmPortbDrain, ()>>>,
    /// NVX time ABI v1 configuration: restore packet version 4 and the time
    /// samples at port `0xeb`.
    pub time_abi: MicrovmPortbTimeAbi,
}

/// NVX time ABI v1 configuration of the portb device.
#[derive(MeshPayload)]
pub struct MicrovmPortbTimeAbi {
    /// The generation counter of this VM process.
    pub generation: u32,
    /// Test hook: milliseconds added to every host UTC reading.
    pub utc_offset_ms: i64,
    /// Test hook: microseconds of delay before latching a UTC reading.
    pub sample_delay_us: u32,
    /// Whether a test hook is active.
    pub test_hooks: bool,
    /// The restore packet of a restored VM process.
    pub restore: Option<MicrovmRestorePacketSource>,
}

/// The parts of a restore packet version 4.
#[derive(MeshPayload)]
pub struct MicrovmRestorePacketSource {
    /// The fields the controller knows before the worker starts.
    pub base: crate::microvm_time::RestorePacketBase,
    /// The time fields the worker seals before the first restored VP runs.
    pub time: mesh::OneshotReceiver<crate::microvm_time::RestoreTimeRecord>,
    /// Notified when the guest first selects the packet, which ends the
    /// guest-resume phase of the restore profile.
    pub selected: Option<mesh::OneshotSender<()>>,
}

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmPortbHandle {
    const ID: &'static str = "microvm-portb";
}

/// microVM shutdown control port at `0x604`.
#[derive(MeshPayload)]
pub struct MicrovmShutdownHandle;

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmShutdownHandle {
    const ID: &'static str = "microvm-shutdown";
}

/// microVM snapshot-request port at `0x605`.
#[derive(MeshPayload)]
pub struct MicrovmSnapshotRequestHandle {
    /// Optional worker-local boundary coordination target.
    pub notify: Option<mesh::Sender<MicrovmSnapshotBoundaryRequest>>,
    /// Maximum time allowed to gate host input before stopping vCPUs.
    pub input_gate_timeout: std::time::Duration,
}

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmSnapshotRequestHandle {
    const ID: &'static str = "microvm-snapshot-request";
}
