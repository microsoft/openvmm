// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VFIO-backed PCI device assignment for OpenVMM.
//!
//! This crate implements a `ChipsetDevice` that proxies PCI config space
//! and BAR MMIO accesses to a physical device opened via Linux VFIO. The device
//! appears as a standard PCIe endpoint to the guest. MSI-X table and PBA
//! accesses are intercepted and handled by a software emulator; all other BAR
//! MMIO regions are mapped directly into guest GPA space via a `MemoryMapper`,
//! allowing the guest to access device registers without VM exits. A
//! `MemoryMapper` is required for VFIO device assignment; mapping failures are
//! fatal.

#![cfg(target_os = "linux")]

pub mod manager;
pub mod resolver;

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigByteEnable;
use chipset_device::pci::PciConfigSpace;
use guestmem::MappableGuestMemory;
use guestmem::MemoryMapper;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use pci_core::bar_mapping::BarMappings;
use pci_core::capabilities::PciCapability;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps;
use pci_core::spec::cfg_space;
use pci_core::spec::cfg_space::HeaderType00;
use std::collections::BTreeMap;
use std::ops::Range;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;

/// VFIO BAR region information (offset and size within the device fd).
#[derive(Debug, Clone, Copy, Inspect)]
struct VfioBarInfo {
    /// Offset within the VFIO device fd where this BAR region starts.
    #[inspect(hex)]
    pub vfio_offset: u64,
    /// Size of the BAR region in bytes.
    #[inspect(hex)]
    pub size: u64,
}

/// A direct mapping of a VFIO BAR sub-region into guest GPA space.
///
/// Created during device initialization for each mmappable sub-region
/// of a BAR. The `memory` handle is mapped/unmapped from guest GPA space
/// as the guest enables/disables MMIO via the PCI Command register.
#[derive(Inspect)]
struct BarDirectMap {
    /// Guest memory mapping handle backed by the VFIO device fd.
    #[inspect(skip)]
    memory: Box<dyn MappableGuestMemory>,
    /// BAR index this sub-region belongs to.
    bar_index: u8,
    /// The memory range within the BAR.
    bar_range: MemoryRange,
    /// Whether this sub-region is currently mapped into guest GPA space.
    mapping: Option<MemoryRange>,
}

/// A patch to apply to a DWORD-aligned PCI config space read.
///
/// When reading config space, if the DWORD-aligned offset has an entry in the
/// patch table, the hardware value is blended with the patch:
/// `(hw_value & !mask) | (value & mask)`.
///
/// This allows hiding capabilities (by zeroing their ID), rewriting chain
/// pointers, or injecting fully synthetic capability data.
#[derive(Debug, Clone, Copy, Inspect)]
struct ConfigPatch {
    /// Bitmask selecting which bits to replace. Set bits come from `value`;
    /// clear bits come from hardware.
    #[inspect(hex)]
    mask: u32,
    /// Replacement bits for the masked positions.
    #[inspect(hex)]
    value: u32,
}

/// MSI-X emulation state, discovered from the physical device's capabilities.
#[derive(Inspect)]
struct MsixEmulationState {
    /// Software MSI-X table emulator (handles table entries, PBA,
    /// enable/disable state transitions, and irqfd route management).
    #[inspect(skip)]
    emulator: MsixEmulator,
    /// MSI-X PCI capability handler (shared state with emulator; used to
    /// forward config space writes so the emulator tracks enable/disable).
    #[inspect(skip)]
    capability: Box<dyn PciCapability>,
    /// Offset of the MSI-X capability in PCI config space.
    #[inspect(hex)]
    cap_offset: u16,
    /// Number of MSI-X vectors.
    vector_count: u16,
    /// BAR index containing the MSI-X table.
    table_bar: u8,
    #[inspect(with = r#"|x| format!("{:#x}-{:#x}", x.start, x.end)"#)]
    table_range: Range<u64>,
    /// BAR index containing the PBA.
    pba_bar: u8,
    #[inspect(with = r#"|x| format!("{:#x}-{:#x}", x.start, x.end)"#)]
    pba_range: Range<u64>,
    /// Whether MSI-X is currently enabled by the guest.
    enabled: bool,
}

/// A PCI device backed by a VFIO device file.
///
/// Config space reads/writes are proxied to the physical device via the VFIO
/// config region. BARs are cached locally so the guest can probe sizes without
/// hitting hardware on every access. MSI-X table and PBA MMIO accesses are
/// intercepted and handled by a software emulator; all other BAR MMIO is
/// proxied to the physical device via pread/pwrite on the VFIO device fd.
#[derive(InspectMut)]
pub(crate) struct VfioAssignedPciDevice {
    /// The PCI address string (e.g., "0000:01:00.0") for diagnostics.
    #[inspect(display)]
    pci_id: String,

    #[inspect(flatten)]
    vfio_device: VfioPciDevice,

    /// BAR masks as read from the physical device (write 0xFFFFFFFF, read back).
    #[inspect(iter_by_index, hex)]
    bar_masks: [u32; 6],

    /// Current BAR values as seen by the guest.
    #[inspect(iter_by_index, hex)]
    bars: [u32; 6],

    /// Low bits of each BAR that encode type/prefetch flags.
    #[inspect(iter_by_index, hex)]
    bar_flags: [u32; 6],

    /// BAR values to restore on reset. For passthrough BARs, these include
    /// the physical addresses from sysfs overlaid on the encoding bits.
    /// For non-passthrough BARs, these are the same as `bar_flags`.
    #[inspect(iter_by_index, hex)]
    bar_reset_defaults: [u32; 6],

    /// Current MMIO-enabled state (from PCI Command register bit 1).
    mmio_enabled: bool,

    /// Offset of the PMCSR register (PM capability offset + 4), if the
    /// device has a PCI Power Management capability. Used to intercept
    /// power state transitions and unmap BARs in non-D0 states.
    #[inspect(hex)]
    pm_csr_offset: Option<u16>,

    /// Whether the device is currently in D0 power state. BARs are only
    /// mapped into guest address space when the device is in D0.
    in_d0: bool,

    /// Decoded BAR mappings when MMIO is enabled.
    active_bars: BarMappings,

    /// Chipset MMIO region controls per BAR — used to register/unregister
    /// the device's BAR address ranges with the chipset so MMIO accesses
    /// are routed to this device.
    #[inspect(skip)]
    bar_mmio_controls: [Option<Box<dyn chipset_device::mmio::ControlMmioIntercept>>; 6],

    /// VFIO region info per BAR for MMIO proxying via pread/pwrite.
    #[inspect(iter_by_index)]
    bar_regions: [Option<VfioBarInfo>; 6],

    /// MSI-X emulation state (None if device has no MSI-X capability).
    msix: Option<MsixEmulationState>,

    /// Whether the device supports VFIO_DEVICE_RESET (cached from device info
    /// flags at init).
    supports_reset: bool,

    /// Direct guest GPA mappings for BAR sub-regions that support mmap.
    /// When MMIO is enabled, these are mapped to guest GPA space for direct
    /// access without VM exits. When MMIO is disabled, they are unmapped.
    #[inspect(iter_by_index)]
    bar_direct_maps: Vec<BarDirectMap>,

    /// Patch-on-read table for config space filtering. Keyed by
    /// DWORD-aligned config space offset. Built at init from capability
    /// parsing; immutable thereafter.
    #[inspect(
        with = "|m| inspect::iter_by_key(m.iter().map(|(k, v)| (format!(\"{k:#06x}\"), v)))"
    )]
    config_patches: BTreeMap<u16, ConfigPatch>,

    /// VFIO binding. Keeps the container/group (legacy) or iommufd/IOAS
    /// (cdev) fds alive and cleans up on drop.
    binding: manager::VfioBinding,
}

