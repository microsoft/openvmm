// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM-specific Linux boot configuration.

use super::linux::Error as LinuxError;
use super::linux::KernelConfig;
use super::linux::KernelIsolationConfig;
use guestmem::GuestMemory;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use memory_range::MemoryRange;
use std::ffi::CString;
use std::io::Seek;
use vm_loader::InitialLoad;
use vm_loader::Loader;

const ARM64_KERNEL_BASE: u64 = 0x20_0000;
const ARM64_DTB_CAPACITY: u64 = 0x20_0000;

#[derive(Debug, thiserror::Error)]
pub enum Arm64BootError {
    #[error("failed to read the ARM kernel or initramfs")]
    Read(#[source] std::io::Error),
    #[error("invalid ARM microVM boot placement or Image size")]
    Placement,
    #[error("ARM microVM requires zero-based RAM containing its shared-status page")]
    Ram,
    #[error("ARM microVM direct boot does not support isolation")]
    Isolation,
    #[error("ARM microVM device tree is invalid")]
    DeviceTree(#[from] super::device_tree::DeviceTreeError),
    #[error("ARM Linux image loading failed")]
    Loader(#[from] loader::linux::Error),
}

struct Arm64BootLayout {
    kernel: MemoryRange,
    dtb: MemoryRange,
    initrd: Option<MemoryRange>,
}

fn arm64_boot_layout(
    low_ram: MemoryRange,
    text_offset: u64,
    image_size: u64,
    file_size: u64,
    initrd_size: Option<u64>,
) -> Result<Arm64BootLayout, Arm64BootError> {
    let status = MemoryRange::new(
        openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
            ..openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
                + openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    );
    if low_ram.start() != 0 || !low_ram.contains(&status) {
        return Err(Arm64BootError::Ram);
    }
    if file_size < 64
        || !text_offset.is_multiple_of(0x1000)
        || (image_size != 0 && image_size < file_size)
        || initrd_size == Some(0)
    {
        return Err(Arm64BootError::Placement);
    }
    let size = if image_size == 0 {
        file_size
    } else {
        image_size
    };
    let size = size
        .checked_next_multiple_of(0x1000)
        .ok_or(Arm64BootError::Placement)?;
    let start = ARM64_KERNEL_BASE
        .checked_add(text_offset)
        .ok_or(Arm64BootError::Placement)?;
    let end = start.checked_add(size).ok_or(Arm64BootError::Placement)?;
    let kernel = MemoryRange::try_new(start..end).map_err(|_| Arm64BootError::Placement)?;
    let dtb_end = end
        .checked_add(ARM64_DTB_CAPACITY)
        .ok_or(Arm64BootError::Placement)?;
    let dtb = MemoryRange::try_new(end..dtb_end).map_err(|_| Arm64BootError::Placement)?;
    let initrd = initrd_size
        .map(|size| {
            let start = dtb_end
                .checked_next_multiple_of(0x20_0000)
                .ok_or(Arm64BootError::Placement)?;
            let end = start
                .checked_add(size)
                .and_then(|end| end.checked_next_multiple_of(0x1000))
                .ok_or(Arm64BootError::Placement)?;
            MemoryRange::try_new(start..end).map_err(|_| Arm64BootError::Placement)
        })
        .transpose()?;
    if !low_ram.contains(&kernel)
        || !low_ram.contains(&dtb)
        || initrd.is_some_and(|range| !low_ram.contains(&range))
        || kernel.overlaps(&status)
    {
        return Err(Arm64BootError::Placement);
    }
    Ok(Arm64BootLayout {
        kernel,
        dtb,
        initrd,
    })
}

/// Applies the microVM placement policy while reusing the shared ARM Image
/// loader, register initialization, and architecture-specific DT emission.
#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
pub fn load_linux_arm64(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    topology: &vm_topology::processor::ProcessorTopology<
        vm_topology::processor::aarch64::Aarch64Topology,
    >,
    chipset_mmio: &crate::worker::memory_layout::ChipsetMmioRanges,
    devices: &[super::device_tree::FixedMmioDevice<'_>],
) -> Result<InitialLoad<loader::importer::Aarch64Register>, Arm64BootError> {
    use std::io::Read;
    use std::io::SeekFrom;

    if !matches!(cfg.isolation, KernelIsolationConfig::None) {
        return Err(Arm64BootError::Isolation);
    }
    let low_ram = cfg
        .mem_layout
        .ram()
        .first()
        .ok_or(Arm64BootError::Ram)?
        .range;
    let mut kernel_file = cfg.kernel;
    kernel_file.rewind().map_err(Arm64BootError::Read)?;
    let mut header = [0; 64];
    kernel_file
        .read_exact(&mut header)
        .map_err(Arm64BootError::Read)?;
    let text_offset = u64::from_le_bytes(std::array::from_fn(|index| header[8 + index]));
    let image_size = u64::from_le_bytes(std::array::from_fn(|index| header[16 + index]));
    let file_size = kernel_file
        .seek(SeekFrom::End(0))
        .map_err(Arm64BootError::Read)?;
    let (mut initrd_reader, initrd_size) = if let Some(mut initrd) = cfg.initrd.as_ref() {
        let size = initrd
            .seek(SeekFrom::End(0))
            .map_err(Arm64BootError::Read)?;
        initrd.rewind().map_err(Arm64BootError::Read)?;
        (Some(initrd), Some(size))
    } else {
        (None, None)
    };
    let layout = arm64_boot_layout(low_ram, text_offset, image_size, file_size, initrd_size)?;
    kernel_file.rewind().map_err(Arm64BootError::Read)?;
    let mut image = Vec::new();
    kernel_file
        .take(file_size.checked_add(1).ok_or(Arm64BootError::Placement)?)
        .read_to_end(&mut image)
        .map_err(Arm64BootError::Read)?;
    if image.len() as u64 != file_size || image.get(..64) != Some(&header) {
        return Err(Arm64BootError::Placement);
    }
    let reservations = [MemoryRange::new(
        openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
            ..openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
                + openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    )];
    let dtb = super::device_tree::DeviceTreeBuilder::new(
        topology,
        cfg.mem_layout.ram(),
        &[],
        &[],
        ARM64_DTB_CAPACITY as usize,
        super::device_tree::DeviceTreeBootType::LinuxDirect(super::device_tree::LinuxDirectBoot {
            low_mmio: chipset_mmio.low,
            high_mmio: chipset_mmio.high,
            initrd: layout
                .initrd
                .zip(initrd_size)
                .map(|(range, size)| (range.start(), range.start() + size)),
            smmus: &[],
        }),
    )
    .with_command_line(cfg.cmdline)
    .with_fixed_devices(devices)
    .with_reserved_memory(&reservations)
    .build()?;
    let initrd_config = initrd_reader
        .as_mut()
        .zip(initrd_size)
        .zip(layout.initrd)
        .map(|((reader, size), range)| InitrdConfig {
            initrd_address: InitrdAddressType::Address(range.start()),
            initrd: reader,
            size,
        });
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    let info = loader::linux::load_kernel_and_initrd_arm64(
        &mut loader,
        &mut std::io::Cursor::new(image),
        ARM64_KERNEL_BASE,
        initrd_config,
        Some(&dtb),
    )?;
    if info.kernel.entrypoint != layout.kernel.start()
        || info.kernel.size != layout.kernel.len()
        || info
            .dtb
            .as_ref()
            .is_none_or(|range| range.start != layout.dtb.start())
    {
        return Err(Arm64BootError::Placement);
    }
    loader::linux::set_direct_boot_registers_arm64(&mut loader, &info)?;
    Ok(loader.initial_regs_and_page_imports())
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_linux_x86_mptable(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    apic_ids: &[u32],
    level_triggered_irqs: &[u32],
    ioapic_inputs: u8,
    reserved_memory_ranges: &[MemoryRange],
) -> Result<InitialLoad<X86Register>, LinuxError> {
    if !matches!(cfg.isolation, KernelIsolationConfig::None) {
        return Err(LinuxError::MpTableIsolation);
    }

    let mut kernel_file = cfg.kernel;
    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(LinuxError::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(LinuxError::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };
    let initrd_config = initrd_reader.as_mut().map(|reader| InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: reader,
        size: initrd_size,
    });
    let cmdline = CString::new(cfg.cmdline).map_err(LinuxError::CommandLineNul)?;
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    loader::linux::mptable_boot::load_x86_mptable(
        &mut loader,
        &mut kernel_file,
        initrd_config,
        &cmdline,
        cfg.mem_layout,
        &loader::mptable::MpTableConfig {
            apic_ids,
            level_triggered_irqs,
            ioapic_inputs,
        },
        reserved_memory_ranges,
    )
    .map_err(LinuxError::Loader)?;
    Ok(loader.initial_regs_and_page_imports())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn arm64_placement_reserves_status_and_separates_boot_artifacts() {
        let ram = MemoryRange::new(0..128 * 1024 * 1024);
        let layout = arm64_boot_layout(ram, 0, 32 << 20, 1 << 20, Some(16 << 20)).unwrap();
        assert_eq!(layout.kernel.start(), 0x20_0000);
        assert_eq!(layout.kernel.len(), 32 << 20);
        assert_eq!(layout.dtb.start(), layout.kernel.end());
        assert_eq!(layout.dtb.len(), ARM64_DTB_CAPACITY);
        let initrd = layout.initrd.unwrap();
        assert!(initrd.start().is_multiple_of(0x20_0000));
        assert!(initrd.start() >= layout.dtb.end());
        assert_eq!(initrd.len(), 16 << 20);
        let exact = MemoryRange::new(0..initrd.end());
        arm64_boot_layout(exact, 0, 32 << 20, 1 << 20, Some(16 << 20)).unwrap();
        assert!(
            arm64_boot_layout(
                MemoryRange::new(0..initrd.end() - 0x1000),
                0,
                32 << 20,
                1 << 20,
                Some(16 << 20),
            )
            .is_err()
        );
    }

    #[test]
    fn arm64_placement_rejects_overflow_alignment_and_invalid_image_bounds() {
        let ram = MemoryRange::new(0..128 * 1024 * 1024);
        for (offset, image, file, initrd) in [
            (1, 0x1000, 64, None),
            (!0xfff, 0x1000, 64, None),
            (0, u64::MAX, 64, None),
            (0, 0x1000, 0x2000, None),
            (0, 0, 63, None),
            (0, 0, u64::MAX, None),
            (0, 0x1000, 64, Some(0)),
            (0, 0x1000, 64, Some(u64::MAX)),
        ] {
            assert!(arm64_boot_layout(ram, offset, image, file, initrd).is_err());
        }
        assert!(arm64_boot_layout(MemoryRange::new(0..0x30000), 0, 0x1000, 64, None).is_err());
        assert!(
            arm64_boot_layout(MemoryRange::new(0x1000..128 << 20), 0, 0x1000, 64, None).is_err()
        );
        let legacy = arm64_boot_layout(ram, 0x1000, 0, 0x1001, None).unwrap();
        assert_eq!(legacy.kernel.start(), ARM64_KERNEL_BASE + 0x1000);
        assert_eq!(legacy.kernel.len(), 0x2000);
    }
}
