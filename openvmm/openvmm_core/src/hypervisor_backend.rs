// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor backend resolution infrastructure.
//!
//! A [`ResolvedHypervisorBackend`] wraps any concrete [`virt::Hypervisor`]
//! implementation into an erased form that the VM worker can use to create
//! a VM.
//!
//! Resource resolvers construct a `ResolvedHypervisorBackend` via
//! [`ResolvedHypervisorBackend::new`], passing in the concrete hypervisor
//! instance (e.g. `virt_kvm::Kvm`).

use crate::partition::HvlitePartition;
use crate::worker::dispatch::InitializedVm;
use crate::worker::dispatch::Manifest;
use futures::future::BoxFuture;
use hypervisor_resources::HypervisorKind;
use membacking::SharedMemoryBacking;
use vm_resource::CanResolveTo;
use vmcore::vm_task::VmTaskDriverSource;

/// Marker trait for [`virt::Hypervisor`] implementations that are compatible
/// with `openvmm_core`.
///
/// A blanket impl is provided for any [`virt::Hypervisor`] whose partition
/// type satisfies `openvmm_core`'s requirements. This trait exists to
/// provide a single, clean bound for [`ResolvedHypervisorBackend::new`].
pub trait HypervisorBackend:
    virt::Hypervisor<Partition: 'static + HvlitePartition> + Send + 'static
where
    for<'a> Self::ProtoPartition<'a>: Send,
{
}

impl<H> HypervisorBackend for H
where
    H: virt::Hypervisor + Send + 'static,
    H::Partition: 'static + HvlitePartition,
    for<'a> H::ProtoPartition<'a>: Send,
{
}

// ---- ResolvedHypervisorBackend ----

/// Type alias for the VM creation function stored inside
/// [`ResolvedHypervisorBackend`].
pub(crate) type CreateVmFn = Box<
    dyn FnOnce(
            VmTaskDriverSource,
            Manifest,
            Option<SharedMemoryBacking>,
        ) -> BoxFuture<'static, anyhow::Result<InitializedVm>>
        + Send,
>;

/// The resolved output of a `Resource<HypervisorKind>`.
///
/// Wraps an erased hypervisor instance. Construct via [`Self::new`].
pub struct ResolvedHypervisorBackend(pub(crate) CreateVmFn);

impl ResolvedHypervisorBackend {
    /// Wraps a [`virt::Hypervisor`] into a resolved backend.
    ///
    /// Only compiles for hypervisors whose partition type is compatible
    /// with `openvmm_core`.
    pub fn new<H>(hypervisor: H) -> Self
    where
        H: HypervisorBackend,
        for<'a> H::ProtoPartition<'a>: Send,
    {
        Self(Box::new(move |driver_source, cfg, shared_memory| {
            Box::pin(async move {
                let mut hv = hypervisor;
                let platform_info = virt::Hypervisor::platform_info(&hv);
                InitializedVm::new_with_hypervisor(
                    driver_source,
                    &mut hv,
                    platform_info,
                    cfg,
                    shared_memory,
                )
                .await
            })
        }))
    }
}

impl CanResolveTo<ResolvedHypervisorBackend> for HypervisorKind {
    type Input<'a> = ();
}

/// Registers hypervisor backend probes for auto-detection.
///
/// Each entry is a unit struct implementing
/// [`HypervisorProbe`](hypervisor_resources::HypervisorProbe).
///
/// Probes are checked in registration order when auto-detecting the
/// hypervisor, so register them from highest to lowest priority.
///
/// Resource resolvers should be registered separately via
/// [`vm_resource::register_static_resolvers!`].
///
/// # Example
///
/// ```ignore
/// openvmm_core::register_hypervisor_probes! {
///     #[cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]
///     openvmm_hypervisors::kvm::KvmProbe,
/// }
/// ```
#[macro_export]
macro_rules! register_hypervisor_probes {
    {} => {};
    { $( $(#[$a:meta])* $probe:path ),+ $(,)? } => {
        $(
        $(#[$a])*
        const _: () = {
            static PROBE_INSTANCE: $probe = $probe;

            #[hypervisor_resources::private::linkme::distributed_slice(
                hypervisor_resources::private::HYPERVISOR_PROBES
            )]
            #[linkme(crate = hypervisor_resources::private::linkme)]
            static PROBE: Option<&'static dyn hypervisor_resources::HypervisorProbe> =
                Some(&PROBE_INSTANCE);
        };
        )*
    };
}
