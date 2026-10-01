// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The time ABI primitives a backend provides.

use super::HostTimeSample;
use super::TimeAbiError;
use crate::CpuidLeaf;
use vm_topology::processor::VpIndex;

/// Time ABI primitives of a backend partition, returned by
/// [`Partition::time_abi`](crate::Partition::time_abi) when the partition
/// was built with [`TimeAbiConfig`](super::TimeAbiConfig).
///
/// Every method is called with all VPs stopped. No method falls back to
/// another mechanism: a backend that cannot meet an obligation returns the
/// failure code the specification names.
pub trait TimeAbiBackend: Send + Sync {
    /// Returns the native guest TSC rate `F_d`, in Hz. The guest TSC is never
    /// scaled. Fails with `E_TSC_RATE_UNAVAILABLE`.
    fn native_tsc_hz(&self) -> Result<u64, TimeAbiError>;

    /// Returns the LAPIC timer rate, in Hz: the backend constant. Fails with
    /// `E_LAPIC_RATE_UNAVAILABLE`.
    fn lapic_hz(&self) -> Result<u64, TimeAbiError>;

    /// Verifies, once after the partition is built, that identity routing is
    /// installed, that the synchronized TSC set is available, and that the
    /// guest TSC is not scaled. Fails with `E_IDENTITY_ROUTING`,
    /// `E_TSC_SYNC_UNSUPPORTED`, or `E_TSC_SCALING_ACTIVE`.
    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError>;

    /// Returns the CPUID the backend programmed for VP 0, for the
    /// effective-CPUID record and check. Fails with `E_CPU_SURFACE`.
    fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError>;

    /// Takes the capture anchor: VP 0's TSC paired with
    /// [`sample_host_time`](super::host::sample_host_time). Fails with
    /// `E_TSC_ANCHOR` if the pair cannot be formed within the bound.
    fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError>;

    /// Performs the synchronized TSC set.
    ///
    /// The backend takes the restore anchor, one host instant, and calls
    /// `target` with the host sample of that instant. The orchestrator
    /// selects the downtime there and returns the TSC target, or fails. The
    /// backend then makes every instantiated VP hold that target as of the
    /// anchor and verifies it by read-back before returning
    /// (`E_TSC_SYNC_READBACK`). Creating a VP afterwards is an error
    /// (`E_VP_LATE_CREATION`).
    fn set_synchronized_tsc(
        &self,
        target: &mut dyn FnMut(&HostTimeSample) -> Result<u64, TimeAbiError>,
    ) -> Result<TscSetReport, TimeAbiError>;

    /// Releases partition time immediately before the restored VPs first run.
    ///
    /// A backend whose synchronized set leaves partition time frozen resumes
    /// it here, unless the first VP run resumes it. The default does
    /// nothing.
    fn release_time(&self) -> Result<(), TimeAbiError> {
        Ok(())
    }
}

/// How a backend delivers the identity MSR range to [`super::TimeAbiMsrs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMsrRoute {
    /// Every access in the range exits to OpenVMM.
    ExitToVmm,
    /// The hypervisor serves the range natively with values verified equal
    /// to the time ABI's (TBD(mshv)).
    NativeVerified,
}

/// How a backend performs the synchronized TSC set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TscSyncMethod {
    /// One TSC offset for every VP, computed from one host TSC read (KVM).
    CommonOffset,
    /// Partition time is frozen, the target is written to every VP and read
    /// back while frozen, and time resumes at release (MSHV, WHP).
    FrozenWrite,
}

/// The result of [`TimeAbiBackend::preflight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendPreflight {
    /// The identity MSR route.
    pub msr_route: IdentityMsrRoute,
    /// The synchronized TSC set method.
    pub sync: TscSyncMethod,
}

/// The capture anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TscAnchor {
    /// `T_c`, VP 0's TSC.
    pub tsc: u64,
    /// The host sample paired with `tsc`.
    pub sample: HostTimeSample,
    /// The pairing uncertainty: the most the host instant of `sample` can
    /// differ from the instant `tsc` was read, in nanoseconds.
    pub pairing_ns: u64,
}

/// The result of [`TimeAbiBackend::set_synchronized_tsc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TscSetReport {
    /// The TSC target every VP holds as of the anchor.
    pub target: u64,
    /// The host sample of the restore anchor.
    pub sample: HostTimeSample,
    /// The read-back evidence, one entry per instantiated VP: the TSC value,
    /// or the offset for [`TscSyncMethod::CommonOffset`].
    pub readback: Vec<(VpIndex, u64)>,
    /// The method used.
    pub method: TscSyncMethod,
}