#[derive(Inspect)]
struct VfioPciDevice {
    /// The VFIO device, used for config space, BAR MMIO, and MSI-X mapping.
    #[inspect(skip)]
    device: vfio_sys::Device,

    /// Offset into the VFIO device fd where the PCI config region starts.
    #[inspect(hex)]
    config_offset: u64,

    /// Size of the config space region.
    #[inspect(hex)]
    config_size: u64,
}

impl ConfigSpaceRead for VfioPciDevice {
    fn read_config(&self, offset: u16, value: ByteEnabledDwordRead<'_>) -> anyhow::Result<()> {
        let (byte_offset, len) = value.byte_enable().to_byte_offset_len();
        let cfg_offset = (offset as u64) + (byte_offset as u64);
        if cfg_offset + (len as u64) > self.config_size {
            anyhow::bail!(
                "config read offset {offset:#x} (byte offset {byte_offset:#x}, length {len:#x}) out of range"
            );
        }
        let n = self
            .device
            .as_ref()
            .read_at(
                value.into_valid_byte_slice(),
                self.config_offset + cfg_offset,
            )
            .with_context(|| format!("failed to read config at offset {offset:#x}"))?;
        anyhow::ensure!(
            n == len,
            "short config read at offset {offset:#x}: got {n} bytes expected {len}"
        );
        Ok(())
    }
}

impl VfioPciDevice {
    fn write_config(&self, offset: u16, value: ByteEnabledDwordWrite) -> anyhow::Result<()> {
        let (byte_offset, len) = value.byte_enable().to_byte_offset_len();
        let cfg_offset = (offset as u64) + (byte_offset as u64);
        if cfg_offset + (len as u64) > self.config_size {
            anyhow::bail!(
                "config write offset {offset:#x} (byte offset {byte_offset:#x}, length {len:#x}) out of range"
            );
        }
        let n = self
            .device
            .as_ref()
            .write_at(value.as_valid_byte_slice(), self.config_offset + cfg_offset)?;
        anyhow::ensure!(
            n == len,
            "short config write at offset {offset:#x}: wrote {n} bytes expected {len}"
        );
        Ok(())
    }
}

impl VfioAssignedPciDevice {
    /// Create a new VFIO assigned PCI device.
    ///
    /// Reads BAR flags from config space and derives BAR masks from the VFIO
    /// region sizes (avoiding the write-all-ones probe cycle). Discovers MSI-X
    /// capability if present and creates a software emulator for it.
    pub async fn new(
        binding: manager::VfioDeviceBinding,
        pci_id: String,
        register_mmio: &mut (dyn chipset_device::mmio::RegisterMmioIntercept + Send),
        msi_target: &MsiTarget,
        memory_mapper: &dyn MemoryMapper,
        bar_pt: [bool; 6],
    ) -> anyhow::Result<Self> {
        let vfio_device = binding
            .group()
            .open_device(&pci_id)
            .with_context(|| format!("failed to open VFIO device {pci_id}"))?;

        Self::from_device(
            vfio_device,
            manager::VfioBinding::Group(binding),
            pci_id,
            register_mmio,
            msi_target,
            memory_mapper,
            bar_pt,
        )
        .await
    }

    /// Create from a pre-opened VFIO device and a cdev binding.
    pub async fn from_cdev(
        cdev_binding: manager::VfioCdevBinding,
        pci_id: String,
        register_mmio: &mut (dyn chipset_device::mmio::RegisterMmioIntercept + Send),
        msi_target: &MsiTarget,
        memory_mapper: &dyn MemoryMapper,
        bar_pt: [bool; 6],
    ) -> anyhow::Result<Self> {
        let (device, binding) = cdev_binding.into_parts();
        Self::from_device(
            device,
            manager::VfioBinding::Cdev(binding),
            pci_id,
            register_mmio,
            msi_target,
            memory_mapper,
            bar_pt,
        )
        .await
    }

    async fn from_device(
        vfio_device: vfio_sys::Device,
        mut binding: manager::VfioBinding,
        pci_id: String,
        register_mmio: &mut (dyn chipset_device::mmio::RegisterMmioIntercept + Send),
        msi_target: &MsiTarget,
        memory_mapper: &dyn MemoryMapper,
        bar_pt: [bool; 6],
    ) -> anyhow::Result<Self> {
        let config_info = vfio_device
            .region_info(vfio_bindings::bindings::vfio::VFIO_PCI_CONFIG_REGION_INDEX)
            .context("failed to get VFIO config region info")?;

        let vfio_device = VfioPciDevice {
            device: vfio_device,
            config_offset: config_info.offset,
            config_size: config_info.size,
        };

        // Read BAR encoding bits from config space and derive masks from
        // VFIO region sizes. This avoids the standard write-all-ones probe
        // cycle — VFIO already knows the BAR sizes from the host kernel.
        let mut bar_masks = [0u32; 6];
        let mut bar_flags = [0u32; 6];

        let mut bar_regions = [None; 6];
        let mut bar_mmio_controls = [(); 6].map(|_| None);
        let mut bar_mmap_areas: [Vec<_>; 6] = Default::default();
        let mut processed = 0;
        while processed < 6 {
            let i = processed;
            processed += 1;
            let Ok(info) = vfio_device.device.region_info(i as u32) else {
                continue;
            };
            if info.size == 0 {
                continue;
            }

            let mut flags = 0;
            vfio_device.read_config(
                HeaderType00::BAR0.0 + (i as u16) * 4,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut flags),
            )?;
            flags &= 0xf;
            bar_flags[i] = flags;
            let encoded = cfg_space::BarEncodingBits::from(bar_flags[i]);
            if encoded.use_pio() {
                anyhow::bail!("PIO BARs are not supported");
            }
            let is_64bit = encoded.type_64_bit();
            if is_64bit && i == 5 {
                anyhow::bail!("64-bit BAR at index 5 is invalid");
            }

            if !info.size.is_power_of_two() {
                anyhow::bail!("BAR size is not a power of two: {:#x}", info.size);
            }

            // Derive the mask from the VFIO region size. For a BAR of size N
            // (power of 2), the mask is ~(N - 1). Set the type_64_bit flag
            // so that BarMappings::parse correctly merges 64-bit BAR pairs.
            let mask64 = !(info.size - 1);
            bar_masks[i] = (mask64 as u32) | flags;
            if is_64bit {
                bar_masks[i + 1] = (mask64 >> 32) as u32;
                processed += 1;
            }

            bar_regions[i] = Some(VfioBarInfo {
                vfio_offset: info.offset,
                size: info.size,
            });

            bar_mmio_controls[i] = Some(register_mmio.new_io_region(&format!("bar{i}"), info.size));
            bar_mmap_areas[i] = vfio_device
                .device
                .region_mmap_areas(i as u32)
                .with_context(|| format!("failed to query VFIO mmap areas for BAR {i}"))?;
        }

        // Walk both the standard and extended PCI capability chains in a
        // single pass. This discovers MSI-X (for emulation) and PM (for
        // BAR unmap on D-state transitions), and builds the config patch
        // table that hides capabilities the guest shouldn't see.
        let caps = discover_capabilities(&vfio_device, msi_target);
        let msix = caps.msix;
        let pm_csr_offset = caps.pm_csr_offset;
        let config_patches = caps.config_patches;

        // Cache whether the device supports VFIO_DEVICE_RESET so we can skip
        // the ioctl on every VM reset for devices that don't support it.
        let supports_reset = vfio_device
            .device
            .info()
            .map(|info| info.flags.reset())
            .unwrap_or(false);

        // If the device has MSI-X, remove the table and PBA regions from
        // the mmap areas so they remain trap-and-emulate.
        if let Some(msix) = &msix {
            subtract_msix_regions(&mut bar_mmap_areas, msix);
        }

