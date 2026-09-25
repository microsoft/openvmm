// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM chipset manifest construction.

use super::BaseChipsetManifest;
use super::BaseChipsetType;
use super::Error;
use super::ErrorInner;
use super::LayoutConfig;
use super::MachineArch;
use super::VmChipsetResult;

pub(super) fn vmbus_enabled(ty: &BaseChipsetType, default: bool) -> bool {
    default && !matches!(ty, BaseChipsetType::Microvm)
}

pub(super) fn build(arch: MachineArch, result: &mut VmChipsetResult) -> Result<(), Error> {
    if arch != MachineArch::X86_64 {
        return Err(Error(ErrorInner::UnsupportedArch));
    }
    result.chipset = BaseChipsetManifest {
        with_generic_cmos_rtc: true,
        ..BaseChipsetManifest::empty()
    };
    result.attach_generic_ioapic();
    result.attach_pic();
    result.attach_pit();
    Ok(())
}

pub(super) fn layout_config() -> LayoutConfig {
    LayoutConfig {
        chipset_low_mmio_size: 1024 * 1024 * 1024,
        chipset_high_mmio_size: 0,
        vtl2_chipset_mmio_size: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::super::BaseChipsetType;
    use super::super::MachineArch;
    use super::super::VmManifestBuilder;
    use chipset_resources::pic::PicDeviceHandle;
    use chipset_resources::pit::PitDeviceHandle;
    use test_with_tracing::test;
    use vm_resource::ResourceId;

    #[test]
    fn microvm_microvm_has_only_allowlisted_base_devices() {
        let builder = VmManifestBuilder::new(BaseChipsetType::Microvm, MachineArch::X86_64);
        assert!(!builder.vmbus);
        assert_eq!(
            builder.layout_config().chipset_low_mmio_size,
            1024 * 1024 * 1024
        );

        let result = builder.build().unwrap();
        assert!(result.chipset.with_generic_cmos_rtc);
        assert_eq!(
            result
                .chipset_devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            ["ioapic", PicDeviceHandle::ID, PitDeviceHandle::ID]
        );
        assert!(result.capabilities.with_ioapic);
        assert!(result.capabilities.with_pic);
        assert!(result.capabilities.with_pit);
        assert!(result.pci_chipset_devices.is_empty());
        assert!(result.isa_dma_controller.is_none());
    }
}
