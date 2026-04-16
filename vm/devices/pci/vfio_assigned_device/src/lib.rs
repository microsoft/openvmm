// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VFIO-backed PCI device assignment for OpenVMM.
//!
//! This crate implements a `ChipsetDevice` that proxies PCI config space
//! and BAR MMIO accesses to a physical device opened via Linux VFIO. The device
//! appears as a standard PCIe endpoint to the guest. MSI-X table and PBA
//! accesses are intercepted and handled by a software emulator; all other BAR
//! MMIO is proxied directly to hardware.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::bar_mapping::BarMappings;
use pci_core::capabilities::PciCapability;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::msi::MsiTarget;
use pci_core::spec::cfg_space;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use virt::irqfd::IrqFd;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;

/// PCI config space offsets as plain constants for use in match patterns.
const CFG_BAR0: u16 = 0x10;
const CFG_BAR5: u16 = 0x24;
const CFG_STATUS_COMMAND: u16 = 0x04;
const CFG_CAP_PTR: u16 = 0x34;

/// VFIO BAR region information (offset and size within the device fd).
#[derive(Debug, Clone, Copy, Inspect)]
pub struct VfioBarInfo {
    /// Offset within the VFIO device fd where this BAR region starts.
    #[inspect(hex)]
    pub vfio_offset: u64,
    /// Size of the BAR region in bytes.
    #[inspect(hex)]
    pub size: u64,
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
    /// Byte offset of the MSI-X table within its BAR.
    #[inspect(hex)]
    table_offset: u32,
    /// Total size of the MSI-X table in bytes (vector_count * 16).
    #[inspect(hex)]
    table_size: u64,
    /// BAR index containing the PBA.
    pba_bar: u8,
    /// Byte offset of the PBA within its BAR.
    #[inspect(hex)]
    pba_offset: u32,
    /// Total size of the PBA in bytes.
    #[inspect(hex)]
    pba_size: u64,
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
pub struct VfioAssignedPciDevice {
    /// The PCI address string (e.g., "0000:01:00.0") for diagnostics.
    #[inspect(display)]
    pci_id: String,

    /// The VFIO device, used for config space, BAR MMIO, and MSI-X mapping.
    #[inspect(skip)]
    vfio_device: vfio_sys::Device,

    /// irqfd routing interface for registering eventfds with the hypervisor.
    #[inspect(skip)]
    irqfd: Arc<dyn IrqFd>,

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
    bar_mmio_controls: Vec<Box<dyn chipset_device::mmio::ControlMmioIntercept>>,

    /// VFIO region info per BAR for MMIO proxying via pread/pwrite.
    #[inspect(iter_by_index)]
    bar_regions: [Option<VfioBarInfo>; 6],

    /// MSI-X emulation state (None if device has no MSI-X capability).
    msix: Option<MsixEmulationState>,