        // Create direct BAR mappings for mmappable regions. Each
        // mmappable sub-region gets a guest memory mapping backed by the
        // VFIO device fd. These are mapped into guest GPA space when the
        // guest enables MMIO, allowing direct hardware access without VM
        // exits. Non-mmappable regions (e.g. MSI-X table/PBA) remain
        // trap-and-emulate.
        //
        // For the iommufd/cdev path, also export each mmappable BAR area as a
        // dmabuf and register it under the area's intrinsic identity (cdev
        // inode + BAR-region file offset). This lets other assigned devices
        // perform peer-to-peer DMA to this BAR via `IOMMU_IOAS_MAP_FILE` — the
        // kernel maps the BAR's physical MMIO through the PCI P2PDMA provider,
        // avoiding the need to pin MMIO pages by host VA (which usually fails).
        // The legacy group/type1 path has no registry, so dmabuf P2P is
        // skipped there and BAR P2P falls back to best-effort VA mapping.
        let dmabuf_registry = binding.dmabuf_registry().cloned();
        let dmabuf_inode = if dmabuf_registry.is_some() && vfio_device.device.supports_dma_buf()? {
            Some(
                vfio_sys::fd_identity(vfio_device.device.as_fd())
                    .context("failed to stat VFIO cdev for dmabuf registration")?,
            )
        } else {
            None
        };
        // Record the cdev inode up front, before registering any BAR dmabufs
        // in the loop below. If device setup fails partway through (e.g. a
        // region mapping error returns early), the binding's `Drop` then
        // deregisters and closes whatever dmabufs were already registered.
        // Deferring this until after the loop would leak those fds into the
        // shared per-IOAS registry on an early return.
        if let Some(inode) = dmabuf_inode {
            binding.set_dmabuf_inode(inode);
        }
        let mut bar_direct_maps = Vec::new();
        for (i, areas) in bar_mmap_areas.iter().enumerate() {
            let Some(region) = &bar_regions[i] else {
                continue;
            };
            for &area in areas {
                let name = format!("vfio-{pci_id}-bar{i}-{area}");
                let (memory, mapped_region) = memory_mapper
                    .new_region(area.len() as usize, name)
                    .with_context(|| {
                    format!("failed to create BAR {i} direct mapping region for {pci_id}")
                })?;
                mapped_region
                    .map(
                        0,
                        &vfio_device.device,
                        region.vfio_offset + area.start(),
                        area.len() as usize,
                        true,
                    )
                    .with_context(|| {
                        format!(
                            "failed to map VFIO BAR {i} region at offset {:#x}",
                            area.start()
                        )
                    })?;
                bar_direct_maps.push(BarDirectMap {
                    memory,
                    bar_index: i as u8,
                    bar_range: area,
                    mapping: None,
                });

                // Export a dmabuf for this BAR area and register it under the
                // same key the DMA target computes at map time: the cdev inode
                // plus the BAR-region file offset (`vfio_offset + area.start`).
                // The dmabuf itself is created from the BAR-relative range.
                if let (Some(registry), Some((st_dev, st_ino))) = (&dmabuf_registry, dmabuf_inode) {
                    match vfio_device
                        .device
                        .export_dma_buf(i as u32, area.start(), area.len())
                    {
                        Ok(dmabuf) => {
                            registry.register(
                                st_dev,
                                st_ino,
                                region.vfio_offset + area.start(),
                                dmabuf,
                            );
                        }
                        // Exporting a BAR dmabuf is best-effort and never
                        // fatal. The common failure is EINVAL when the BAR has
                        // no P2PDMA provider on this platform/topology (the
                        // kernel's `vfio_pci_core_get_dmabuf_phys` calls
                        // `pcim_p2pdma_provider()` and reports EINVAL when it
                        // returns NULL). That just means this area can't be a
                        // peer-to-peer DMA *target* here, so fall back to
                        // best-effort VA mapping; device assignment still works
                        // for everything except P2P into this BAR.
                        Err(e) => {
                            tracing::warn!(
                                error = e.as_ref() as &dyn std::error::Error,
                                pci_id = pci_id.as_str(),
                                bar = i,
                                area = %area,
                                "failed to export BAR dmabuf; P2P DMA to this \
                                 area will fall back to best-effort VA mapping"
                            );
                        }
                    }
                }
            }
        }

        tracing::info!(
            pci_id = pci_id.as_str(),
            ?bar_masks,
            has_msix = msix.is_some(),
            supports_reset,
            "VFIO assigned PCI device initialized"
        );

        // Build initial BAR values. Start from bar_flags (encoding bits
        // only — guaranteed clean). For passthrough BARs, overlay the
        // physical addresses from sysfs.
        let bars = apply_bar_passthrough(&pci_id, &bar_flags, &bar_masks, &bar_pt)?;
        let bar_reset_defaults = bars;

        Ok(Self {
            pci_id,
            vfio_device,
            bar_masks,
            bars,
            bar_flags,
            bar_reset_defaults,
            mmio_enabled: false,
            pm_csr_offset,
            in_d0: true,
            active_bars: BarMappings::default(),
            bar_mmio_controls,
            bar_regions,
            msix,
            supports_reset,
            bar_direct_maps,
            config_patches,
            binding,
        })
    }

    fn read_phys_config(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        if let Err(e) = self.vfio_device.read_config(offset, value.reborrow()) {
            tracelimit::warn_ratelimited!(
                offset,
                error = e.as_ref() as &dyn std::error::Error,
                "VFIO config space read failed"
            );
            value.set(!0);
        }
    }

    fn write_phys_config(&self, offset: u16, value: ByteEnabledDwordWrite) {
        if let Err(e) = self.vfio_device.write_config(offset, value) {
            tracelimit::warn_ratelimited!(
                offset,
                error = e.as_ref() as &dyn std::error::Error,
                "VFIO config space write failed"
            );
        }
    }

    /// Map a BAR + offset to an MsixEmulator offset, if the access falls
    /// within the MSI-X table or PBA region.
    fn msix_emulator_offset(&self, bar: u8, offset: u64) -> Option<u64> {
        let msix = self.msix.as_ref()?;

        // Check MSI-X table region.
        if bar == msix.table_bar && msix.table_range.contains(&offset) {
            // Emulator table starts at offset 0.
            return Some(offset - msix.table_range.start);
        }

        // Check PBA region.
        if bar == msix.pba_bar && msix.pba_range.contains(&offset) {
            // In the emulator, PBA starts right after the table.
            let emu_pba_start = msix.table_range.end - msix.table_range.start;
            return Some(emu_pba_start + (offset - msix.pba_range.start));
        }

        None
    }

    /// Set up irqfd-backed MSI-X interrupt delivery when the guest enables MSI-X.
    ///
    /// Gets an interrupt for each vector and triggers lazy irqfd route
    /// creation by requesting the backing event. Passes the resulting
    /// events to VFIO so the physical device signals them on interrupt.
    fn msix_enable(&mut self) -> anyhow::Result<()> {
        let msix = self.msix.as_ref().expect("msix must be present");
        let count = msix.vector_count;

        // Get an interrupt for each vector and trigger lazy irqfd route
        // creation by requesting the backing event.
        let interrupts: Vec<_> = (0..count)
            .map(|i| msix.emulator.interrupt(i).expect("vector in range"))
            .collect();

        let events: Vec<_> = interrupts
            .iter()
            .map(|int| int.event())
            .collect::<Option<Vec<_>>>()
            .context("failed to allocate irqfd routes for MSI-X vectors")?;

        self.vfio_device
            .device
            .map_msix(0, &events)
            .context("VFIO map_msix failed")?;

        tracing::info!(
            count,
            pci_id = self.pci_id.as_str(),
            "MSI-X enabled: mapped vectors to irqfd routes"
        );
        Ok(())
    }

    /// Tear down VFIO MSI-X eventfd mapping when the guest disables MSI-X.
    ///
    /// Callers must only invoke this when MSI-X is currently enabled (the
    /// kernel returns EINVAL otherwise).
    fn msix_disable(&mut self) {
        if let Err(e) = self.vfio_device.device.unmap_msix() {
            tracing::warn!(
                error = e.as_ref() as &dyn std::error::Error,
                pci_id = self.pci_id.as_str(),
                "VFIO unmap_msix failed"
            );
        }

        tracing::info!(
            pci_id = self.pci_id.as_str(),
            "MSI-X disabled: unmapped vectors"
        );
    }

    /// Re-evaluate BAR mappings against the current BAR register values.
    ///
    /// Diffs the old and new decoded addresses and only unmaps/remaps BARs
    /// whose address actually changed. When MMIO is disabled, all BARs are
    /// treated as unmapped so the diff naturally tears everything down.
    fn update_bar_mappings(&mut self) {
        let new_bars = if self.mmio_enabled && self.in_d0 {
            BarMappings::parse(&self.bars, &self.bar_masks)
        } else {
            BarMappings::default()
        };

        // For each BAR that had a mapping, check if its address changed.
        // Unmap any that moved or disappeared.
        for old in self.active_bars.iter() {
            let new_addr = new_bars.get(old.index);
            if new_addr == Some(old.base_address) {
                continue;
            }
            // Address changed or BAR disappeared — tear down old mapping.
            if let Some(control) = self.bar_mmio_controls[old.index as usize].as_mut() {
                control.unmap();
            }
            for dm in &mut self.bar_direct_maps {
                if dm.bar_index == old.index {
                    dm.memory.unmap_from_guest();
                    dm.mapping = None;
                }
            }
        }

        // For each BAR in the new set, map any that are new or moved.
        for new in new_bars.iter() {
            let old_addr = self.active_bars.get(new.index);
            if old_addr == Some(new.base_address) {
                continue;
            }
            // New or moved — set up mapping.
            self.bar_mmio_controls[new.index as usize]
                .as_mut()
                .expect("BAR MMIO control must be present")
                .map(new.base_address);

            for dm in &mut self.bar_direct_maps {
                if dm.bar_index == new.index {
                    let gpa = new.base_address + dm.bar_range.start();
                    match dm.memory.map_to_guest(gpa, true) {
                        Ok(()) => {
                            dm.mapping = Some(MemoryRange::new(gpa..gpa + dm.bar_range.len()));
                        }
                        Err(e) => {
                            tracelimit::error_ratelimited!(
                                bar = dm.bar_index,
                                gpa,
                                error = &e as &dyn std::error::Error,
                                pci_id = self.pci_id.as_str(),
                                "failed to direct-map BAR region to guest"
                            );
                        }
                    }
                }
            }
        }

        self.active_bars = new_bars;
    }
}

