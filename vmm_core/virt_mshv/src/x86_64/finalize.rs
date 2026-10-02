// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition finalization after guest memory is attached.
//!
//! On x86_64 the BSP is created only after all guest memory is attached. The
//! partition capabilities are then discovered from the BSP, so they are not
//! available before [`MshvPartitionInner::finalize_memory`] completes.

use super::hv1_reference_tsc_page_supported;
use crate::Error;
use crate::ErrorInner;
use crate::MshvPartitionInner;
use inspect::Inspect;
use mshv_ioctls::VcpuFd;
use std::sync::OnceLock;
use virt::ProtoPartitionConfig;
use virt::VpIndex;

/// Partition state that exists only after memory finalization.
pub(crate) struct MshvFinalizedPartition {
    pub(super) bsp_vcpufd: VcpuFd,
    pub(super) caps: virt::PartitionCapabilities,
}

/// The CPUID entries that the partition capabilities read
/// (`X86PartitionCapabilities::from_cpuid`): the maximum and feature leaves,
/// SGX, every architectural XSAVE component, the address sizes and SEV
/// leaves, and the hypervisor leaves. Finalization reads them in one bulk
/// call.
const CAPS_CPUID_ENTRIES: [(u32, u32); 32] = [
    (0x0, 0),
    (0x1, 0),
    (0x7, 0),
    (0x12, 2),
    (0xd, 0),
    (0xd, 1),
    (0xd, 2),
    (0xd, 3),
    (0xd, 4),
    (0xd, 5),
    (0xd, 6),
    (0xd, 7),
    (0xd, 8),
    (0xd, 9),
    (0xd, 10),
    (0xd, 11),
    (0xd, 12),
    (0xd, 13),
    (0xd, 14),
    (0xd, 15),
    (0xd, 16),
    (0xd, 17),
    (0xd, 18),
    (0xd, 19),
    (0x8000_0000, 0),
    (0x8000_0001, 0),
    (0x8000_0008, 0),
    (0x8000_001f, 0),
    (hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION, 0),
    (hvdef::HV_CPUID_FUNCTION_HV_INTERFACE, 0),
    (hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0),
    (hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION, 0),
];

/// Partition creation settings that are needed after the partition is built.
#[derive(Inspect)]
pub(crate) struct CreationConfig {
    pub(super) cpuid: virt::CpuidLeafSet,
    #[inspect(skip)]
    pub(super) processor_topology: vm_topology::processor::ProcessorTopology,
    pub(super) hv_configured: bool,
}

impl CreationConfig {
    pub(super) fn new(cpuid: virt::CpuidLeafSet, config: &ProtoPartitionConfig<'_>) -> Self {
        Self {
            cpuid,
            processor_topology: (*config.processor_topology).clone(),
            hv_configured: config.hv_config.is_some(),
        }
    }
}

fn finalized_partition<T>(finalized: &OnceLock<T>) -> Result<&T, Error> {
    finalized
        .get()
        .ok_or_else(|| ErrorInner::PartitionNotFinalized.into())
}

fn finalize_memory_once<T>(
    finalized: &OnceLock<T>,
    memory_attached: bool,
    initialize: impl FnOnce() -> Result<T, Error>,
) -> Result<(), Error> {
    if finalized.get().is_some() {
        return Err(ErrorInner::PartitionAlreadyFinalized.into());
    }
    if !memory_attached {
        return Err(ErrorInner::GuestMemoryNotAttached.into());
    }

    finalized
        .set(initialize()?)
        .map_err(|_| ErrorInner::PartitionAlreadyFinalized.into())
}

/// Returns the hypervisor maximum CPUID leaf for the SNP hypervisor CPUID
/// overrides.
///
/// This reads the configured CPUID results, since the BSP that could be
/// queried does not exist until memory is finalized.
pub(super) fn snp_native_hv_max_leaf(cpuid: &[virt::CpuidLeaf]) -> u32 {
    cpuid
        .iter()
        .find(|leaf| {
            leaf.function == hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION
                && leaf.index.unwrap_or(0) == 0
        })
        .map(|leaf| leaf.result[0])
        .unwrap_or(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION)
}

impl MshvPartitionInner {
    pub(super) fn finalized(&self) -> Result<&MshvFinalizedPartition, Error> {
        finalized_partition(&self.finalized)
    }

    pub(super) fn caps(&self) -> &virt::PartitionCapabilities {
        &self
            .finalized
            .get()
            .expect("partition memory must be finalized before capability access")
            .caps
    }

    pub(super) fn isolation_type(&self) -> virt::IsolationType {
        if self.isolation.snp().is_some() {
            virt::IsolationType::Snp
        } else {
            virt::IsolationType::None
        }
    }

