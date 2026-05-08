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
use pci_core::spec::cfg_space;
use pci_core::spec::cfg_space::HeaderType00;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;

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

    /// The VFIO device, used for config space, BAR MMIO, and MSI-X mapping.
    #[inspect(skip)]
    vfio_device: vfio_sys::Device,

    /// Offset into the VFIO device fd where the PCI config region starts.
    #[inspect(hex)]
    config_offset: u64,

    /// Size of the config space region.
    #[inspect(hex)]
    config_size: u64,

    /// BAR masks as read from the physical device (write 0xFFFFFFFF, read back).
    #[inspect(iter_by_index, hex)]
    bar_masks: [u32; 6],

    /// Current BAR values as seen by the guest.
    #[inspect(iter_by_index, hex)]
    bars: [u32; 6],

    /// Low bits of each BAR that encode type/prefetch flags.
    #[inspect(iter_by_index, hex)]
    bar_flags: [u32; 6],

    /// Current MMIO-enabled state (from PCI Command register bit 1).
    mmio_enabled: bool,

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

    /// VFIO container/group binding. Keeps the container and group fds alive
    /// and notifies the container manager on drop.
    binding: manager::VfioDeviceBinding,
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
        driver_source: &VmTaskDriverSource,
        register_mmio: &mut (dyn chipset_device::mmio::RegisterMmioIntercept + Send),
        msi_target: &MsiTarget,
        memory_mapper: &dyn MemoryMapper,
    ) -> anyhow::Result<Self> {
        let driver = driver_source.simple();
        let retry = vfio_sys::VfioRetry::new(&driver, &pci_id);
        let is_enodev = |e: &anyhow::Error| {
            e.chain().any(|cause| {
                cause
                    .downcast_ref::<nix::errno::Errno>()
                    .is_some_and(|e| *e == nix::errno::Errno::ENODEV)
            })
        };
        let vfio_device = retry
            .retry(
                || binding.group().open_device(&pci_id),
                &is_enodev,
                "open_device",
            )
            .await
            .with_context(|| format!("failed to open VFIO device {pci_id}"))?;

        let config_info = vfio_device
            .region_info(vfio_bindings::bindings::vfio::VFIO_PCI_CONFIG_REGION_INDEX)
            .context("failed to get VFIO config region info")?;

        let config_offset = config_info.offset;
        let config_size = config_info.size;

        // Read BAR values and derive masks from VFIO region sizes.
        // This avoids the standard write-all-ones probe cycle — VFIO already
        // knows the BAR sizes from the host kernel.
        let mut bar_masks = [0u32; 6];
        let mut bar_flags = [0u32; 6];

        let mut bars = [0u32; 6];
        for (i, bar) in bars.iter_mut().enumerate() {
            *bar = read_config_u32(
                vfio_device.as_ref(),
                config_offset,
                config_size,
                HeaderType00::BAR0.0 + (i as u16) * 4,
            )?;
        }

        let mut bar_regions = [None; 6];
        let mut bar_mmio_controls = [(); 6].map(|_| None);
        let mut bar_mmap_areas: [Vec<_>; 6] = Default::default();
        let mut processed = 0;
        while processed < 6 {
            let i = processed;
            processed += 1;
            let Ok(info) = vfio_device.region_info(i as u32) else {
                continue;
            };
            if info.size == 0 {
                continue;
            }

            let flags = bars[i] & 0xf;
            bar_flags[i] = flags;
            let encoded = cfg_space::BarEncodingBits::from(flags);
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
                .region_mmap_areas(i as u32)
                .with_context(|| format!("failed to query VFIO mmap areas for BAR {i}"))?;
        }

        // Discover MSI-X capability from physical device config space.
        // This must happen BEFORE creating direct BAR mappings so we can
        // exclude the MSI-X table/PBA regions.
        let msix = discover_msix(vfio_device.as_ref(), config_offset, config_size, msi_target);

        // Cache whether the device supports VFIO_DEVICE_RESET so we can skip
        // the ioctl on every VM reset for devices that don't support it.
        let supports_reset = vfio_device
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
                        &vfio_device,
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
            }
        }

        tracing::info!(
            pci_id = pci_id.as_str(),
            ?bar_masks,
            has_msix = msix.is_some(),
            supports_reset,
            "VFIO assigned PCI device initialized"
        );

        Ok(Self {
            pci_id,
            vfio_device,
            config_offset,
            config_size,
            bar_masks,
            bars: bar_flags, // Ignore the current BAR values--we don't care what the device thinks the BARs are.
            bar_flags,
            mmio_enabled: false,
            active_bars: BarMappings::default(),
            bar_mmio_controls,
            bar_regions,
            msix,
            supports_reset,
            bar_direct_maps,
            binding,
        })
    }

    fn read_phys_config(&self, offset: u16) -> u32 {
        match read_config_u32(
            self.vfio_device.as_ref(),
            self.config_offset,
            self.config_size,
            offset,
        ) {
            Ok(value) => value,
            Err(e) => {
                tracelimit::warn_ratelimited!(
                    offset,
                    error = ?e,
                    "VFIO config space read failed"
                );
                !0
            }
        }
    }

    fn write_phys_config(&self, offset: u16, value: u32) {
        if let Err(e) = write_config_u32(
            self.vfio_device.as_ref(),
            self.config_offset,
            self.config_size,
            offset,
            value,
        ) {
            tracelimit::warn_ratelimited!(
                offset,
                error = ?e,
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

        // VFIO map_msix has a hard limit of 256 eventfds per call.
        anyhow::ensure!(
            count <= 256,
            "MSI-X vector count ({count}) exceeds VFIO limit of 256"
        );

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
    fn msix_disable(&mut self) {
        let count = self
            .msix
            .as_ref()
            .expect("msix must be present")
            .vector_count;

        if let Err(e) = self.vfio_device.unmap_msix(0, count as u32) {
            tracing::warn!(
                error = ?e,
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
        let new_bars = if self.mmio_enabled {
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
                                error = ?e,
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

fn read_config_u32(
    file: &std::fs::File,
    config_offset: u64,
    config_size: u64,
    offset: u16,
) -> anyhow::Result<u32> {
    if (offset as u64) + 4 > config_size {
        anyhow::bail!("config read offset {offset:#x} out of range");
    }
    let mut buf = [0u8; 4];
    let n = file
        .read_at(&mut buf, config_offset + offset as u64)
        .with_context(|| format!("failed to read config at offset {offset:#x}"))?;
    anyhow::ensure!(
        n == 4,
        "short config read at offset {offset:#x}: got {n} bytes"
    );
    // VFIO config space reads return host-endian bytes on x86. Using
    // native endian is correct on LE platforms (x86, aarch64).
    Ok(u32::from_ne_bytes(buf))
}

fn write_config_u32(
    file: &std::fs::File,
    config_offset: u64,
    config_size: u64,
    offset: u16,
    value: u32,
) -> anyhow::Result<()> {
    if (offset as u64) + 4 > config_size {
        anyhow::bail!("config write offset {offset:#x} out of range");
    }
    let n = file.write_at(&value.to_ne_bytes(), config_offset + offset as u64)?;
    anyhow::ensure!(
        n == 4,
        "short config write at offset {offset:#x}: wrote {n} bytes"
    );
    Ok(())
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

/// Walk the PCI capabilities list to find an MSI-X capability. If found,
/// create an [`MsixEmulator`] and return the discovery info.
fn discover_msix(
    device_file: &std::fs::File,
    config_offset: u64,
    config_size: u64,
    msi_target: &MsiTarget,
) -> Option<MsixEmulationState> {
    // Read the Capabilities Pointer. Bottom 2 bits are reserved per PCI spec §6.7.
    let cap_ptr_dword = read_config_u32(
        device_file,
        config_offset,
        config_size,
        HeaderType00::RESERVED_CAP_PTR.0,
    )
    .ok()?;
    let mut cap_ptr = (cap_ptr_dword & 0xFC) as u16; // mask off reserved bits [1:0]
    let mut iterations = 0usize;

    while cap_ptr != 0 {
        // Guard against malformed capability lists (cycles or excessive length).
        // PCI config space is 256 bytes; capabilities are at least 4 bytes each.
        const MAX_CAPS: usize = 48;
        if iterations >= MAX_CAPS {
            tracing::warn!("PCI capability list exceeded {MAX_CAPS} entries, aborting walk");
            return None;
        }
        iterations += 1;

        let header = read_config_u32(device_file, config_offset, config_size, cap_ptr).ok()?;
        let cap_id = (header & 0xFF) as u8;
        let next_ptr = ((header >> 8) & 0xFC) as u16;

        if cap_id == pci_core::spec::caps::CapabilityId::MSIX.0 {
            // Message Control is in the upper 16 bits of the first DWORD.
            let msg_ctrl = (header >> 16) as u16;
            let table_count = (msg_ctrl & 0x7FF) + 1;

            // Table Offset/BIR (second DWORD of the capability).
            let table_dword =
                read_config_u32(device_file, config_offset, config_size, cap_ptr + 4).ok()?;
            let table_bir = (table_dword & 0x7) as u8;
            let table_offset = table_dword & !0x7;

            // PBA Offset/BIR (third DWORD of the capability).
            let pba_dword =
                read_config_u32(device_file, config_offset, config_size, cap_ptr + 8).ok()?;
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

            return Some(MsixEmulationState {
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
        }

        cap_ptr = next_ptr;
    }

    None
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
            pci_id,
            vfio_device,
            config_offset: _, // immutable device geometry
            config_size: _,   // immutable device geometry
            bar_masks: _,     // immutable device geometry
            bars,
            bar_flags,
            mmio_enabled: _,      // handled above
            active_bars: _,       // handled by update_bar_mappings()
            bar_mmio_controls: _, // handled by update_bar_mappings()
            bar_direct_maps: _,   // handled by update_bar_mappings()
            bar_regions: _,       // immutable device geometry
            msix,
            supports_reset,
            binding: _, // lifetime handle — no reset needed
        } = self;

        // Reset emulated MSI-X table and capability to power-on defaults
        // (all vectors masked, address/data zeroed). The capability and
        // emulator share state via Arc<Mutex>.
        if let Some(msix) = msix {
            msix.enabled = false;
            msix.capability.reset();
        }

        // Reset cached BAR addresses to power-on defaults (flags only, no
        // address bits). The guest will re-probe and re-program BARs.
        *bars = *bar_flags;

        // Reset the physical device via VFIO so it starts in a clean state.
        if *supports_reset {
            if let Err(err) = vfio_device.reset() {
                tracing::warn!(
                    pci_id = pci_id.as_str(),
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to reset VFIO device"
                );
            }
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
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = match HeaderType00(offset) {
            // BAR registers: return locally cached values.
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                let i = (offset - HeaderType00::BAR0.0) as usize / 4;
                self.bars[i]
            }
            // MSI-X capability first DWORD: merge hardware ID/NextPtr (low
            // 16 bits) with emulator's Message Control (high 16 bits). The
            // emulator tracks the enable/function-mask bits; the hardware
            // provides the correct capability ID and Next Pointer so the
            // capability chain remains intact.
            offset if self.msix.as_ref().is_some_and(|m| offset.0 == m.cap_offset) => {
                let msix = self.msix.as_ref().unwrap();
                let hw = self.read_phys_config(offset.0);
                let emu = msix.capability.read_u32(0);
                // Low 16 bits from hardware (cap ID + next ptr),
                // high 16 bits from emulator (message control).
                (hw & 0xFFFF) | (emu & 0xFFFF0000)
            }
            // Everything else: read from physical device.
            _ => self.read_phys_config(offset),
        };

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match HeaderType00(offset) {
            // Command register: track MMIO enable/disable.
            HeaderType00::STATUS_COMMAND => {
                let command = cfg_space::Command::from_bits(value as u16);
                let new_mmio_enabled = command.mmio_enabled();

                if new_mmio_enabled != self.mmio_enabled {
                    self.mmio_enabled = new_mmio_enabled;
                    self.update_bar_mappings();
                    tracing::debug!(
                        pci_id = self.pci_id.as_str(),
                        enabled = new_mmio_enabled,
                        "MMIO state changed by guest"
                    );
                }

                self.write_phys_config(offset, value);
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
                self.bars[i] = (value & self.bar_masks[i]) | self.bar_flags[i];

                if self.mmio_enabled {
                    self.update_bar_mappings();
                }
            }
            // All other registers: pass through to physical device.
            _ => {
                // Intercept MSI-X capability writes to track enable/disable
                // state in the software emulator. Do NOT forward the MSI-X
                // control register to hardware via write_phys_config — VFIO
                // manages the hardware MSI-X enable bit internally via
                // VFIO_DEVICE_SET_IRQS. Writing it again through config space
                // causes VFIO to tear down and re-setup MSI-X, losing the
                // eventfd associations.
                if let Some(msix) = &mut self.msix {
                    if offset == msix.cap_offset {
                        let new_enabled = value & 0x8000_0000 != 0;
                        let was_enabled = msix.enabled;

                        if new_enabled && !was_enabled {
                            // Install irqfd routes BEFORE writing the
                            // capability, so that when the capability
                            // processes the enable transition it can call
                            // enable() on the already-installed routes.
                            match self.msix_enable() {
                                Ok(()) => {
                                    let msix = self.msix.as_mut().unwrap();
                                    msix.capability.write_u32(0, value);
                                    msix.enabled = true;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = ?e,
                                        pci_id = self.pci_id.as_str(),
                                        "failed to enable MSI-X"
                                    );
                                }
                            }
                        } else if was_enabled && !new_enabled {
                            // Write capability first to disable vectors,
                            // then tear down VFIO mapping.
                            msix.capability.write_u32(0, value);
                            self.msix_disable();
                            self.msix.as_mut().unwrap().enabled = false;
                        } else {
                            // No enable/disable transition — just forward.
                            msix.capability.write_u32(0, value);
                        }
                        // Skip write_phys_config for MSI-X control register.
                        return IoResult::Ok;
                    }
                }
                self.write_phys_config(offset, value);
            }
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
                                error = ?e,
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