/// Remove MSI-X table and PBA regions from the mmap areas for the
/// corresponding BARs. This ensures those regions are NOT direct-mapped
/// and remain trap-and-emulate so the software MSI-X emulator can
/// intercept accesses.
///
/// Each mmap area that overlaps with the MSI-X table or PBA is split
/// into up to two non-overlapping areas (before and after the excluded
/// region). The exclusion zone is expanded to page boundaries since
/// the resulting areas must be page-aligned for mmap.
fn subtract_msix_regions(bar_mmap_areas: &mut [Vec<MemoryRange>; 6], msix: &MsixEmulationState) {
    let page_size = page_size();

    for (i, area) in bar_mmap_areas.iter_mut().enumerate() {
        let i = i as u8;
        if area.is_empty() || (msix.table_bar != i && msix.pba_bar != i) {
            continue;
        }
        area.sort();
        *area = memory_range::subtract_ranges(
            memory_range::subtract_ranges(
                area.iter().copied(),
                if msix.table_bar == i {
                    Some(MemoryRange::bounding_aligned(
                        msix.table_range.clone(),
                        page_size,
                    ))
                } else {
                    None
                },
            ),
            if msix.pba_bar == i {
                Some(MemoryRange::bounding_aligned(
                    msix.pba_range.clone(),
                    page_size,
                ))
            } else {
                None
            },
        )
        .collect();
    }
}

fn page_size() -> u64 {
    vfio_sys::host_page_size()
}

/// Apply BAR passthrough: validate the `bar_pt` flags against the discovered
/// BAR layout and overlay physical addresses from sysfs.
///
/// Rejects requests for unimplemented BARs (zero mask) and for the upper half
/// of a 64-bit BAR pair (the lower BAR implicitly covers both halves).
fn apply_bar_passthrough(
    pci_id: &str,
    bar_flags: &[u32; 6],
    bar_masks: &[u32; 6],
    bar_pt: &[bool; 6],
) -> anyhow::Result<[u32; 6]> {
    if !bar_pt.iter().any(|&pt| pt) {
        return Ok(*bar_flags);
    }

    // Validate before reading sysfs.
    for i in 0..6 {
        if !bar_pt[i] {
            continue;
        }
        if bar_masks[i] == 0 {
            anyhow::bail!("BAR {i} is not implemented by the device");
        }
        // If the previous BAR is 64-bit, this index is its upper half.
        if i > 0
            && cfg_space::BarEncodingBits::from(bar_flags[i - 1]).type_64_bit()
            && bar_masks[i - 1] != 0
        {
            anyhow::bail!("BAR {i} is the upper half of a 64-bit BAR pair");
        }
    }

    // VFIO config space returns cleared BARs after device reset, so sysfs
    // is the only reliable source of physical addresses.
    let phys = read_physical_bar_addresses(pci_id)?;
    let mut bars = *bar_flags;
    for i in 0..6 {
        if bar_pt[i] {
            let addr = phys[i];
            if addr == 0 {
                anyhow::bail!("BAR {i} passthrough requested but sysfs address is 0");
            }
            let is_64bit = cfg_space::BarEncodingBits::from(bar_flags[i]).type_64_bit();
            if !is_64bit && addr > u32::MAX as u64 {
                anyhow::bail!("BAR {i} is 32-bit but sysfs address {addr:#x} exceeds 4 GB");
            }
            bars[i] = (addr as u32 & !0xf) | bar_flags[i];
            if is_64bit && i + 1 < 6 {
                bars[i + 1] = (addr >> 32) as u32;
            }
            tracing::info!(
                pci_id,
                bar_index = i,
                addr = format_args!("{:#x}", addr),
                "passthrough BAR"
            );
        }
    }
    Ok(bars)
}

/// Read physical BAR base addresses from the host kernel's resource table.
///
/// Parses `/sys/bus/pci/devices/<bdf>/resource` which has one line per
/// PCI resource: `start end flags` in hex. Lines 0–5 correspond to
/// BAR0–BAR5. For 64-bit BARs, the full 64-bit address appears on the
/// line for the lower BAR index; the upper-half line is zero.
///
/// This is necessary because VFIO config space returns cleared BARs after
/// device reset — only the encoding bits (type, prefetchable) survive.
/// The kernel's resource table retains the physical addresses.
fn read_physical_bar_addresses(pci_id: &str) -> anyhow::Result<[u64; 6]> {
    let path = format!("/sys/bus/pci/devices/{pci_id}/resource");
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;

    let mut addresses = [0u64; 6];
    for (i, line) in content.lines().take(6).enumerate() {
        let start_str = line
            .split_whitespace()
            .next()
            .with_context(|| format!("malformed resource line {i} in {path}"))?;
        addresses[i] = start_str
            .strip_prefix("0x")
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .with_context(|| format!("failed to parse BAR{i} address '{start_str}' in {path}"))?;
    }

    Ok(addresses)
}

