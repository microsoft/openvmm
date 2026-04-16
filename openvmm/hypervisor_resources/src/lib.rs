// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource types and probe infrastructure for hypervisor backends.
//!
//! This crate defines [`HypervisorKind`] (the resource kind for hypervisor
//! backends), per-backend handle types, and the [`HypervisorProbe`] trait +
//! distributed slice used for auto-detection.
//!
//! Backends register probes via the `register_hypervisor_probes!` macro in
//! `openvmm_core`. Callers use [`probes()`] to iterate registered backends
//! and [`probe_by_name()`] to look up a specific one.

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;

/// Resource kind for hypervisor backends.
///
/// A [`Resource<HypervisorKind>`] identifies which hypervisor backend to use
/// and can carry backend-specific initialization data.
pub enum HypervisorKind {}

impl ResourceKind for HypervisorKind {
    const NAME: &'static str = "hypervisor";
}

/// Handle for the KVM hypervisor backend.
///
/// Contains the open `/dev/kvm` file descriptor so that it can be probed
/// early and reused when creating the partition.
#[derive(MeshPayload)]
pub struct KvmHandle {
    /// An open `/dev/kvm` file descriptor, open with read and write
    /// permissions.
    pub kvm: std::fs::File,
}

impl ResourceId<HypervisorKind> for KvmHandle {
    const ID: &'static str = "kvm";
}

/// Handle for the MSHV hypervisor backend.
#[derive(MeshPayload)]
pub struct MshvHandle;

impl ResourceId<HypervisorKind> for MshvHandle {
    const ID: &'static str = "mshv";
}

/// Handle for the WHP hypervisor backend.
#[derive(MeshPayload)]
pub struct WhpHandle;

impl ResourceId<HypervisorKind> for WhpHandle {
    const ID: &'static str = "whp";
}

/// Handle for the HVF hypervisor backend.
#[derive(MeshPayload)]
pub struct HvfHandle;

impl ResourceId<HypervisorKind> for HvfHandle {
    const ID: &'static str = "hvf";
}

/// Trait for probing hypervisor backend availability.
///
/// Each registered backend provides a probe that can check whether the
/// backend is available and construct a resource for it.
pub trait HypervisorProbe: Send + Sync + 'static {
    /// Short name (e.g. "kvm", "whp"). Matches the handle's `ResourceId::ID`.
    fn name(&self) -> &str;

    /// Checks whether this backend is available and, if so, returns a new
    /// [`Resource<HypervisorKind>`] for it.
    ///
    /// Returns `Ok(None)` if the backend is not available on this system.
    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>>;
}

/// Private module for linkme infrastructure.
#[doc(hidden)]
pub mod private {
    // UNSAFETY: Needed for linkme.
    #![expect(unsafe_code)]

    pub use linkme;

    use super::HypervisorProbe;

    // Use Option<&X> in case the linker inserts some stray nulls, as we
    // think it might on Windows.
    //
    // See <https://devblogs.microsoft.com/oldnewthing/20181108-00/?p=100165>.
    #[linkme::distributed_slice]
    pub static HYPERVISOR_PROBES: [Option<&'static dyn HypervisorProbe>] = [..];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(HYPERVISOR_PROBES)]
    static WORKAROUND: Option<&'static dyn HypervisorProbe> = None;
}

/// Returns an iterator over all registered hypervisor probes.
///
/// Probes are returned in registration order (highest priority first).
pub fn probes() -> impl Iterator<Item = &'static dyn HypervisorProbe> {
    private::HYPERVISOR_PROBES.iter().flatten().copied()
}

/// Looks up a probe by backend name.
pub fn probe_by_name(name: &str) -> Option<&'static dyn HypervisorProbe> {
    probes().find(|p| p.name() == name)
}
