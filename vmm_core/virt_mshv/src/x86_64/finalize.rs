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

/// Partition state that exists only after memory finalization.
pub(crate) struct MshvFinalizedPartition {
    pub(super) bsp_vcpufd: VcpuFd,
    pub(super) caps: virt::PartitionCapabilities,
}

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
            let result = self.vmfd.create_vcpu(0);
            tracing::info!(
                elapsed_us = started.elapsed().as_micros() as u64,
                success = result.is_ok(),
                "MSHV_CREATE_VCPU completed"
            );
            let bsp_vcpufd = result.map_err(|e| ErrorInner::CreateVcpu(e.into()))?;
            let caps = self.build_caps(&bsp_vcpufd)?;
            Ok(MshvFinalizedPartition { bsp_vcpufd, caps })
        })
    }

    fn build_caps(&self, bsp: &VcpuFd) -> Result<virt::PartitionCapabilities, Error> {
        let mut cpuid_error = None;
        let cpuid_caps = virt::PartitionCapabilities::from_cpuid(
            &self.config.processor_topology,
            &mut |function, index| {
                bsp.get_cpuid_values(function, index, 0, 0)
                    .unwrap_or_else(|error| {
                        cpuid_error.get_or_insert(error);
                        [0; 4]
                    })
            },
        );
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