/// Abstraction over PCI config space reads, allowing the capability
/// discovery logic to be tested without a real VFIO device.
trait ConfigSpaceRead {
    /// Read a partial DWORD from PCI config space at the given DWORD-aligned offset.
    fn read_config(&self, offset: u16, value: ByteEnabledDwordRead<'_>) -> anyhow::Result<()>;
}

/// Results from walking both the standard and extended PCI capability chains.
struct DiscoveredCapabilities {
    /// MSI-X emulation state, if the device has an MSI-X capability.
    msix: Option<MsixEmulationState>,
    /// Offset of the PMCSR register (PM cap offset + 4), if the device has
    /// a Power Management capability.
    pm_csr_offset: Option<u16>,
    /// Config space patch table for filtering capabilities from the guest.
    config_patches: BTreeMap<u16, ConfigPatch>,
}

/// Walk both the standard (0x34+) and extended (0x100+) PCI capability chains
/// to discover capabilities we need to emulate or intercept, and build the
/// config space patch table for filtering.
///
/// Standard cap chain: discovers MSI-X (for software emulation) and PM (for
/// BAR unmap on D-state transitions).
///
/// Extended cap chain: builds patches to hide SR-IOV, ARI, Resizable BAR
/// from the guest.
fn discover_capabilities(
    config: &dyn ConfigSpaceRead,
    msi_target: &MsiTarget,
) -> DiscoveredCapabilities {
    let mut result = DiscoveredCapabilities {
        msix: None,
        pm_csr_offset: None,
        config_patches: BTreeMap::new(),
    };

    // Clear multi-function bit so the device appears as single-function.
    result.config_patches.insert(
        HeaderType00::BIST_HEADER.0,
        ConfigPatch {
            mask: cfg_space::BistHeader::new()
                .with_multi_function(true)
                .into(),
            value: 0,
        },
    );

    // --- Standard capability chain (offsets < 0x100) ---

    let mut cap_ptr_dword = 0;
    if config
        .read_config(
            HeaderType00::RESERVED_CAP_PTR.0,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut cap_ptr_dword),
        )
        .is_err()
    {
        return result;
    }
    let mut cap_ptr = (cap_ptr_dword & 0xFC) as u16; // mask off reserved bits [1:0]
    let mut iterations = 0usize;

    while cap_ptr != 0 {
        // Guard against malformed capability lists (cycles or excessive length).
        const MAX_CAPS: usize = 48;
        if iterations >= MAX_CAPS {
            tracing::warn!("PCI capability list exceeded {MAX_CAPS} entries, aborting walk");
            break;
        }
        iterations += 1;

        let mut header = 0;
        if config
            .read_config(
                cap_ptr,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut header),
            )
            .is_err()
        {
            break;
        }
        let cap_id = (header & 0xFF) as u8;
        let next_ptr = ((header >> 8) & 0xFC) as u16;

        if cap_id == caps::CapabilityId::MSIX.0 && result.msix.is_none() {
            // Message Control is in the upper 16 bits of the first DWORD.
            let msg_ctrl = (header >> 16) as u16;
            let table_count = (msg_ctrl & 0x7FF) + 1;

            // Table Offset/BIR (second DWORD of the capability).
            let mut table_dword = 0;
            if config
                .read_config(
                    cap_ptr + 4,
                    ByteEnabledDwordRead::with_all_bytes_enabled(&mut table_dword),
                )
                .is_err()
            {
                break;
            }
            let table_bir = (table_dword & 0x7) as u8;
            let table_offset = table_dword & !0x7;

            // PBA Offset/BIR (third DWORD of the capability).
            let mut pba_dword = 0;
            if config
                .read_config(
                    cap_ptr + 8,
                    ByteEnabledDwordRead::with_all_bytes_enabled(&mut pba_dword),
                )
                .is_err()
            {
                break;
            }
            let pba_bir = (pba_dword & 0x7) as u8;
            let pba_offset = pba_dword & !0x7;

            let table_size = table_count as u64 * 16; // MSI-X entry size
            // PBA: one bit per vector, rounded up to QWORD boundary.
            let pba_size = (table_count as u64).div_ceil(64) * 8;

            let (emulator, msix_cap) = MsixEmulator::new(table_bir, table_count, msi_target);

            tracing::info!(
                table_count,
                table_bir,
                table_offset,
                pba_bir,
                pba_offset,
                cap_offset = cap_ptr,
                "discovered MSI-X capability"
            );

            result.msix = Some(MsixEmulationState {
                emulator,
                capability: Box::new(msix_cap),
                cap_offset: cap_ptr,
                vector_count: table_count,
                table_bar: table_bir,
                table_range: table_offset as u64..table_offset as u64 + table_size,
                pba_bar: pba_bir,
                pba_range: pba_offset as u64..pba_offset as u64 + pba_size,
                enabled: false,
            });
        } else if cap_id == caps::CapabilityId::POWER_MANAGEMENT.0 && result.pm_csr_offset.is_none()
        {
            let pmcsr_offset = cap_ptr + 4;
            tracing::info!(
                cap_offset = cap_ptr,
                pmcsr_offset,
                "discovered PCI Power Management capability"
            );
            result.pm_csr_offset = Some(pmcsr_offset);
        }

        cap_ptr = next_ptr;
    }

    // --- Extended capability chain (offsets 0x100+) ---

    // Check if extended caps are reachable by probing the first offset.
    let mut ext_cap_header = 0;
    if config
        .read_config(
            caps::EXT_CAP_START,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut ext_cap_header),
        )
        .is_ok()
    {
        let mut offset = caps::EXT_CAP_START;
        let mut iterations = 0usize;

        loop {
            const MAX_EXT_CAPS: usize = 256;
            if iterations >= MAX_EXT_CAPS {
                tracing::warn!(
                    "extended capability list exceeded {MAX_EXT_CAPS} entries, aborting walk"
                );
                break;
            }
            iterations += 1;

            let mut header = 0;
            if config
                .read_config(
                    offset,
                    ByteEnabledDwordRead::with_all_bytes_enabled(&mut header),
                )
                .is_err()
            {
                break;
            }

            if header == 0 {
                break;
            }

            let cap_id = caps::ExtendedCapabilityId((header & 0xFFFF) as u16);
            let cap_next = ((header >> 20) & 0xFFF) as u16;

            tracing::debug!(
                ?cap_id,
                offset,
                next = cap_next,
                "discovered extended PCI capability"
            );

            match cap_id {
                caps::ExtendedCapabilityId::SRIOV
                | caps::ExtendedCapabilityId::ARI
                | caps::ExtendedCapabilityId::REBAR => {
                    tracing::info!(
                        ?cap_id,
                        offset = format_args!("{offset:#x}"),
                        "filtering extended capability from guest view"
                    );
                    result.config_patches.insert(
                        offset,
                        ConfigPatch {
                            mask: 0x0000_FFFF,
                            value: 0,
                        },
                    );
                }
                _ => {}
            }

            if cap_next == 0 {
                break;
            }

            if cap_next < caps::EXT_CAP_START || cap_next & 0x3 != 0 {
                tracing::warn!(
                    cap_next = format_args!("{cap_next:#x}"),
                    offset = format_args!("{offset:#x}"),
                    "malformed extended capability next pointer, aborting walk"
                );
                break;
            }

            offset = cap_next;
        }
    }

    result
}

/// Read from the MSI-X emulator at the given offset, handling sub-DWORD
/// accesses by aligning to u32 boundaries.
fn read_msix_emulator(emulator: &MsixEmulator, offset: u64, data: &mut [u8]) {
    let aligned = offset & !3;
    let shift = (offset & 3) as usize;
    let val = emulator.read_u32(aligned);
    let bytes = val.to_le_bytes();
    let first_chunk = data.len().min(4 - shift);
    data[..first_chunk].copy_from_slice(&bytes[shift..shift + first_chunk]);

    // Handle reads that span a u32 boundary.
    if first_chunk < data.len() {
        let next_val = emulator.read_u32(aligned + 4);
        let next_bytes = next_val.to_le_bytes();
        let remaining = data.len() - first_chunk;
        data[first_chunk..first_chunk + remaining].copy_from_slice(&next_bytes[..remaining]);
    }
}