    /// VFIO container and group handles. These must be kept alive for the
    /// lifetime of the device — dropping them would close the VFIO fds and
    /// tear down IOMMU mappings. Declared after `vfio_device` so they are
    /// dropped after the device fd.
    #[inspect(skip)]
    _vfio_container: vfio_sys::Container,
    #[inspect(skip)]
    _vfio_group: vfio_sys::Group,
}

/// Parameters for creating a [`VfioAssignedPciDevice`].
pub struct VfioAssignedPciDeviceConfig {
    /// PCI BDF string (e.g., "0000:01:00.0").
    pub pci_id: String,
    /// The opened VFIO device (from `vfio_sys::Group::open_device`).
    pub vfio_device: vfio_sys::Device,
    /// Config region info from `vfio_sys::Device::region_info(CONFIG_REGION_INDEX)`.
    pub config_offset: u64,
    /// Config region size.
    pub config_size: u64,
    /// MSI target for the MsixEmulator (used to track table entries; interrupt
    /// delivery bypasses this path via irqfd).
    pub msi_target: MsiTarget,
    /// VFIO region info per BAR. `None` if the BAR region does not exist or
    /// has zero size.
    pub bar_info: [Option<VfioBarInfo>; 6],
    /// irqfd routing interface for registering eventfds with the hypervisor.
    pub irqfd: Arc<dyn IrqFd>,
    /// Chipset MMIO region controls per BAR (created via
    /// `services.register_mmio().new_io_region()`).
    pub bar_mmio_controls: Vec<Box<dyn chipset_device::mmio::ControlMmioIntercept>>,
    /// VFIO container handle (must outlive the device).
    pub vfio_container: vfio_sys::Container,
    /// VFIO group handle (must outlive the device).
    pub vfio_group: vfio_sys::Group,
}

impl VfioAssignedPciDevice {
    /// Create a new VFIO assigned PCI device.
    ///
    /// Reads BAR flags from config space and derives BAR masks from the VFIO
    /// region sizes (avoiding the write-all-ones probe cycle). Discovers MSI-X
    /// capability if present and creates a software emulator for it.
    pub fn new(config: VfioAssignedPciDeviceConfig) -> anyhow::Result<Self> {
        let vfio_device = config.vfio_device;
        let config_offset = config.config_offset;
        let config_size = config.config_size;
        let device_file = vfio_device.as_ref();

        // Read BAR values and derive masks from VFIO region sizes.
        // This avoids the standard write-all-ones probe cycle — VFIO already
        // knows the BAR sizes from the host kernel.
        let mut bar_masks = [0u32; 6];
        let mut bars = [0u32; 6];
        let mut bar_flags = [0u32; 6];

        let mut i = 0;
        while i < 6 {
            let offset = CFG_BAR0 + (i as u16) * 4;
            let original = read_config_u32(device_file, config_offset, config_size, offset)?;
            bars[i] = original;
            bar_flags[i] = original & 0xf;

            // Derive the mask from the VFIO region size. For a BAR of size N
            // (power of 2), the mask is ~(N - 1) with the low flag bits clear.
            // Use the full u64 size to avoid truncation for 64-bit BARs.
            if let Some(info) = &config.bar_info[i] {
                let mask64 = !(info.size.wrapping_sub(1));
                bar_masks[i] = (mask64 as u32) & !0xf;
            }

            // Skip the upper 32 bits of a 64-bit BAR.
            if cfg_space::BarEncodingBits::from_bits(original).type_64_bit() {
                if i + 1 < 6 {
                    let upper_offset = CFG_BAR0 + ((i + 1) as u16) * 4;
                    let upper_original =
                        read_config_u32(device_file, config_offset, config_size, upper_offset)?;
                    bars[i + 1] = upper_original;
                    bar_flags[i + 1] = 0;

                    // Upper 32 bits of a 64-bit BAR: mask from the upper
                    // portion of the full 64-bit mask.
                    if let Some(info) = &config.bar_info[i] {
                        let mask64 = !(info.size.wrapping_sub(1));
                        bar_masks[i + 1] = (mask64 >> 32) as u32;
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }

        let bar_regions = config.bar_info;

        // Discover MSI-X capability from physical device config space.
        let msix = discover_msix(device_file, config_offset, config_size, &config.msi_target);

        tracing::info!(
            pci_id = config.pci_id.as_str(),
            ?bar_masks,
            has_msix = msix.is_some(),
            "VFIO assigned PCI device initialized"
        );

        Ok(Self {
            pci_id: config.pci_id,
            vfio_device,
            irqfd: config.irqfd,
            config_offset,
            config_size,
            bar_masks,
            bars,
            bar_flags,
            mmio_enabled: false,
            active_bars: BarMappings::default(),
            bar_mmio_controls: config.bar_mmio_controls,
            bar_regions,
            msix,
            _vfio_container: config.vfio_container,
            _vfio_group: config.vfio_group,
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
        if bar == msix.table_bar {
            let table_start = msix.table_offset as u64;
            let table_end = table_start + msix.table_size;
            if offset >= table_start && offset < table_end {
                // Emulator table starts at offset 0.
                return Some(offset - table_start);
            }
        }

        // Check PBA region.
        if bar == msix.pba_bar {
            let pba_start = msix.pba_offset as u64;
            let pba_end = pba_start + msix.pba_size;
            if offset >= pba_start && offset < pba_end {
                // In the emulator, PBA starts right after the table.
                let emu_pba_start = msix.table_size;
                return Some(emu_pba_start + (offset - pba_start));
            }
        }

        None
    }

    /// Set up irqfd-backed MSI-X interrupt delivery when the guest enables MSI-X.
    ///
    /// Tells the emulator to create irqfd routes and passes the resulting
    /// events to VFIO so the physical device signals them on interrupt.
    fn msix_enable(&mut self) -> anyhow::Result<()> {
        let msix = self.msix.as_mut().expect("msix must be present");
        let count = msix.vector_count;

        // VFIO map_msix has a hard limit of 256 eventfds per call.
        anyhow::ensure!(
            count <= 256,
            "MSI-X vector count ({count}) exceeds VFIO limit of 256"
        );

        let vfio_device = &self.vfio_device;
        msix.emulator.enable_irqfd(self.irqfd.as_ref(), |events| {
            vfio_device
                .map_msix(0, events)
                .context("VFIO map_msix failed")
        })?;

        tracing::info!(
            count,
            pci_id = self.pci_id.as_str(),
            "MSI-X enabled: mapped vectors to irqfd routes"
        );
        Ok(())
    }

    /// Tear down VFIO MSI-X eventfd mapping when the guest disables MSI-X.
    fn msix_disable(&mut self) {
        let msix = self.msix.as_mut().expect("msix must be present");
        let count = msix.vector_count;

        if let Err(e) = self.vfio_device.unmap_msix(0, count as u32) {
            tracing::warn!(
                error = ?e,
                pci_id = self.pci_id.as_str(),
                "VFIO unmap_msix failed"
            );
        }

        msix.emulator.disable_irqfd();
        tracing::info!(
            pci_id = self.pci_id.as_str(),
            "MSI-X disabled: unmapped vectors"
        );
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
    let n = file.read_at(&mut buf, config_offset + offset as u64)?;
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

/// Walk the PCI capabilities list to find an MSI-X capability. If found,
/// create an [`MsixEmulator`] and return the discovery info.
fn discover_msix(
    device_file: &std::fs::File,
    config_offset: u64,
    config_size: u64,
    msi_target: &MsiTarget,
) -> Option<MsixEmulationState> {
    // Read the Capabilities Pointer. Bottom 2 bits are reserved per PCI spec §6.7.
    let cap_ptr_dword =
        read_config_u32(device_file, config_offset, config_size, CFG_CAP_PTR).ok()?;
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
                table_offset,
                table_size,
                pba_bar: pba_bir,
                pba_offset,
                pba_size,
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
            self.msix.as_mut().expect("msix must be present").enabled = false;
        }
        // Unmap BAR MMIO regions.
        for control in &mut self.bar_mmio_controls {
            control.unmap();
        }
        self.mmio_enabled = false;
        self.active_bars = BarMappings::default();
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
        *value = match offset {
            // BAR registers: return locally cached values.
            CFG_BAR0..=CFG_BAR5 if (offset - CFG_BAR0).is_multiple_of(4) => {
                let i = (offset - CFG_BAR0) as usize / 4;
                self.bars[i]
            }
            // MSI-X capability first DWORD: merge hardware ID/NextPtr (low
            // 16 bits) with emulator's Message Control (high 16 bits). The
            // emulator tracks the enable/function-mask bits; the hardware
            // provides the correct capability ID and Next Pointer so the
            // capability chain remains intact.
            offset if self.msix.as_ref().is_some_and(|m| offset == m.cap_offset) => {
                let msix = self.msix.as_ref().unwrap();
                let hw = self.read_phys_config(offset);
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
        match offset {
            // Command register: track MMIO enable/disable.
            CFG_STATUS_COMMAND => {
                let command = cfg_space::Command::from_bits(value as u16);
                let new_mmio_enabled = command.mmio_enabled();

                if new_mmio_enabled && !self.mmio_enabled {
                    self.active_bars = BarMappings::parse(&self.bars, &self.bar_masks);
                    // Register BAR address ranges with the chipset so MMIO
                    // accesses are routed to this device.
                    for mapping in self.active_bars.iter() {
                        if let Some(control) =
                            self.bar_mmio_controls.get_mut(mapping.index as usize)
                        {
                            control.map(mapping.base_address);
                        }
                    }
                    tracing::debug!(pci_id = self.pci_id.as_str(), "MMIO enabled by guest");
                } else if !new_mmio_enabled && self.mmio_enabled {
                    // Unregister BAR address ranges.
                    for control in &mut self.bar_mmio_controls {
                        control.unmap();
                    }
                    self.active_bars = BarMappings::default();
                    tracing::debug!(pci_id = self.pci_id.as_str(), "MMIO disabled by guest");
                }

                self.mmio_enabled = new_mmio_enabled;
                self.write_phys_config(offset, value);
            }
            // BAR registers: mask and cache locally.
            CFG_BAR0..=CFG_BAR5 if (offset - CFG_BAR0).is_multiple_of(4) => {
                let i = (offset - CFG_BAR0) as usize / 4;
                self.bars[i] = (value & self.bar_masks[i]) | self.bar_flags[i];
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
                            // set_msi() on the already-installed routes.
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
