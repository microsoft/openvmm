// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests.

#![cfg(test)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use closeable_mutex::CloseableMutex;
use guestmem::GuestMemory;
use guid::Guid;
use hvdef::Vtl;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use openhcl_tdisp::mocks::TdispNoopResourceValidator;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::sync::Arc;
use task_control::StopTask;
use tdisp::TdispHostDeviceTargetEmulator;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use tdisp::test_helpers::new_null_tdisp_interface;
use test_with_tracing::test;
use virt::IsolationType;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmcore::vpci_msi::VpciInterruptParameters;
use vpci::bus::VpciBusConfig;
use vpci::bus::VpciBusDevice;
use vpci::test_helpers::TestVpciInterruptController;

struct NoopDevice {
    tdisp_interface: TdispHostDeviceTargetEmulator,
}

impl ChipsetDevice for NoopDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_tdisp(&mut self) -> Option<&mut dyn tdisp::TdispHostDeviceTarget> {
        Some(&mut self.tdisp_interface)
    }
}

impl PciConfigSpace for NoopDevice {
    fn pci_cfg_read(&mut self, _offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        value.set(0);
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: ByteEnabledDwordWrite) -> IoResult {
        IoResult::Ok
    }
}

struct BusWrapper(VpciBusDevice);

impl super::MemoryAccess for BusWrapper {
    fn gpa(&mut self) -> u64 {
        0x123456780000
    }

    fn read(&mut self, addr: u64, value: &mut [u8]) {
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_read(addr, value)
            .unwrap();
    }

    fn write(&mut self, addr: u64, value: &[u8]) {
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_write(addr, value)
            .unwrap();
    }
}

fn make_noop_device() -> Arc<CloseableMutex<NoopDevice>> {
    Arc::new(CloseableMutex::new(NoopDevice {
        tdisp_interface: new_null_tdisp_interface("vpci-unit-test"),
    }))
}

#[async_test]
async fn test_negotiate_version(driver: DefaultDriver) {
    let device = make_noop_device();
    let msi_controller = TestVpciInterruptController::new();
    let (bus, mut channel) = VpciBusDevice::new(
        VpciBusConfig {
            instance_id: Guid::new_random(),
            vtom: None,
            vnode: None,
        },
        device,
        &mut ExternallyManagedMmioIntercepts,
        VpciInterruptMapper::new(msi_controller),
    )
    .unwrap();

    let (host, guest) = vmbus_channel::connected_async_channels(32768);

    let mut runner = channel.open(host, GuestMemory::empty()).unwrap();
    let _task = driver.spawn("server", async move {
        StopTask::run_with(std::future::pending(), async |stop| {
            let _ = channel.run(stop, &mut runner).await;
        })
        .await
    });

    let (_client, devices) =
        super::VpciClient::connect(&driver, guest, Box::new(BusWrapper(bus)), mesh::channel().0)
            .await
            .unwrap();

    let (device, _removed) = devices
        .into_iter()
        .next()
        .unwrap()
        .init(
            Arc::new(TdispNoopResourceValidator::new()),
            IsolationType::None,
            0,
            Vtl::Vtl0,
        )
        .await
        .unwrap();
    let MsiAddressData { address, data } = device
        .register_interrupt(
            1,
            &VpciInterruptParameters {
                vector: 5,
                multicast: false,
                target_processors: &[1, 2, 3],
            },
        )
        .await
        .unwrap();

    let mut value = 0;
    device.read_cfg(
        256,
        ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
    );
    assert_eq!(value, 0);

    device.unregister_interrupt(address, data).await;
}