/// Write to the MSI-X emulator at the given offset, handling sub-DWORD
/// accesses via read-modify-write.
fn write_msix_emulator(emulator: &mut MsixEmulator, offset: u64, data: &[u8]) {
    let aligned = offset & !3;
    let shift = (offset & 3) as usize;
    let first_chunk = data.len().min(4 - shift);

    if first_chunk == 4 && shift == 0 {
        // Fast path: aligned u32 write.
        let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        emulator.write_u32(aligned, val);
    } else {
        // Read-modify-write for sub-DWORD access.
        let mut current = emulator.read_u32(aligned).to_le_bytes();
        current[shift..shift + first_chunk].copy_from_slice(&data[..first_chunk]);
        emulator.write_u32(aligned, u32::from_le_bytes(current));
    }

    // Handle writes that span a u32 boundary.
    if first_chunk < data.len() {
        let remaining = data.len() - first_chunk;
        let mut next = emulator.read_u32(aligned + 4).to_le_bytes();
        next[..remaining].copy_from_slice(&data[first_chunk..]);
        emulator.write_u32(aligned + 4, u32::from_le_bytes(next));
    }
}

impl ChangeDeviceState for VfioAssignedPciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        // Tear down MSI-X irqfd routes before resetting state.
        if self.msix.as_ref().is_some_and(|m| m.enabled) {
            self.msix_disable();
        }

        self.mmio_enabled = false;
        self.update_bar_mappings();

        // Destructure to ensure every field is explicitly considered for reset.
        let Self {
            ref pci_id,
            ref vfio_device,
            bar_masks: _, // immutable device geometry
            ref mut bars,
            bar_flags: _,
            bar_reset_defaults,
            mmio_enabled: _,  // handled above
            pm_csr_offset: _, // not used during reset
            ref mut in_d0,
            active_bars: _,       // handled by update_bar_mappings()
            bar_mmio_controls: _, // handled by update_bar_mappings()
            bar_direct_maps: _,   // handled by update_bar_mappings()
            bar_regions: _,       // immutable device geometry
            ref mut msix,
            supports_reset,
            config_patches: _, // immutable — built at init
            binding: _,        // lifetime handle — no reset needed
        } = *self;

        // Reset emulated MSI-X table and capability to power-on defaults
        // (all vectors masked, address/data zeroed). The capability and
        // emulator share state via Arc<Mutex>.
        if let Some(msix) = msix {
            msix.enabled = false;
            msix.capability.reset();
        }

        // Reset cached BAR addresses to power-on defaults. For passthrough
        // BARs, this restores the physical addresses so that preserve_bars
        // in the PCI assignment pass will see them after VM reset.
        *bars = bar_reset_defaults;

        // Reset the physical device via VFIO so it starts in a clean state.
        //
        // TODO: handle the case where the physical device does not support reset,
        // or when the reset operation fails.
        if supports_reset {
            if let Err(err) = vfio_device.device.reset() {
                tracing::warn!(
                    pci_id = pci_id.as_str(),
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to reset VFIO device"
                );
            }
            *in_d0 = true;
        }
    }
}

impl ChipsetDevice for VfioAssignedPciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl PciConfigSpace for VfioAssignedPciDevice {
    fn pci_cfg_read(&mut self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        match HeaderType00(offset) {
            // BAR registers: return locally cached values.
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                value.set(self.bars[i]);
            }
            // MSI-X capability first DWORD: merge hardware ID/NextPtr (low
            // 16 bits) with emulator's Message Control (high 16 bits). The
            // emulator tracks the enable/function-mask bits; the hardware
            // provides the correct capability ID and Next Pointer so the
            // capability chain remains intact.
            offset if self.msix.as_ref().is_some_and(|m| offset.0 == m.cap_offset) => {
                let msix = self.msix.as_ref().unwrap();
                // The low word (if targeted) comes from hardware.
                if let Some(v) = value.restrict(PciConfigByteEnable::LOW_WORD) {
                    self.read_phys_config(offset.0, v);
                }
                // The high word (if targeted) comes from the emulator.
                if let Some(v) = value.restrict(PciConfigByteEnable::HIGH_WORD) {
                    msix.capability.read(0, v);
                }
            }
            // Everything else: read from physical device, applying any
            // config space patches.
            _ => {
                self.read_phys_config(offset, value.reborrow());
                if let Some(patch) = self.config_patches.get(&offset) {
                    let hw = value.extract();
                    let patched = (hw & !patch.mask) | (patch.value & patch.mask);
                    tracing::trace!(
                        offset = format_args!("{offset:#x}"),
                        hw = format_args!("{hw:#010x}"),
                        patched = format_args!("{patched:#010x}"),
                        "applied config space patch"
                    );
                    value.set(patched);
                }
            }
        }

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        match HeaderType00(offset) {
            // Command register: track MMIO enable/disable.
            HeaderType00::STATUS_COMMAND => {
                let mse_mask: u32 = cfg_space::Command::new()
                    .with_mmio_enabled(true)
                    .into_bits()
                    .into();

                let mmio_change = if value.valid_mask() & mse_mask != 0 {
                    let command = cfg_space::Command::from_bits(value.extract_low());
                    let new_mmio_enabled = command.mmio_enabled();
                    (new_mmio_enabled != self.mmio_enabled).then_some(new_mmio_enabled)
                } else {
                    None
                };

                match mmio_change {
                    // Enabling MMIO: propagate the Command write to the
                    // physical device *before* mapping BARs into the IOMMU.
                    // Enabling memory space on the real device un-revokes any
                    // exported BAR dmabufs; the IOAS map-by-file P2P import
                    // (IOMMU_IOAS_MAP_FILE) returns ENODEV while a dmabuf is
                    // revoked, so the hardware enable must land first.
                    Some(true) => {
                        self.mmio_enabled = true;
                        self.write_phys_config(offset, value);
                        self.update_bar_mappings();
                    }
                    // Disabling MMIO: tear down the IOMMU mappings while device
                    // memory is still enabled, then propagate the disable to
                    // the physical device.
                    Some(false) => {
                        self.mmio_enabled = false;
                        self.update_bar_mappings();
                        self.write_phys_config(offset, value);
                    }
                    // No MMIO-enable change: just forward the write.
                    None => {
                        self.write_phys_config(offset, value);
                    }
                }

                if let Some(enabled) = mmio_change {
                    tracing::debug!(
                        pci_id = self.pci_id.as_str(),
                        enabled,
                        "MMIO state changed by guest"
                    );
                }
            }
            // BAR registers: mask and cache locally. If MMIO is active,
            // re-evaluate mappings so the device responds at the new address
            // immediately (matching real hardware behavior).
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                let value = value.merge(self.bars[i]);
                self.bars[i] = (value & self.bar_masks[i]) | self.bar_flags[i];

                if self.mmio_enabled {
                    self.update_bar_mappings();
                }
            }
            _ if Some(offset) == self.pm_csr_offset => {
                // Intercept PMCSR writes to track D0/non-D0 transitions. When
                // the device leaves D0, VFIO will invalidate mapped MMIO pages.
                // Unmap them from the guest to avoid generating unresolvable
                // faults on guest access.
                //
                // When returning to D0, write PMCSR first so the faults are
                // resolvable before remapping MMIO into guest space.
                if value.valid_mask() & 0x3 != 0 {
                    let power_state = value.extract() & 0x3; // bits [1:0] = PowerState
                    let new_in_d0 = power_state == 0;
                    let old_in_d0 = self.in_d0;
                    if new_in_d0 {
                        // Entering D0: forward first, then remap BARs.
                        // If the write fails, leave BARs unmapped to
                        // avoid SIGBUS from VFIO mmaps that are still
                        // faulting.
                        if let Err(e) = self.vfio_device.write_config(offset, value) {
                            tracelimit::warn_ratelimited!(
                                offset,
                                error = e.as_ref() as &dyn std::error::Error,
                                "VFIO config space write failed"
                            );
                            return IoResult::Ok;
                        }
                    }
                    self.in_d0 = new_in_d0;
                    self.update_bar_mappings();
                    if !new_in_d0 {
                        // Leaving D0: unmap BARs first, then forward.
                        self.write_phys_config(offset, value);
                    }
                    if new_in_d0 && !old_in_d0 {
                        tracing::debug!(
                            pci_id = self.pci_id.as_str(),
                            power_state,
                            in_d0 = new_in_d0,
                            "PM power state changed by guest"
                        );
                    }
                }
                return IoResult::Ok;
            }
            _ if Some(offset) == self.msix.as_ref().map(|m| m.cap_offset) => {
                // Intercept MSI-X capability writes to track enable/disable
                // state in the software emulator. Do NOT forward the MSI-X
                // control register to hardware via write_phys_config — VFIO
                // manages the hardware MSI-X enable bit internally via
                // VFIO_DEVICE_SET_IRQS. Writing it again through config space
                // causes VFIO to tear down and re-setup MSI-X, losing the
                // eventfd associations.
                const MSIX_ENABLE_MASK: u32 = 0x8000_0000;
                let msix = self.msix.as_mut().unwrap();
                let was_enabled = msix.enabled;
                let new_enabled = if value.valid_mask() & MSIX_ENABLE_MASK != 0 {
                    value.extract() & MSIX_ENABLE_MASK != 0
                } else {
                    was_enabled
                };

                if new_enabled && !was_enabled {
                    // Install irqfd routes BEFORE writing the
                    // capability, so that when the capability
                    // processes the enable transition it can call
                    // enable() on the already-installed routes.
                    match self.msix_enable() {
                        Ok(()) => {
                            let msix = self.msix.as_mut().unwrap();
                            msix.capability.write(0, value);
                            msix.enabled = true;
                        }
                        Err(e) => {
                            tracing::error!(
                                error = e.as_ref() as &dyn std::error::Error,
                                pci_id = self.pci_id.as_str(),
                                "failed to enable MSI-X"
                            );
                        }
                    }
                } else if was_enabled && !new_enabled {
                    // Write capability first to disable vectors,
                    // then tear down VFIO mapping.
                    msix.capability.write(0, value);
                    self.msix_disable();
                    self.msix.as_mut().unwrap().enabled = false;
                } else {
                    // No enable/disable transition — just forward.
                    msix.capability.write(0, value);
                }
                // Skip write_phys_config for MSI-X control register.
                return IoResult::Ok;
            }
            // All other registers: pass through to physical device.
            _ => self.write_phys_config(offset, value),
        }

        IoResult::Ok
    }
}