    /// Creates the BSP and discovers the partition capabilities, once guest
    /// memory is attached.
    pub(super) fn finalize_memory(&self) -> Result<(), Error> {
        let memory_attached = self.memory.lock().ranges.iter().any(Option::is_some);

        finalize_memory_once(&self.finalized, memory_attached, || {
            let _span = tracing::info_span!("mshv create BSP", vp_index = 0).entered();
            let started = std::time::Instant::now();
            let result = self.create_vp(VpIndex::BSP);
            tracing::info!(
                elapsed_us = started.elapsed().as_micros() as u64,
                success = result.is_ok(),
                "MSHV_CREATE_VCPU completed"
            );
            let bsp_vcpufd = result?;
            if let Some(time_abi) = &self.time_abi {
                time_abi.register_cpuid(&self.vmfd, &bsp_vcpufd, &self.config.cpuid)?;
            }
            let caps = self.build_caps(&bsp_vcpufd)?;
            Ok(MshvFinalizedPartition { bsp_vcpufd, caps })
        })
    }

    fn build_caps(&self, bsp: &VcpuFd) -> Result<virt::PartitionCapabilities, Error> {
        // One bulk read serves the entries the capabilities read, instead of
        // one hypercall each (about 15 of them, 9 us apiece on bare metal);
        // any other entry falls back to a single read.
        let prefetched =
            super::time_abi::vp_cpuid_many(bsp, VpIndex::BSP.index(), &CAPS_CPUID_ENTRIES)
                .inspect_err(|error| {
                    tracing::debug!(
                        error = error as &dyn std::error::Error,
                        "MSHV bulk CPUID read failed, reading the capabilities one entry at a time"
                    );
                })
                .unwrap_or_default();
        let mut cpuid_error = None;
        let mut cpuid = |function, index| {
            if let Some(position) = CAPS_CPUID_ENTRIES
                .iter()
                .position(|&entry| entry == (function, index))
                .filter(|&position| position < prefetched.len())
            {
                return prefetched[position];
            }
            bsp.get_cpuid_values(function, index, 0, 0)
                .unwrap_or_else(|error| {
                    cpuid_error.get_or_insert(error);
                    [0; 4]
                })
        };
        // The time ABI identity must not make the partition look like an hv1
        // or KVM-clock guest, so its capabilities ignore the hypervisor range.
        let cpuid_caps = if self.time_abi.is_some() {
            virt::PartitionCapabilities::from_cpuid(
                &self.config.processor_topology,
                &mut virt::time_abi::identity::capabilities_cpuid(&mut cpuid),
            )
        } else {
            virt::PartitionCapabilities::from_cpuid(&self.config.processor_topology, &mut cpuid)
        };
        let mut caps = match (cpuid_caps, cpuid_error) {
            (Ok(caps), None) => caps,
            (result, error) => {
                tracing::warn!(
                    error = error.as_ref().map(|error| error as &dyn std::error::Error),
                    capabilities_error = result
                        .err()
                        .as_ref()
                        .map(|error| error as &dyn std::error::Error),
                    "failed to query CPUID capabilities, falling back to partition properties; some features may be unavailable"
                );
                self.caps_from_properties(bsp)?
            }
        };
        caps.hv1 = self.config.hv_configured;
        caps.hv1_reference_tsc_page = hv1_reference_tsc_page_supported(
            caps.hv1,
            self.isolation_type(),
            caps.hv1_reference_tsc_page,
        );
        caps.tsc_deadline = false;
        caps.xsaves_state_bv_broken = true;
        // Ordinary state access does not freeze the partition clock.
        caps.can_freeze_time = false;
        if self.time_abi.is_some() && caps.hv1 {
            return Err(virt::time_abi::TimeAbiError::new(
                virt::time_abi::TimeAbiCode::IdentityRouting,
                "the time ABI partition capabilities include hv1",
            )
            .into());
        }
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn finalization_requires_attached_memory_and_runs_once() {
        let finalized = OnceLock::new();
        let events = RefCell::new(Vec::new());

        let result = finalize_memory_once(&finalized, false, || {
            events.borrow_mut().push("backend initialized");
            Ok(42)
        });
        assert!(matches!(
            result,
            Err(Error(ErrorInner::GuestMemoryNotAttached))
        ));
        assert!(events.borrow().is_empty());

        events.borrow_mut().push("memory attached");
        finalize_memory_once(&finalized, true, || {
            events.borrow_mut().push("backend initialized");
            Ok(42)
        })
        .unwrap();
        assert_eq!(
            events.into_inner(),
            ["memory attached", "backend initialized"]
        );
        assert_eq!(finalized_partition(&finalized).unwrap(), &42);

        let result = finalize_memory_once(&finalized, true, || Ok(43));
        assert!(matches!(
            result,
            Err(Error(ErrorInner::PartitionAlreadyFinalized))
        ));
        assert_eq!(finalized_partition(&finalized).unwrap(), &42);
    }

    #[test]
    fn finalization_propagates_backend_failure() {
        let finalized = OnceLock::<()>::new();

        let result =
            finalize_memory_once(&finalized, true, || Err(ErrorInner::NotSupported.into()));

        assert!(matches!(result, Err(Error(ErrorInner::NotSupported))));
        assert!(finalized.get().is_none());
    }

    #[test]
    fn finalized_state_rejects_early_access() {
        let finalized = OnceLock::<()>::new();

        assert!(matches!(
            finalized_partition(&finalized),
            Err(Error(ErrorInner::PartitionNotFinalized))
        ));
    }
}
