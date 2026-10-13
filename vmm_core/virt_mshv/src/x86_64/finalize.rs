// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition finalization after guest memory is attached.
//!
//! On x86_64 the BSP is created only after all guest memory is attached. The
//! partition capabilities are then discovered from the BSP, so they are not
//! available before [`MshvPartitionInner::finalize_memory`] completes.
//!
//! For a non-isolated partition, attached memory is already registered with
//! MSHV, so the BSP is created after registration. An SNP partition instead
//! defers registering its pre-launch ranges to the SNP launch, which needs the
//! BSP to build the measured CPUID page and to set the SEV control register.
//! On SNP, the BSP is therefore created after the ranges are recorded but
//! before MSHV registers them.

use super::hv1_reference_tsc_page_supported;
use crate::Error;
use crate::ErrorInner;
use crate::MshvPartitionInner;
use inspect::Inspect;
use mshv_ioctls::VcpuFd;
use parking_lot::Mutex;
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
    finalize_lock: &Mutex<()>,
    memory_attached: bool,
    initialize: impl FnOnce() -> Result<T, Error>,
) -> Result<(), Error> {
    let _guard = finalize_lock.lock();
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
        let memory_attached = self.memory.lock().guest_memory_attached();

        finalize_memory_once(
            &self.finalized,
            &self.finalize_lock,
            memory_attached,
            || {
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
            },
        )
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
        caps.xsaves_state_bv_broken = true;
        caps.can_freeze_time = true;
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn finalization_requires_attached_memory_and_runs_once() {
        let finalized = OnceLock::new();
        let finalize_lock = Mutex::new(());
        let events = RefCell::new(Vec::new());

        let result = finalize_memory_once(&finalized, &finalize_lock, false, || {
            events.borrow_mut().push("backend initialized");
            Ok(42)
        });
        assert!(matches!(
            result,
            Err(Error(ErrorInner::GuestMemoryNotAttached))
        ));
        assert!(events.borrow().is_empty());

        events.borrow_mut().push("memory attached");
        finalize_memory_once(&finalized, &finalize_lock, true, || {
            events.borrow_mut().push("backend initialized");
            Ok(42)
        })
        .unwrap();
        assert_eq!(
            events.into_inner(),
            ["memory attached", "backend initialized"]
        );
        assert_eq!(finalized_partition(&finalized).unwrap(), &42);

        let result = finalize_memory_once(&finalized, &finalize_lock, true, || Ok(43));
        assert!(matches!(
            result,
            Err(Error(ErrorInner::PartitionAlreadyFinalized))
        ));
        assert_eq!(finalized_partition(&finalized).unwrap(), &42);
    }

    #[test]
    fn finalization_propagates_backend_failure() {
        let finalized = OnceLock::<()>::new();
        let finalize_lock = Mutex::new(());

        let result = finalize_memory_once(&finalized, &finalize_lock, true, || {
            Err(ErrorInner::NotSupported.into())
        });

        assert!(matches!(result, Err(Error(ErrorInner::NotSupported))));
        assert!(finalized.get().is_none());
    }

    #[test]
    fn concurrent_finalization_initializes_once() {
        const THREAD_COUNT: usize = 8;

        let finalized = Arc::new(OnceLock::new());
        let finalize_lock = Arc::new(Mutex::new(()));
        let ready = Arc::new(Barrier::new(THREAD_COUNT));
        let initialize_count = Arc::new(AtomicUsize::new(0));
        let threads = (0..THREAD_COUNT)
            .map(|_| {
                let finalized = finalized.clone();
                let finalize_lock = finalize_lock.clone();
                let ready = ready.clone();
                let initialize_count = initialize_count.clone();
                std::thread::spawn(move || {
                    ready.wait();
                    finalize_memory_once(&finalized, &finalize_lock, true, || {
                        initialize_count.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok(42)
                    })
                })
            })
            .collect::<Vec<_>>();

        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(initialize_count.load(Ordering::Relaxed), 1);
        assert_eq!(finalized.get(), Some(&42));
    }

    #[test]
    fn internal_snp_mapping_is_not_guest_memory() {
        let mut memory = crate::MshvMemoryRangeState::default();
        memory.ranges.push(Some(crate::MshvMemoryRange {
            region: Default::default(),
            mapped: false,
            guest_memory: false,
        }));
        assert!(!memory.guest_memory_attached());

        memory.ranges.push(Some(crate::MshvMemoryRange {
            region: Default::default(),
            mapped: false,
            guest_memory: true,
        }));
        assert!(memory.guest_memory_attached());
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