/// Tests that VPCI can negotiate basic TDISP commands with a device.
/// This test covers:
/// - VMBUS VPCI packet serialization for VpciTdispCommand
/// - TDISP command serialization
/// - VPCI VMBUS server interface receiving and responding to TDISP commands
/// - VPCI VMBUS client interface sending and receiving TDISP commands
/// - Basic TDISP state machine processing
#[async_test]
async fn test_tdisp_interface_get_device_interface_info(driver: DefaultDriver) {
    let device = make_noop_device();
    let msi_controller = TestVpciInterruptController::new();
    let (bus, mut channel) = VpciBusDevice::new(
        VpciBusConfig {
            instance_id: Guid::new_random(),
            vtom: None,
            vnode: None,
        },
        device,
        &mut ExternallyManagedMmioIntercepts,
        VpciInterruptMapper::new(msi_controller),
    )
    .unwrap();

    let (host, guest) = vmbus_channel::connected_async_channels(32768);

    let mut runner = channel.open(host, GuestMemory::empty()).unwrap();
    let _task = driver.spawn("server", async move {
        StopTask::run_with(std::future::pending(), async |stop| {
            let _ = channel.run(stop, &mut runner).await;
        })
        .await
    });

    let (_client, devices) =
        super::VpciClient::connect(&driver, guest, Box::new(BusWrapper(bus)), mesh::channel().0)
            .await
            .unwrap();

    let (device, _removed) = devices
        .into_iter()
        .next()
        .unwrap()
        .init(
            Arc::new(TdispNoopResourceValidator::new()),
            IsolationType::None,
            0,
            Vtl::Vtl0,
        )
        .await
        .unwrap();
    let interface = device
        .tdisp_get_device_interface_info(TDISP_MOCK_GUEST_PROTOCOL)
        .await;

    match interface {
        Ok(interface) => {
            assert_eq!(
                interface.guest_protocol_type,
                TDISP_MOCK_GUEST_PROTOCOL as i32
            );
            assert_eq!(interface.supported_features, TDISP_MOCK_SUPPORTED_FEATURES);
            assert_eq!(interface.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
        }
        Err(err) => panic!("unexpected error: {err}"),
    }
}

mod active_mmio_bars {
    use crate::ActiveMmioBar;
    use crate::active_mmio_bars;

    /// Build the size mask a device reports for a 32-bit memory BAR of `size`
    /// bytes. `size` must be a power of two.
    pub(super) fn mask_32(size: u32, prefetchable: bool) -> u32 {
        let mut mask = (!(size - 1)) & !0xF;
        if prefetchable {
            mask |= 0b1000;
        }
        mask
    }

    /// Build the low and high size masks a device reports for a 64-bit memory
    /// BAR of `size` bytes. `size` must be a power of two.
    pub(super) fn mask_64(size: u64, prefetchable: bool) -> (u32, u32) {
        let full = (!(size - 1)) & !0xF;
        let mut low = full as u32;
        // Bits 2:1 == 0b10 marks the BAR as 64-bit.
        low |= 0b0100;
        if prefetchable {
            low |= 0b1000;
        }
        ((low) & !0b0010, (full >> 32) as u32)
    }

    #[test]
    fn no_bars_implemented() {
        assert_eq!(active_mmio_bars(&[0; 6], &[0; 6]), vec![]);
    }

    #[test]
    fn single_32_bit_bar() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        masks[0] = mask_32(0x1000, false);
        bars[0] = 0xf000_0000;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0xf000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn thirty_two_bit_bar_ignores_encoding_bits_in_the_base() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        masks[0] = mask_32(0x1000, true);
        // The guest writes the address; the device's encoding bits stay in the
        // low nibble and must not leak into the reported base.
        bars[0] = 0xf000_0000 | 0b1000;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0xf000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn single_64_bit_bar_consumes_two_slots() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        let (low, high) = mask_64(0x20_0000, true);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0xe000_0000;
        bars[1] = 0x0000_0001;

        // Reported once, under the lower half's index, with the two halves
        // combined into one address.
        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0x1_e000_0000,
                length_bytes: 0x20_0000,
            }]
        );
    }

    #[test]
    fn sixty_four_bit_bar_with_zero_high_half() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        let (low, high) = mask_64(0x1000, false);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0xf000_0000;
        bars[1] = 0;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0xf000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn mixed_32_and_64_bit_bars() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];

        // BAR 0: 32-bit, 4KiB.
        masks[0] = mask_32(0x1000, false);
        bars[0] = 0xf000_0000;

        // BAR 1+2: 64-bit, 2MiB. Reported under index 1.
        let (low, high) = mask_64(0x20_0000, true);
        masks[1] = low;
        masks[2] = high;
        bars[1] = 0xe000_0000;
        bars[2] = 0x0000_0002;

        // BAR 3: unimplemented.
        // BAR 4: 32-bit, 64KiB.
        masks[4] = mask_32(0x1_0000, false);
        bars[4] = 0xd000_0000;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![
                ActiveMmioBar {
                    bar_id: 0,
                    base_address: 0xf000_0000,
                    length_bytes: 0x1000,
                },
                ActiveMmioBar {
                    bar_id: 1,
                    base_address: 0x2_e000_0000,
                    length_bytes: 0x20_0000,
                },
                ActiveMmioBar {
                    bar_id: 4,
                    base_address: 0xd000_0000,
                    length_bytes: 0x1_0000,
                },
            ]
        );
    }

    #[test]
    fn sixty_four_bit_upper_half_is_not_reported_separately() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        let (low, high) = mask_64(0x1000, false);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0xf000_0000;
        // An upper half that would decode to a nonzero base of its own, so a
        // decoder that failed to consume this slot would emit a second entry
        // here rather than folding it into BAR 0's address.
        bars[1] = 0x0000_0010;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0x10_f000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn unmapped_bar_is_skipped() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        // Implemented but never programmed by the guest: base stays zero.
        masks[0] = mask_32(0x1000, false);
        bars[0] = 0;
        // A programmed one alongside it, to show only the unmapped one drops.
        masks[1] = mask_32(0x1000, false);
        bars[1] = 0xf000_0000;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 1,
                base_address: 0xf000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn sixty_four_bit_bar_larger_than_4gib() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        // 8GiB, which a u32 length could not have described at all.
        let (low, high) = mask_64(0x2_0000_0000, true);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0;
        bars[1] = 0x0000_0004;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0x4_0000_0000,
                length_bytes: 0x2_0000_0000,
            }]
        );
    }

    #[test]
    fn exactly_4gib_64_bit_bar() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        // 4GiB is one byte past what a u32 length could hold, so this is the
        // smallest BAR the old u32 plumbing had to reject outright.
        let (low, high) = mask_64(0x1_0000_0000, true);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0;
        bars[1] = 0x0000_0008;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0x8_0000_0000,
                length_bytes: 0x1_0000_0000,
            }]
        );
    }

    #[test]
    fn two_gib_64_bit_bar_still_decodes() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        // 2GiB is the largest power of two that fit in a u32 length, so it is
        // the boundary the old plumbing stopped at. It must still work.
        let (low, high) = mask_64(0x8000_0000, true);
        masks[0] = low;
        masks[1] = high;
        bars[0] = 0x8000_0000;
        bars[1] = 0;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 0,
                base_address: 0x8000_0000,
                length_bytes: 0x8000_0000,
            }]
        );
    }

    #[test]
    fn sixty_four_bit_bar_in_the_last_slot_falls_back_to_32_bit() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        // A 64-bit BAR in slot 5 has no upper half to pair with, which is
        // malformed. It is decoded as 32-bit rather than reading past the end.
        let (low, _high) = mask_64(0x1000, false);
        masks[5] = low;
        bars[5] = 0xf000_0000;

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![ActiveMmioBar {
                bar_id: 5,
                base_address: 0xf000_0000,
                length_bytes: 0x1000,
            }]
        );
    }

    #[test]
    fn all_six_slots_used_by_three_64_bit_bars() {
        let mut bars = [0u32; 6];
        let mut masks = [0u32; 6];
        for pair in 0..3u32 {
            let low_index = pair as usize * 2;
            let (low, high) = mask_64(0x1000, false);
            masks[low_index] = low;
            masks[low_index + 1] = high;
            bars[low_index] = 0xf000_0000 + pair * 0x1000;
            // Nonzero upper halves, so that a decoder which failed to consume
            // the second slot of each pair would emit spurious entries for
            // them rather than silently dropping them as zero bases.
            bars[low_index + 1] = 0x10 + pair * 0x10;
        }

        assert_eq!(
            active_mmio_bars(&bars, &masks),
            vec![
                ActiveMmioBar {
                    bar_id: 0,
                    base_address: 0x10_f000_0000,
                    length_bytes: 0x1000,
                },
                ActiveMmioBar {
                    bar_id: 2,
                    base_address: 0x20_f000_1000,
                    length_bytes: 0x1000,
                },
                ActiveMmioBar {
                    bar_id: 4,
                    base_address: 0x30_f000_2000,
                    length_bytes: 0x1000,
                },
            ]
        );
    }
}

