// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use crate::HardwareIsolatedMemoryProtector;
use crate::MemoryAcceptor;
use crate::mapping::GuestMemoryMapping;
use crate::mapping::GuestMemoryView;
use crate::mapping::GuestMemoryViewReadType;
use crate::mapping::GuestPartitionMemoryView;
use anyhow::Context;
use cvm_tracing::CVM_ALLOWED;
use futures::future::try_join_all;
use guestmem::GuestMemory;
use hcl::GuestVtl;
use hcl::ioctl::MshvHvcall;
use hcl::ioctl::MshvVtlLow;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
use inspect::Inspect;
use memory_range::AlignedSubranges;
use memory_range::MemoryRange;
use pal_async::task::Spawn;
use std::sync::Arc;
use tracing::Instrument;
use underhill_threadpool::AffinitizedThreadpool;
use virt::IsolationType;
use virt_mshv_vtl::ProtectIsolatedMemory;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::processor::ProcessorTopology;

#[derive(Inspect)]
pub struct MemoryMappings {
    vtl0: Arc<GuestMemoryMapping>,
    vtl1: Option<Arc<GuestMemoryMapping>>,
    #[inspect(skip)]
    vtl0_gm: GuestMemory,
    #[inspect(skip)]
    vtl0_kx_gm: GuestMemory,
    #[inspect(skip)]
    vtl0_ux_gm: GuestMemory,
    #[inspect(skip)]
    vtl1_gm: Option<GuestMemory>,
    #[inspect(flatten)]
    cvm_memory: Option<CvmMemory>,
}

#[derive(Inspect)]
/// Mappings, pools, and useful types for memory management that are only
/// available in confidential VMs.
pub struct CvmMemory {
    shared_mapping: Arc<GuestMemoryMapping>,
    #[inspect(skip)]
    pub shared_gm: GuestMemory,
    #[inspect(skip)]
    /// Includes only private VTL0 memory, not pages that have been made shared.
    pub private_vtl0_memory: GuestMemory,
    #[inspect(skip)]
    pub protector: Arc<dyn ProtectIsolatedMemory>,
}

impl MemoryMappings {
    /// Includes all VTL0-accessible memory (private and shared).
    pub fn vtl0(&self) -> &GuestMemory {
        &self.vtl0_gm
    }

    pub fn vtl0_kernel_execute(&self) -> &GuestMemory {
        &self.vtl0_kx_gm
    }

    pub fn vtl0_user_execute(&self) -> &GuestMemory {
        &self.vtl0_ux_gm
    }

    pub fn vtl1(&self) -> Option<&GuestMemory> {
        self.vtl1_gm.as_ref()
    }

    pub fn cvm_memory(&self) -> Option<&CvmMemory> {
        self.cvm_memory.as_ref()
    }
}

pub struct Init<'a> {
    pub processor_topology: &'a ProcessorTopology,
    pub isolation: IsolationType,
    pub vtl0_alias_map_bit: Option<u64>,
    pub vtom: Option<u64>,
    pub mem_layout: &'a MemoryLayout,
    pub complete_memory_layout: &'a MemoryLayout,
    pub boot_init: Option<BootInit<'a>>,
    pub shared_pool: &'a [MemoryRangeWithNode],
    pub maximum_vtl: Vtl,
}

pub struct BootInit<'a> {
    pub tp: &'a AffinitizedThreadpool,
    pub vtl2_memory: &'a [MemoryRangeWithNode],
    pub accepted_regions: &'a [MemoryRange],
}