impl MmioIntercept for VfioAssignedPciDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if let Some((bar, offset)) = self.active_bars.find(addr) {
            // Check if this access falls in the MSI-X table or PBA.
            if let Some(emu_offset) = self.msix_emulator_offset(bar, offset) {
                let msix = self.msix.as_ref().expect("msix must be present");
                read_msix_emulator(&msix.emulator, emu_offset, data);
                return IoResult::Ok;
            }

            // Proxy to physical device BAR via pread.
            if let Some(region) = &self.bar_regions[bar as usize] {
                if offset + data.len() as u64 <= region.size {
                    match self
                        .vfio_device
                        .device
                        .as_ref()
                        .read_at(data, region.vfio_offset + offset)
                    {
                        Ok(n) if n == data.len() => return IoResult::Ok,
                        Ok(n) => {
                            tracelimit::warn_ratelimited!(
                                bar,
                                offset,
                                expected = data.len(),
                                actual = n,
                                "VFIO BAR short read"
                            );
                        }
                        Err(_) => {}
                    }
                }
                tracelimit::warn_ratelimited!(
                    bar,
                    offset,
                    len = data.len(),
                    pci_id = self.pci_id.as_str(),
                    "VFIO BAR read failed or out of range"
                );
            }
        }
        data.fill(!0);
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if let Some((bar, offset)) = self.active_bars.find(addr) {
            // Check if this access falls in the MSI-X table or PBA.
            if let Some(emu_offset) = self.msix_emulator_offset(bar, offset) {
                let msix = self.msix.as_mut().expect("msix must be present");
                write_msix_emulator(&mut msix.emulator, emu_offset, data);
                return IoResult::Ok;
            }

            // Proxy to physical device BAR via pwrite.
            if let Some(region) = &self.bar_regions[bar as usize] {
                if offset + data.len() as u64 <= region.size {
                    match self
                        .vfio_device
                        .device
                        .as_ref()
                        .write_at(data, region.vfio_offset + offset)
                    {
                        Ok(n) if n == data.len() => return IoResult::Ok,
                        Ok(n) => {
                            tracelimit::warn_ratelimited!(
                                bar,
                                offset,
                                expected = data.len(),
                                actual = n,
                                pci_id = self.pci_id.as_str(),
                                "VFIO BAR short write"
                            );
                        }
                        Err(e) => {
                            tracelimit::warn_ratelimited!(
                                bar,
                                offset,
                                error = &e as &dyn std::error::Error,
                                pci_id = self.pci_id.as_str(),
                                "VFIO BAR write failed"
                            );
                        }
                    }
                    return IoResult::Ok;
                }
                tracelimit::warn_ratelimited!(
                    bar,
                    offset,
                    len = data.len(),
                    pci_id = self.pci_id.as_str(),
                    "VFIO BAR write out of range"
                );
            }
        }
        IoResult::Ok
    }
}