mod implemented_bars {
    use super::active_mmio_bars::mask_32;
    use super::active_mmio_bars::mask_64;
    use crate::implemented_bars;

    #[test]
    fn no_bars_implemented() {
        assert_eq!(implemented_bars(&[0; 6]), [false; 6]);
    }

    #[test]
    fn thirty_two_bit_bars_in_some_slots() {
        let mut masks = [0u32; 6];
        masks[0] = mask_32(0x1000, false);
        masks[4] = mask_32(0x1_0000, true);

        assert_eq!(
            implemented_bars(&masks),
            [true, false, false, false, true, false]
        );
    }

    #[test]
    fn sixty_four_bit_bar_marks_only_its_lower_half() {
        let mut masks = [0u32; 6];
        let (low, high) = mask_64(0x20_0000, true);
        masks[0] = low;
        masks[1] = high;

        // The upper half is not addressable in its own right, so it is not a
        // BAR even though its mask is nonzero.
        assert_eq!(
            implemented_bars(&masks),
            [true, false, false, false, false, false]
        );
    }

    #[test]
    fn exactly_4gib_64_bit_bar_is_implemented() {
        let mut masks = [0u32; 6];
        // A 4GiB BAR has no address bits in its low mask at all: the whole
        // size sits in the high one. Testing the halves separately would call
        // this BAR unimplemented.
        let (low, high) = mask_64(0x1_0000_0000, true);
        masks[0] = low;
        masks[1] = high;
        assert_eq!(low & !0xF, 0, "the low mask must have no address bits");

        assert_eq!(
            implemented_bars(&masks),
            [true, false, false, false, false, false]
        );
    }

    #[test]
    fn sixty_four_bit_bar_larger_than_4gib_is_implemented() {
        let mut masks = [0u32; 6];
        let (low, high) = mask_64(0x2_0000_0000, true);
        masks[0] = low;
        masks[1] = high;

        assert_eq!(
            implemented_bars(&masks),
            [true, false, false, false, false, false]
        );
    }

    #[test]
    fn all_six_slots_used_by_three_64_bit_bars() {
        let mut masks = [0u32; 6];
        for pair in 0..3 {
            let (low, high) = mask_64(0x1000, false);
            masks[pair * 2] = low;
            masks[pair * 2 + 1] = high;
        }

        assert_eq!(
            implemented_bars(&masks),
            [true, false, true, false, true, false]
        );
    }

    #[test]
    fn sixty_four_bit_bar_in_the_last_slot_has_no_upper_half() {
        let mut masks = [0u32; 6];
        // Malformed, but it must not read past the end of the array.
        let (low, _high) = mask_64(0x1000, false);
        masks[5] = low;

        assert_eq!(
            implemented_bars(&masks),
            [false, false, false, false, false, true]
        );
    }
}