pub async fn init(params: &Init<'_>) -> anyhow::Result<MemoryMappings> {
    let mut validated_ranges = Vec::new();

    let acceptor = if params.isolation.is_isolated() {
        Some(Arc::new(MemoryAcceptor::new(params.isolation)?))
    } else {
        None
    };

    let hardware_isolated = params.isolation.is_hardware_isolated();

    if let Some(boot_init) = &params.boot_init {
        if !params.isolation.is_isolated() {
            // TODO: VTL 2 protections are applied in the boot shim for isolated
            // VMs. Since non-isolated VMs can undergo servicing and this is an
            // expensive operation, continue to apply protections here for now. In
            // the future, the boot shim should be made aware of when it's booting
            // during a servicing operation and unify the application of vtl2
            // protections.
            tracing::debug!("Applying VTL2 protections");
            apply_vtl2_protections(boot_init.tp, boot_init.vtl2_memory)
                .instrument(tracing::info_span!("apply_vtl2_protections", CVM_ALLOWED))
                .await?;
        } else {
            // Prepare VTL0 memory for mapping.
            let acceptor = acceptor.as_ref().unwrap();
            let ram = params.mem_layout.ram().iter().map(|r| r.range);
            let accepted_ranges = boot_init.accepted_regions.iter().copied();
            // On hardware isolated platforms, accepted memory was accepted with
            // VTL2 only permissions. Provide VTL0 access here.
            tracing::debug!("Applying VTL0 protections");
            if hardware_isolated {
                for range in memory_range::overlapping_ranges(ram.clone(), accepted_ranges.clone())
                {
                    acceptor.apply_initial_lower_vtl_protections(range)?;
                }
            }

            // Accept the memory that was not accepted by the boot loader.
            // FUTURE: do this lazily.
            let vp_count = std::cmp::max(1, params.processor_topology.vp_count() - 1);
            let accept_subrange = move |subrange| {
                acceptor.accept_lower_vtl_pages(subrange).unwrap();
                if hardware_isolated {
                    // For VBS-isolated VMs, the VTL protections are set as
                    // part of the accept call.
                    acceptor
                        .apply_initial_lower_vtl_protections(subrange)
                        .unwrap();
                }
            };
            tracing::debug!("Accepting VTL0 memory");
            std::thread::scope(|scope| {
                for source_range in memory_range::subtract_ranges(ram, accepted_ranges) {
                    validated_ranges.push(source_range);

                    // Chunks must be 2mb aligned
                    let two_mb = 2 * 1024 * 1024;
                    let mut range = source_range.aligned_subrange(two_mb);
                    if !range.is_empty() {
                        let chunk_size = (range.page_count_2m().div_ceil(vp_count as u64)) * two_mb;
                        let chunk_count = range.len().div_ceil(chunk_size);

                        for _ in 0..chunk_count {
                            let subrange;
                            (subrange, range) = if range.len() >= chunk_size {
                                range.split_at_offset(chunk_size)
                            } else {
                                (range, MemoryRange::EMPTY)
                            };
                            scope.spawn(move || accept_subrange(subrange));
                        }
                        assert!(range.is_empty());
                    }

                    // Now accept whatever wasn't aligned on the edges
                    scope.spawn(move || {
                        for unaligned_subrange in memory_range::subtract_ranges(
                            [source_range],
                            [source_range.aligned_subrange(two_mb)],
                        ) {
                            accept_subrange(unaligned_subrange);
                        }
                    });
                }
            });
        }
    }

    // Tell the hypervisor we want to use the shared pool for shared memory.
    //
    // TODO: don't we possibly need to unaccept these pages for SNP? Or are
    // we assuming they were not in the boot loader's pre-accepted pages.
    if let Some(acceptor) = &acceptor {
        tracing::debug!("Making shared pool pages shared");

        for range in params.shared_pool {
            // On VBS, we need to accept the pages first before we move them to
            // shared.
            if params.isolation == IsolationType::Vbs {
                acceptor
                    .accept_lower_vtl_pages(range.range)
                    .context("unable to accept shared pool pages")?;
            }

            acceptor
                .modify_gpa_visibility(
                    hvdef::hypercall::HostVisibilityType::SHARED,
                    &Vec::from_iter(range.range.start_4k_gpn()..range.range.end_4k_gpn()),
                )
                .context("unable to make shared pool pages shared vis")?;
        }
    }

    // Map lower VTL memory.
    let gpa_fd = MshvVtlLow::new().context("failed to open /dev/mshv_vtl_low")?;

    let gm = if hardware_isolated {
        assert!(params.vtl0_alias_map_bit.is_none());
        let vtom = params.vtom.unwrap();

        // Create the encrypted mapping with just the lower VTL memory.
        //
        // Do not register this mapping with the kernel. It will not be safe for
        // use with syscalls that expect virtual addresses to be in
        // kernel-registered RAM.

        tracing::debug!("Building valid encrypted memory view");
        let encrypted_memory_view = {
            let _span = tracing::info_span!("create encrypted memory view", CVM_ALLOWED).entered();
            GuestPartitionMemoryView::new(
                params.mem_layout,
                crate::mapping::GuestValidMemoryType::Encrypted,
                true,
            )?
        };

        tracing::debug!("Building encrypted memory map");
        let encrypted_mapping = Arc::new({
            let _span = tracing::info_span!("map_vtl1_memory", CVM_ALLOWED).entered();
            GuestMemoryMapping::builder(0)
                .dma_base_address(None)
                .build_with_bitmap(&gpa_fd, &encrypted_memory_view)
                .context("failed to map lower vtl encrypted memory")?
        });

        let use_vtl1 = params.maximum_vtl >= Vtl::Vtl1;

        // Start by giving VTL 0 full access to all lower-vtl memory.
        // TODO GUEST VSM: with lazy acceptance, it should instead be initialized to no
        // access.
        tracing::debug!("Building VTL0 memory map");
        let vtl0_mapping = Arc::new({
            let _span = tracing::info_span!("map_vtl0_memory", CVM_ALLOWED).entered();
            GuestMemoryMapping::builder(0)
                .dma_base_address(None)
                .use_permissions_bitmaps(if use_vtl1 { Some(true) } else { None })
                .build_with_bitmap(&gpa_fd, &encrypted_memory_view)
                .context("failed to map vtl0 memory")?
        });

        // Create the shared mapping with the complete memory map, to include
        // the shared pool. This memory is not private to VTL2 and is expected
        // that devices will do DMA to them.
        let shared_offset = match params.isolation {
            IsolationType::Tdx => {
                // Register memory just once, as shared memory. This
                // registration will be used both to map pages as shared and as
                // encrypted. If the kernel remaps a page into a kernel address,
                // it will be marked as shared, which can cause a fault or,
                // worse, an information leak.
                //
                // This is done this way because in TDX, there is only one
                // mapping for each page. The distinguishing bit is a reserved
                // bit, from the kernel's perspective. (You can also just see it
                // as the high bit of the GPA, but the Linux kernel does not
                // treat it that way.)
                //
                // TODO CVM: figure out how to prevent passing encrypted pages
                // to syscalls. Idea: prohibit locking of `GuestMemory` pages
                // for encrypted memory, so that there's no way to get a virtual
                // address. Downside: vmbus ring buffers are currently accessed
                // by locking memory, and this would need to be changed to use
                // some kind of override, or to go through `GuestMemory`
                // accessors, or something.
                0
            }
            IsolationType::Snp => {
                // SNP has two mappings for each shared page: one below and one
                // above VTOM. So, unlike for TDX, for SNP we could choose to
                // register memory twice, allowing the kernel to operate on
                // either shared or encrypted memory. But, for consistency with
                // TDX, just register the shared mapping.
                //
                // Register the VTOM mapping instead of the low mapping. In
                // theory it shouldn't matter; we should be able to ignore VTOM.
                // However, the ioctls to issue pvalidate and rmpadjust
                // instructions operate on VAs, and they must either be VAs
                // mapping unregistered pages or pages that were registered as
                // encrypted. Since we want to avoid registering the pages as
                // encrypted, the lower alias must remain unregistered, and so
                // the shared registration must use the high mapping.
                vtom
            }
            _ => unreachable!(),
        };

        // For TDX, the spec says that the IOMMU _may_ reject DMAs with the
        // shared bit clear, so set it in the IOVAs returned for the shared
        // mapping.
        //
        // For SNP, the hardware doesn't care; VTOM is not known by the IOMMU
        // and the hypervisor includes the VTOM alias in the IOMMU's page
        // tables. Use the VTOM alias for consistency with TDX.
        let dma_base_address = vtom;

        // Create the shared mapping with the complete memory map, to include
        // the shared pool. This memory is not private to VTL2 and is expected
        // that devices will access it via DMA.
        //
        // Don't allow kernel access here either--the kernel seems to get
        // confused about shared memory, and our current use of kernel-mode
        // guest memory access is limited to low-perf paths where we can use
        // bounce buffering.
        tracing::debug!("Building shared memory map");

        let shared_memory_view = {
            let _span = tracing::info_span!("create shared memory view", CVM_ALLOWED).entered();
            GuestPartitionMemoryView::new(
                params.complete_memory_layout,
                crate::mapping::GuestValidMemoryType::Shared,
                false,
            )?
        };

        let valid_shared_memory = shared_memory_view.partition_valid_memory();

        // Update the shared mapping bitmap for pages used by the shared
        // visibility pool to be marked as shared, since by default pages are
        // marked as no-access in the bitmap.
        tracing::debug!("Updating shared mapping bitmaps");
        for range in params.shared_pool {
            valid_shared_memory.as_ref().update_valid(range.range, true);
        }

        let shared_mapping = Arc::new({
            let _span = tracing::info_span!("map_shared_memory", CVM_ALLOWED).entered();
            GuestMemoryMapping::builder(shared_offset)
                .shared(true)
                .ignore_registration_failure(params.boot_init.is_none())
                .dma_base_address(Some(dma_base_address))
                .build_with_bitmap(&gpa_fd, &shared_memory_view)
                .context("failed to map shared memory")?
        });

        let protector = Arc::new(HardwareIsolatedMemoryProtector::new(
            encrypted_memory_view.partition_valid_memory().clone(),
            valid_shared_memory.clone(),
            encrypted_mapping.clone(),
            vtl0_mapping.clone(),
            params.mem_layout.clone(),
            acceptor.as_ref().unwrap().clone(),
        )) as Arc<dyn ProtectIsolatedMemory>;

        tracing::debug!("Creating VTL0 guest memory");
        let vtl0_gm = GuestMemory::new_multi_region(
            "vtl0",
            vtom,
            vec![
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    vtl0_mapping.clone(),
                    GuestMemoryViewReadType::Read,
                    GuestVtl::Vtl0,
                )),
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    shared_mapping.clone(),
                    GuestMemoryViewReadType::Read,
                    GuestVtl::Vtl0,
                )),
            ],
        )
        .context("failed to make vtl0 guest memory")?;

        let (vtl1_mapping, vtl1_gm) = if use_vtl1 {
            tracing::debug!("Creating VTL1 guest memory");
            // For VTL 1, vtl protections are dictated by what VTL 2 thinks is
            // valid lower-vtl memory, and therefore additional vtl protection
            // bitmaps aren't needed for the mapping.
            (
                Some(encrypted_mapping.clone()),
                Some(
                    GuestMemory::new_multi_region(
                        "vtl1",
                        vtom,
                        vec![
                            Some(GuestMemoryView::new(
                                Some(protector.clone()),
                                encrypted_mapping.clone(),
                                GuestMemoryViewReadType::Read,
                                GuestVtl::Vtl1,
                            )),
                            Some(GuestMemoryView::new(
                                Some(protector.clone()),
                                shared_mapping.clone(),
                                GuestMemoryViewReadType::Read,
                                GuestVtl::Vtl1,
                            )),
                        ],
                    )
                    .context("failed to make vtl1 guest memory")?,
                ),
            )
        } else {
            (None, None)
        };

        if params.isolation == IsolationType::Snp {
            // For SNP, zero any newly accepted private lower-vtl memory in case
            // the hypervisor decided to remap VTL 2 memory into lower-VTL GPA
            // space. This is safe to do after the vtl permissions have been
            // applied because the lower VTLs are not running yet.
            //
            // TODO: perform lazily
            let _span =
                tracing::info_span!("zeroing lower vtl memory for SNP", CVM_ALLOWED).entered();

            tracing::debug!("zeroing lower vtl memory for SNP");
            for range in validated_ranges {
                vtl0_gm
                    .fill_at(range.start(), 0, range.len() as usize)
                    .expect("private memory should be valid at this stage");
            }
        }

        // Untrusted devices can only access shared memory, but they can do so
        // from either alias (below and above vtom). This is consistent with
        // what the IOMMU is programmed with.
        tracing::debug!("Creating untrusted shared DMA memory");
        let shared_gm = GuestMemory::new_multi_region(
            "shared",
            vtom,
            vec![
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    shared_mapping.clone(),
                    GuestMemoryViewReadType::Read,
                    GuestVtl::Vtl0,
                )),
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    shared_mapping.clone(),
                    GuestMemoryViewReadType::Read,
                    GuestVtl::Vtl0,
                )),
            ],
        )
        .context("failed to make shared guest memory")?;

        let private_vtl0_memory = GuestMemory::new(
            "trusted",
            GuestMemoryView::new(
                Some(protector.clone()),
                vtl0_mapping.clone(),
                GuestMemoryViewReadType::Read,
                GuestVtl::Vtl0,
            ),
        );

        tracing::debug!("Creating VTL0 guest memory for kernel execute access");
        let vtl0_kx_gm = GuestMemory::new_multi_region(
            "vtl0_kx",
            vtom,
            vec![
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    vtl0_mapping.clone(),
                    GuestMemoryViewReadType::KernelExecute,
                    GuestVtl::Vtl0,
                )),
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    shared_mapping.clone(),
                    GuestMemoryViewReadType::KernelExecute,
                    GuestVtl::Vtl0,
                )),
            ],
        )
        .context("failed to make vtl0 guest memory with kernel execute access")?;

        tracing::debug!("Creating VTL0 guest memory for user execute access");
        let vtl0_ux_gm = GuestMemory::new_multi_region(
            "vtl0_ux",
            vtom,
            vec![
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    vtl0_mapping.clone(),
                    GuestMemoryViewReadType::UserExecute,
                    GuestVtl::Vtl0,
                )),
                Some(GuestMemoryView::new(
                    Some(protector.clone()),
                    shared_mapping.clone(),
                    GuestMemoryViewReadType::UserExecute,
                    GuestVtl::Vtl0,
                )),
            ],
        )
        .context("failed to make vtl0 guest memory with user execute access")?;

        MemoryMappings {
            vtl0: vtl0_mapping,
            vtl1: vtl1_mapping,
            vtl0_gm,
            vtl0_kx_gm,
            vtl0_ux_gm,
            vtl1_gm,
            cvm_memory: Some(CvmMemory {
                shared_gm,
                private_vtl0_memory,
                shared_mapping,
                protector,
            }),
        }
    } else {
        tracing::debug!("Creating VTL0 guest memory");
        let vtl0_mapping = {
            let _span = tracing::info_span!("map_vtl0_memory", CVM_ALLOWED).entered();
            let base_address = params.vtl0_alias_map_bit.unwrap_or(0);

            Arc::new(
                GuestMemoryMapping::builder(base_address)
                    .for_kernel_access(true)
                    .dma_base_address(Some(base_address))
                    .ignore_registration_failure(params.boot_init.is_none())
                    .build_without_bitmap(&gpa_fd, params.mem_layout)
                    .context("failed to map vtl0 memory")?,
            )
        };
        let vtl0_gm = GuestMemory::new(
            "vtl0",
            GuestMemoryView::new(
                None,
                vtl0_mapping.clone(),
                GuestMemoryViewReadType::Read,
                GuestVtl::Vtl0,
            ),
        );

        let vtl1_mapping = if params.maximum_vtl >= Vtl::Vtl1 {
            if params.vtl0_alias_map_bit.is_none() {
                if cfg!(guest_arch = "x86_64") {
                    // Guest VSM cannot be exposed to the guest unless the
                    // alias map is available. Otherwise, Underhill cannot
                    // correctly check for VTL0 access protections. Ideally, UH
                    // would hide Guest VSM from the guest if the alias map is
                    // not available, but the guest secure kernel checks the
                    // access_vsm permission to determine if Guest VSM is
                    // supported, and there is no mechanism for UH to hide that
                    // from the guest. Thus, it is not safe to proceed.
                    anyhow::bail!("cannot safely support VTL 1 without using the alias map");
                } else {
                    // On ARM, the alias map is not exposed: see
                    // underhill_core::init::vtl0_alias_map_bit.
                    tracing::warn!(
                        CVM_ALLOWED,
                        "cannot safely support VTL 1 without using the alias map; Guest VSM not supported"
                    );
                    None
                }
            } else {
                tracing::debug!("Creating VTL 1 memory map");

                let _span = tracing::info_span!("map_vtl1_memory", CVM_ALLOWED).entered();
                Some(Arc::new(
                    GuestMemoryMapping::builder(0)
                        .for_kernel_access(true)
                        .dma_base_address(Some(0))
                        .ignore_registration_failure(params.boot_init.is_none())
                        .build_without_bitmap(&gpa_fd, params.mem_layout)
                        .context("failed to map vtl1 memory")?,
                ))
            }
        } else {
            None
        };

        let vtl1_gm = if let Some(vtl1_mapping) = &vtl1_mapping {
            tracing::info!(CVM_ALLOWED, "VTL 1 memory map created");
            Some(GuestMemory::new(
                "vtl1",
                GuestMemoryView::new(
                    None,
                    vtl1_mapping.clone(),
                    GuestMemoryViewReadType::Read,
                    GuestVtl::Vtl1,
                ),
            ))
        } else {
            tracing::info!(CVM_ALLOWED, "Skipping VTL 1 memory map creation");
            None
        };

        // TODO: make kernel/user execute guest memory objects that use a
        // fallback path to query the hypervisor for the permissions.
        MemoryMappings {
            vtl0: vtl0_mapping,
            vtl1: vtl1_mapping,
            vtl0_gm: vtl0_gm.clone(),
            vtl0_kx_gm: vtl0_gm.clone(),
            vtl0_ux_gm: vtl0_gm.clone(),
            vtl1_gm,
            cvm_memory: None,
        }
    };
    Ok(gm)
}