impl SaveRestore for VfioAssignedPciDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        // TODO
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pci_core::msi::MsiTarget;
    use test_with_tracing::test;

    /// In-memory config space backing store for unit tests.
    struct MockConfigSpace {
        data: Vec<u8>,
    }

    impl MockConfigSpace {
        /// Create a mock config space of the given size, filled with zeros.
        fn new(size: usize) -> Self {
            Self {
                data: vec![0; size],
            }
        }

        /// Write a DWORD at the given offset (native endian, matching real
        /// VFIO behavior on LE platforms).
        fn write_u32(&mut self, offset: u16, value: u32) {
            let off = offset as usize;
            self.data[off..off + 4].copy_from_slice(&value.to_ne_bytes());
        }

        /// Build a standard capability header DWORD:
        /// bits [7:0] = cap_id, bits [15:8] = next_ptr.
        fn cap_header(cap_id: u8, next_ptr: u8) -> u32 {
            cap_id as u32 | ((next_ptr as u32) << 8)
        }

        /// Build an extended capability header DWORD:
        /// bits [15:0] = cap_id, bits [19:16] = version, bits [31:20] = next_ptr.
        fn ext_cap_header(cap_id: u16, version: u8, next_ptr: u16) -> u32 {
            cap_id as u32 | ((version as u32) << 16) | ((next_ptr as u32) << 20)
        }
    }

    impl ConfigSpaceRead for MockConfigSpace {
        fn read_config(
            &self,
            offset: u16,
            mut value: ByteEnabledDwordRead<'_>,
        ) -> anyhow::Result<()> {
            let off = offset as usize;
            let (byte_offset, len) = value.byte_enable().to_byte_offset_len();
            let cfg_offset = off + byte_offset as usize;
            if cfg_offset + len > self.data.len() {
                anyhow::bail!("config read offset {offset:#x} len {len:#x} out of range");
            }
            value.set(u32::from_ne_bytes(
                self.data[cfg_offset..cfg_offset + 4].try_into().unwrap(),
            ));
            Ok(())
        }
    }

    // --- PM capability discovery tests ---

    #[test]
    fn discover_pm_cap_single() {
        let mut cfg = MockConfigSpace::new(256);
        // Capabilities pointer at 0x34 → 0x40
        cfg.write_u32(0x34, 0x40);
        // PM capability at 0x40: cap_id=0x01, next=0
        cfg.write_u32(0x40, MockConfigSpace::cap_header(0x01, 0x00));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert_eq!(caps.pm_csr_offset, Some(0x44)); // cap_offset + 4
        assert!(caps.msix.is_none());
    }

    #[test]
    fn discover_pm_cap_chained() {
        let mut cfg = MockConfigSpace::new(256);
        // Cap ptr → 0x50
        cfg.write_u32(0x34, 0x50);
        // Vendor-specific at 0x50, next → 0x60
        cfg.write_u32(0x50, MockConfigSpace::cap_header(0x09, 0x60));
        // PM at 0x60, next → 0
        cfg.write_u32(0x60, MockConfigSpace::cap_header(0x01, 0x00));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert_eq!(caps.pm_csr_offset, Some(0x64));
    }

    #[test]
    fn discover_no_capabilities() {
        let cfg = MockConfigSpace::new(256);
        // Cap ptr is 0 → no capabilities

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert_eq!(caps.pm_csr_offset, None);
        assert!(caps.msix.is_none());
        // Should still have the multi-function patch
        assert!(caps.config_patches.contains_key(&0x0C));
    }

    // --- MSI-X capability discovery tests ---

    #[test]
    fn discover_msix_cap() {
        let mut cfg = MockConfigSpace::new(256);
        // Cap ptr → 0x40
        cfg.write_u32(0x34, 0x40);
        // MSI-X at 0x40: cap_id=0x11, next=0, table_size=3 (msg_ctrl bits [10:0] = 2 → 3 vectors)
        let msix_header = MockConfigSpace::cap_header(0x11, 0x00) | (0x0002 << 16); // 3 vectors
        cfg.write_u32(0x40, msix_header);
        // Table offset/BIR: table in BAR 2 at offset 0x1000
        cfg.write_u32(0x44, 0x1000 | 2); // BIR=2, offset=0x1000
        // PBA offset/BIR: PBA in BAR 2 at offset 0x2000
        cfg.write_u32(0x48, 0x2000 | 2); // BIR=2, offset=0x2000

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        let msix = caps.msix.as_ref().expect("MSI-X should be discovered");
        assert_eq!(msix.vector_count, 3);
        assert_eq!(msix.cap_offset, 0x40);
        assert_eq!(msix.table_bar, 2);
        assert_eq!(msix.table_range, 0x1000..0x1000 + 3 * 16);
        assert_eq!(msix.pba_bar, 2);
        assert_eq!(msix.pba_range, 0x2000..0x2008); // 3 vectors → 1 QWORD
    }

    // --- Combined PM + MSI-X discovery ---

    #[test]
    fn discover_pm_and_msix() {
        let mut cfg = MockConfigSpace::new(256);
        // Cap ptr → 0x40
        cfg.write_u32(0x34, 0x40);
        // PM at 0x40, next → 0x60
        cfg.write_u32(0x40, MockConfigSpace::cap_header(0x01, 0x60));
        // MSI-X at 0x60, next → 0, 1 vector (msg_ctrl=0)
        let msix_header = MockConfigSpace::cap_header(0x11, 0x00);
        cfg.write_u32(0x60, msix_header);
        cfg.write_u32(0x64, 0); // table BIR=0, offset=0
        cfg.write_u32(0x68, 0x1000); // PBA BIR=0, offset=0x1000

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert_eq!(caps.pm_csr_offset, Some(0x44));
        let msix = caps.msix.as_ref().expect("MSI-X should be discovered");
        assert_eq!(msix.vector_count, 1);
        assert_eq!(msix.table_bar, 0);
    }

    // --- Extended capability patch tests ---

    #[test]
    fn extended_caps_sriov_filtered() {
        let mut cfg = MockConfigSpace::new(0x200);
        // No standard caps
        cfg.write_u32(0x34, 0x00);
        // Extended cap at 0x100: SR-IOV (0x10), version=1, next=0
        cfg.write_u32(0x100, MockConfigSpace::ext_cap_header(0x10, 1, 0));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        let patch = caps
            .config_patches
            .get(&0x100)
            .expect("SR-IOV should be patched");
        assert_eq!(patch.mask, 0x0000_FFFF);
        assert_eq!(patch.value, 0);
    }

    #[test]
    fn extended_caps_ari_and_rebar_filtered() {
        let mut cfg = MockConfigSpace::new(0x300);
        cfg.write_u32(0x34, 0x00);
        // ARI (0x0E) at 0x100, next → 0x200
        cfg.write_u32(0x100, MockConfigSpace::ext_cap_header(0x0E, 1, 0x200));
        // REBAR (0x15) at 0x200, next → 0
        cfg.write_u32(0x200, MockConfigSpace::ext_cap_header(0x15, 1, 0));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert!(
            caps.config_patches.contains_key(&0x100),
            "ARI should be patched"
        );
        assert!(
            caps.config_patches.contains_key(&0x200),
            "REBAR should be patched"
        );
    }

    // --- Malformed capability chains ---

    #[test]
    fn malformed_cap_chain_cycle_terminates() {
        let mut cfg = MockConfigSpace::new(256);
        // Cap ptr → 0x40
        cfg.write_u32(0x34, 0x40);
        // Cap at 0x40 points to itself
        cfg.write_u32(0x40, MockConfigSpace::cap_header(0x09, 0x40));

        let msi = MsiTarget::disconnected();
        // Should not hang — the 48-iteration limit catches cycles.
        let caps = discover_capabilities(&cfg, &msi);

        // We don't care about the result, just that it terminates.
        assert_eq!(caps.pm_csr_offset, None);
    }

    #[test]
    fn extended_cap_malformed_next_pointer() {
        let mut cfg = MockConfigSpace::new(0x200);
        cfg.write_u32(0x34, 0x00);
        // Extended cap at 0x100 with next pointer below 0x100 (invalid).
        cfg.write_u32(0x100, MockConfigSpace::ext_cap_header(0xFF, 1, 0x50));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        // Should terminate after the first cap without crashing.
        assert!(!caps.config_patches.contains_key(&0x50));
    }

    #[test]
    fn extended_cap_unaligned_next_pointer() {
        let mut cfg = MockConfigSpace::new(0x300);
        cfg.write_u32(0x34, 0x00);
        // Extended cap at 0x100 with unaligned next pointer (0x103).
        // The next pointer field is bits [31:20] = offset >> 0, so 0x103 means
        // we encode it in the header. Since the field is 12 bits representing
        // a DWORD-aligned offset, 0x103 with bit 0 set is malformed.
        cfg.write_u32(0x100, MockConfigSpace::ext_cap_header(0xFF, 1, 0x103));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        // Should stop at the malformed pointer.
        assert_eq!(caps.config_patches.len(), 1); // only multi-function
    }

    #[test]
    fn cap_ptr_reserved_bits_masked() {
        let mut cfg = MockConfigSpace::new(256);
        // Cap ptr = 0x43 — bottom two bits should be masked to get 0x40.
        cfg.write_u32(0x34, 0x43);
        cfg.write_u32(0x40, MockConfigSpace::cap_header(0x01, 0x00));

        let msi = MsiTarget::disconnected();
        let caps = discover_capabilities(&cfg, &msi);

        assert_eq!(caps.pm_csr_offset, Some(0x44));
    }
}