/// Apply VTL2 protections to all VTL2 ram ranges. This marks all VTL2 pages as
/// no access by lower VTLs.
async fn apply_vtl2_protections(
    threadpool: &AffinitizedThreadpool,
    vtl2_memory: &[MemoryRangeWithNode],
) -> anyhow::Result<()> {
    let mshv_hvcall = Arc::new(MshvHvcall::new().context("failed to open mshv_hvcall device")?);
    mshv_hvcall.set_allowed_hypercalls(&[HypercallCode::HvCallModifyVtlProtectionMask]);

    // Apply VTL2 protections in 2GB units. This is large enough to get large
    // pages in the kernel, but small enough to allow parallelism across most of
    // the VPs.
    const MAX_RANGE_LEN: u64 = 2 << 30;

    let ranges: Vec<_> = vtl2_memory
        .iter()
        .flat_map(|range| AlignedSubranges::new(range.range).with_max_range_len(MAX_RANGE_LEN))
        .collect();

    try_join_all(
        ranges
            .into_iter()
            .zip(threadpool.active_drivers().cycle())
            .map(|(range, driver)| {
                let mshv_hvcall = mshv_hvcall.clone();
                driver.spawn(
                    "apply-vtl2-protections",
                    async move {
                        tracing::debug!(
                            cpu = underhill_threadpool::Thread::current()
                                .unwrap()
                                .with_driver(|driver| driver.target_cpu()),
                            %range,
                            "applying protections"
                        );
                        mshv_hvcall
                            .modify_vtl_protection_mask(
                                range,
                                hvdef::HV_MAP_GPA_PERMISSIONS_NONE,
                                HvInputVtl::CURRENT_VTL,
                            )
                            .with_context(|| {
                                format!("failed to apply vtl2 protections for {range}")
                            })
                    }
                    .in_current_span(),
                )
            }),
    )
    .await?;

    Ok(())
}
