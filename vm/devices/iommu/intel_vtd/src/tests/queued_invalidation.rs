// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use parking_lot::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use test_with_tracing::test;

const BASE: u64 = 0x1000;
const STATUS: u64 = 0x90000;
const SW: u64 = 1 << 5;
const IF: u64 = 1 << 4;
const FN: u64 = 1 << 6;

struct RecordingMsi {
    gm: GuestMemory,
    events: Mutex<Vec<(u64, u32, u32)>>,
}

impl SignalMsi for RecordingMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        assert_eq!(devid, None);
        self.events
            .lock()
            .push((address, data, self.gm.read_plain::<u32>(STATUS).unwrap()));
    }
}

fn make_queue(dw: bool, mode: u8) -> (IntelVtdDevice, GuestMemory, Arc<RecordingMsi>) {
    let gm = GuestMemory::allocate(0xa0000);
    let msi = Arc::new(RecordingMsi {
        gm: gm.clone(),
        events: Mutex::new(Vec::new()),
    });
    let (mut dev, _) = IntelVtdDevice::new(
        gm.clone(),
        IntelVtdConfig {
            mmio_base: TEST_MMIO_BASE,
        },
        msi.clone(),
    );
    write64(&mut dev, 0x090, BASE | ((dw as u64) << 11));
    // Only the private processing policy uses the future capabilities. There
    // is no alternate device configuration or guest-visible ECAP value.
    {
        let mut state = dev.shared.state.write();
        state.latched_rtaddr = RtaddrReg::from(u64::from(mode) << 10);
        state.gsts.set_qies(true);
    }
    (dev, gm, msi)
}

fn final_ecap() -> EcapReg {
    EcapReg::from(ECAP_VALUE)
        .with_smts(true)
        .with_ssts(true)
        .with_smpwcs(true)
        .with_ssads(true)
        .with_rps(true)
}

fn run_future_queue(dev: &mut IntelVtdDevice, tail: u64) {
    dev.shared.state.write().iqt = IqtReg::from(tail);
    dev.process_invalidation_queue_with_capabilities(CapReg::from(CAP_VALUE), final_ecap());
}

fn submit(dev: &mut IntelVtdDevice, tail: u64, dw: bool) {
    if dw {
        run_future_queue(dev, tail);
    } else {
        write64(dev, 0x088, tail);
    }
}

fn put(gm: &GuestMemory, offset: u64, words: [u64; 4], dw: bool) {
    for (i, word) in words.iter().take(if dw { 4 } else { 2 }).enumerate() {
        gm.write_at(BASE + offset + i as u64 * 8, &word.to_le_bytes())
            .unwrap();
    }
}

fn wait(data: u32, flags: u64) -> [u64; 4] {
    [(u64::from(data) << 32) | flags | 5, STATUS, 0, 0]
}

fn assert_queue(dev: &mut IntelVtdDevice, head: u64, error: bool) {
    assert_eq!(read64(dev, 0x080), head);
    assert_eq!(FstsReg::from(read32(dev, 0x034)).iqe(), error);
}

fn recover(dev: &mut IntelVtdDevice, dw: bool) {
    if dw {
        dev.shared.state.write().fsts.set_iqe(false);
        dev.process_invalidation_queue_with_capabilities(CapReg::from(CAP_VALUE), final_ecap());
    } else {
        write32(dev, 0x034, 1 << 4);
    }
}

#[test]
fn test_queue_sizes_stride_and_wrap() {
    for qs in 0..8 {
        for (dw, mode) in [(false, 0), (true, 0), (true, 1)] {
            let (mut dev, gm, _) = make_queue(dw, mode);
            let stride = if dw { 32 } else { 16 };
            let size = 4096 << qs;
            {
                let mut state = dev.shared.state.write();
                state.iqa.set_qs(qs);
                state.iqh = IqhReg::from(size - 2 * stride);
                state.iqt = IqtReg::from(2 * stride);
                let queue = InvalidationQueue::new(&state, final_ecap()).unwrap();
                assert_eq!(queue.size, size);
                assert_eq!(queue.pending, 4);
                assert_eq!(
                    queue.size / queue.width.bytes() as u64,
                    1 << (qs + if dw { 7 } else { 8 })
                );
            }
            for (i, offset) in [size - 2 * stride, size - stride, 0, stride]
                .into_iter()
                .enumerate()
            {
                put(&gm, offset, wait(i as u32 + 1, SW), dw);
            }
            submit(&mut dev, 2 * stride, dw);
            assert_queue(&mut dev, 2 * stride, false);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 4);
        }
    }
}

#[test]
fn test_maximum_queue_consumes_n_minus_one_descriptors() {
    for dw in [false, true] {
        let (mut dev, gm, _) = make_queue(dw, 0);
        dev.shared.state.write().iqa.set_qs(7);
        let stride = if dw { 32 } else { 16 };
        let tail = 0x80000 - stride;
        for offset in (0..tail).step_by(stride as usize) {
            put(&gm, offset, [0x11, 0, 0, 0], dw);
        }
        put(&gm, tail - stride, wait(0x12345678, SW), dw);
        submit(&mut dev, tail, dw);
        assert_queue(&mut dev, tail, false);
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0x12345678);
    }
}

#[test]
fn test_scalable_queue_processes_all_cache_granularities() {
    let (mut dev, gm, _) = make_queue(true, 1);
    let descriptors = [
        0x11, 0x21, 0x31, // context
        0x12, 0x22, 0x32, // second-stage IOTLB
        0x07, 0x17, 0x37, // PASID-cache
        0x26, 0x36, // P_IOTLB, including nonmatching first-stage pages
        0x04, 0x14, // interrupt cache
    ];
    for (i, lo) in descriptors.into_iter().enumerate() {
        put(&gm, i as u64 * 32, [lo, 0, 0, 0], true);
    }
    let offset = descriptors.len() as u64 * 32;
    put(&gm, offset, wait(0xcafe, SW | FN), true);
    run_future_queue(&mut dev, offset + 32);
    assert_queue(&mut dev, offset + 32, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0xcafe);
    assert_eq!(read64(&mut dev, 0x010), ECAP_VALUE);
    assert_eq!(ECAP_VALUE & ((1 << 43) | (0x1f << 45)), 0);
}

#[test]
fn test_legacy_queue_uses_only_16_bytes() {
    let (mut dev, gm, _) = make_queue(false, 0);
    put(&gm, 0, [0x11, 0, 0, 0], false);
    put(&gm, 16, wait(0xface, SW), false);
    // The wait's nonzero contents are not padding of the first descriptor.
    write64(&mut dev, 0x088, 32);
    assert_queue(&mut dev, 32, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0xface);
}

#[test]
fn test_raw_mmio_dw_is_bit_11_and_remains_gated() {
    let (mut dev, gm, _) = make_queue(false, 0);
    write32(&mut dev, 0x018, 0);
    write64(&mut dev, 0x090, BASE | (1 << 11));
    put(&gm, 0, wait(0xbeef, SW), true);
    write64(&mut dev, 0x088, 32);
    assert_queue(&mut dev, 0, false); // QI disabled.
    write32(&mut dev, 0x018, 1 << 26);
    assert_queue(&mut dev, 0, true);
    assert_eq!(read64(&mut dev, 0x090), BASE | (1 << 11));
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
    assert_eq!(read64(&mut dev, 0x010), 0x00f0_10db);
    // Repair the queue width and resume using ordinary guest MMIO.
    write32(&mut dev, 0x018, 0);
    write64(&mut dev, 0x090, BASE);
    write64(&mut dev, 0x088, 16);
    write32(&mut dev, 0x018, 1 << 26);
    write32(&mut dev, 0x034, 1 << 4);
    assert_queue(&mut dev, 16, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0xbeef);
}

#[test]
fn test_queue_policy_uses_latched_mode() {
    let (mut dev, gm, _) = make_queue(false, 0);
    put(&gm, 0, wait(1, SW), false);
    write64(&mut dev, 0x020, 1 << 10); // Unlatched scalable mode.
    write64(&mut dev, 0x088, 16);
    assert_queue(&mut dev, 16, false);
    put(&gm, 16, wait(2, SW), false);
    // SRTP latches the mode; production does not yet advertise SMTS.
    write32(&mut dev, 0x018, (1 << 30) | (1 << 26));
    assert_queue(&mut dev, 16, true);
    write64(&mut dev, 0x088, 32);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
    write64(&mut dev, 0x020, 0);
    write32(&mut dev, 0x018, (1 << 30) | (1 << 26));
    write32(&mut dev, 0x034, 1 << 4);
    assert_queue(&mut dev, 32, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 2);
}

#[test]
fn test_invalid_tail_alignment_range_and_reserved_bits() {
    for dw in [false, true] {
        for tail in [1, 15, 4096, 4112, 1 << 19, 1 << 32, u64::MAX]
            .into_iter()
            .chain(if dw { Some(16) } else { None })
        {
            let (mut dev, gm, _) = make_queue(dw, 0);
            put(&gm, 0, wait(0xdead, SW), dw);
            submit(&mut dev, tail, dw);
            assert_queue(&mut dev, 0, true);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
            // Even a valid tail cannot cause fetches until IQE is cleared.
            let stride = if dw { 32 } else { 16 };
            submit(&mut dev, stride, dw);
            assert_queue(&mut dev, 0, true);
            recover(&mut dev, dw);
            assert_queue(&mut dev, stride, false);
        }
    }
}

#[test]
fn test_64bit_tail_is_validated_before_any_fetch() {
    let (mut dev, gm, _) = make_queue(false, 0);
    put(&gm, 0, wait(0xdead, SW), false);
    write64(&mut dev, 0x088, (1 << 32) | 16);
    assert_queue(&mut dev, 0, true);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
    write32(&mut dev, 0x08c, 0);
    write32(&mut dev, 0x034, 1 << 4);
    assert_queue(&mut dev, 16, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0xdead);
}

#[test]
fn test_invalid_head_is_bounded_and_mmio_head_is_read_only() {
    for dw in [false, true] {
        for head in [1, 4096, 1 << 19, u64::MAX]
            .into_iter()
            .chain(if dw { Some(16) } else { None })
        {
            let (mut dev, gm, _) = make_queue(dw, 0);
            write64(&mut dev, 0x080, head);
            assert_eq!(read64(&mut dev, 0x080), 0);
            dev.shared.state.write().iqh = IqhReg::from(head);
            submit(&mut dev, 0, dw);
            assert_queue(&mut dev, head, true);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
        }
    }
}

#[test]
fn test_invalid_base_reserved_bits_and_address_overflow() {
    for dw in [false, true] {
        for iqa in (3..11)
            .map(|bit| BASE | (1 << bit))
            .chain([0xffff_ffff_ffff_f001, 0xffff_ffff_ffff_f007])
        {
            let (mut dev, gm, _) = make_queue(dw, 0);
            dev.shared.state.write().iqa = IqaReg::from(iqa | ((dw as u64) << 11));
            submit(&mut dev, 0, dw);
            assert_queue(&mut dev, 0, true);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
        }
        // The last representable page doesn't overflow; its fetch must fail.
        let (mut dev, _, _) = make_queue(dw, 0);
        dev.shared.state.write().iqa = IqaReg::from(0xffff_ffff_ffff_f000 | ((dw as u64) << 11));
        assert!(InvalidationQueue::new(&dev.shared.state.read(), final_ecap()).is_ok());
        submit(&mut dev, if dw { 32 } else { 16 }, dw);
        assert_queue(&mut dev, 0, true);
    }
}

#[test]
fn test_fetch_failure_stops_at_unreadable_descriptor() {
    for dw in [false, true] {
        let (mut dev, gm, _) = make_queue(dw, 0);
        let stride = if dw { 32 } else { 16 };
        // Queue's last mapped entry succeeds, the next is outside guest RAM.
        let base = 0x9f000;
        {
            let mut state = dev.shared.state.write();
            state.iqa = IqaReg::from(base | ((dw as u64) << 11) | 1);
            state.iqh = IqhReg::from(4096 - stride);
        }
        let words = wait(0xabcd, SW);
        for (i, word) in words.iter().take(stride as usize / 8).enumerate() {
            gm.write_at(base + 4096 - stride + i as u64 * 8, &word.to_le_bytes())
                .unwrap();
        }
        submit(&mut dev, 4096 + stride, dw);
        assert_queue(&mut dev, 4096, true);
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0xabcd);
    }
}

#[test]
fn test_256bit_fetch_requires_readable_upper_half() {
    let gm = GuestMemory::allocate(0x2000)
        .subrange(0, 0x1010, false)
        .unwrap();
    let (mut dev, _) = IntelVtdDevice::new(
        gm.clone(),
        IntelVtdConfig {
            mmio_base: TEST_MMIO_BASE,
        },
        Arc::new(TestSignalMsi),
    );
    put(&gm, 0, [0x11, 0, 0, 0], false);
    assert!(gm.read_plain::<[u8; 16]>(BASE).is_ok());
    assert!(gm.read_plain::<[u8; 32]>(BASE).is_err());
    {
        let mut state = dev.shared.state.write();
        state.iqa = IqaReg::from(BASE | (1 << 11));
        state.gsts.set_qies(true);
    }
    run_future_queue(&mut dev, 32);
    assert_queue(&mut dev, 0, true);
}

#[test]
fn test_iqe_stops_at_invalid_descriptor_then_resumes() {
    for (dw, mode) in [(false, 0), (true, 0), (true, 1)] {
        // High Type bits must not alias valid low-nibble commands; no-op cache
        // commands also need semantic validation before advancing IQH.
        for bad in [0, 0x205, 0x407, 0xe01, 1, 2, 0x105, 0x85, 3, 8, 9] {
            let (mut dev, gm, _) = make_queue(dw, mode);
            let stride = if dw { 32 } else { 16 };
            put(&gm, 0, wait(1, SW), dw);
            put(&gm, stride, [bad, 0, 0, 0], dw);
            put(&gm, 2 * stride, wait(3, SW), dw);
            submit(&mut dev, 3 * stride, dw);
            assert_queue(&mut dev, stride, true);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
            put(&gm, stride, [0x11, 0, 0, 0], dw);
            submit(&mut dev, 3 * stride, dw);
            assert_queue(&mut dev, stride, true);
            recover(&mut dev, dw);
            assert_queue(&mut dev, 3 * stride, false);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 3);
        }
    }
}

#[test]
fn test_256bit_padding_and_reserved_fields_stop_processing() {
    for lo in [0x11, 0x12, 4, 5, 0x26, 7] {
        for word in 2..4 {
            for bit in 0..64 {
                let (mut dev, gm, _) = make_queue(true, 1);
                let mut words = [lo, 0, 0, 0];
                words[word] = 1 << bit;
                put(&gm, 0, words, true);
                put(&gm, 32, wait(1, SW), true);
                run_future_queue(&mut dev, 64);
                assert_queue(&mut dev, 0, true);
                assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
            }
        }
    }
}

#[test]
fn test_descriptor_mode_and_width_restrictions_in_queue() {
    for (mode, dw, descriptor) in [
        (0, false, 0x26),
        (0, true, 0x26),
        (0, false, 7),
        (0, true, 7),
        (1, false, 0x11),
        (2, false, 0x11),
        (2, true, 0x11),
        (3, true, 0x11),
    ] {
        let (mut dev, gm, _) = make_queue(dw, mode);
        put(&gm, 0, [descriptor, 0, 0, 0], dw);
        run_future_queue(&mut dev, if dw { 32 } else { 16 });
        assert_queue(&mut dev, 0, true);
    }
}

#[test]
fn test_empty_disabled_enable_and_disable_queue() {
    let (mut dev, gm, _) = make_queue(false, 0);
    // An empty queue does not fetch its invalid (zero) first entry.
    write64(&mut dev, 0x088, 0);
    assert_queue(&mut dev, 0, false);
    write32(&mut dev, 0x018, 0);
    put(&gm, 0, wait(1, SW), false);
    write64(&mut dev, 0x088, 16);
    assert_queue(&mut dev, 0, false);
    write32(&mut dev, 0x018, 1 << 26);
    assert_queue(&mut dev, 16, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
    write32(&mut dev, 0x018, 0);
    assert_queue(&mut dev, 0, false);
}

#[test]
fn test_ite_stops_fetch_and_rw1c_resumes() {
    let (mut dev, gm, _) = make_queue(false, 0);
    put(&gm, 0, wait(1, SW), false);
    dev.shared.state.write().fsts.set_ite(true);
    write64(&mut dev, 0x088, 16);
    assert_queue(&mut dev, 0, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
    write32(&mut dev, 0x034, 1 << 6);
    assert_queue(&mut dev, 16, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
}

#[test]
fn test_queue_fault_event_mask_and_unmask() {
    for masked in [false, true] {
        let (mut dev, _, msi) = make_queue(false, 0);
        write32(&mut dev, 0x03c, 0x51);
        write32(&mut dev, 0x040, 0xfee01000);
        write32(&mut dev, 0x038, (masked as u32) << 31);
        write64(&mut dev, 0x088, 16); // Zero descriptor is invalid.
        assert_queue(&mut dev, 0, true);
        assert_eq!(msi.events.lock().len(), usize::from(!masked));
        assert_eq!(FectlReg::from(read32(&mut dev, 0x038)).ip(), masked);
        write64(&mut dev, 0x088, 32); // Outstanding IQE doesn't generate another event.
        if masked {
            write32(&mut dev, 0x038, 0);
        }
        assert_eq!(&*msi.events.lock(), &[(0xfee01000, 0x51, 0)]);
    }
}

#[test]
fn test_wait_sw_if_fn_combinations() {
    for dw in [false, true] {
        for flags in 0..8 {
            let (mut dev, gm, msi) = make_queue(dw, 0);
            write32(&mut dev, 0x0a0, 0);
            write32(&mut dev, 0x0a4, 0x61);
            write32(&mut dev, 0x0a8, 0xfee02000);
            put(&gm, 0, wait(0x12345678, flags << 4), dw);
            submit(&mut dev, if dw { 32 } else { 16 }, dw);
            assert_queue(&mut dev, if dw { 32 } else { 16 }, false);
            let expected_status = if flags & 2 != 0 { 0x12345678 } else { 0 };
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), expected_status);
            assert_eq!(IcsReg::from(read32(&mut dev, 0x09c)).iwc(), flags & 1 != 0);
            let events = msi.events.lock();
            if flags & 1 != 0 {
                assert_eq!(&*events, &[(0xfee02000, 0x61, expected_status)]);
            } else {
                assert!(events.is_empty());
            }
        }
    }
}

#[test]
fn test_wait_interrupt_coalescing_and_pending_acknowledgement() {
    let (mut dev, gm, msi) = make_queue(false, 0);
    put(&gm, 0, wait(1, IF | SW), false);
    put(&gm, 16, wait(2, IF | SW), false);
    put(&gm, 32, wait(3, IF | SW), false);
    write64(&mut dev, 0x088, 16);
    assert!(IectlReg::from(read32(&mut dev, 0x0a0)).ip());
    assert!(msi.events.lock().is_empty());
    // Acknowledging IWC while masked cancels the pending interrupt.
    write32(&mut dev, 0x09c, 1);
    assert!(!IectlReg::from(read32(&mut dev, 0x0a0)).ip());
    write32(&mut dev, 0x0a0, 0);
    assert!(msi.events.lock().is_empty());
    write64(&mut dev, 0x088, 48);
    assert_eq!(msi.events.lock().len(), 1); // IWC=1 suppresses the second IF.
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 3); // SW still completes.
    write32(&mut dev, 0x09c, 1);
    write32(&mut dev, 0x0a0, 1 << 31);
    put(&gm, 48, wait(4, IF | SW), false);
    write64(&mut dev, 0x088, 64);
    assert!(IectlReg::from(read32(&mut dev, 0x0a0)).ip());
    write32(&mut dev, 0x0a0, 0);
    assert_eq!(msi.events.lock().len(), 2);
    assert_eq!(msi.events.lock()[1].2, 4);
    assert!(!IectlReg::from(read32(&mut dev, 0x0a0)).ip());
}

#[test]
fn test_status_write_failure_does_not_complete_or_advance() {
    for dw in [false, true] {
        let (mut dev, gm, msi) = make_queue(dw, 0);
        let mut bad = wait(1, SW | IF);
        bad[1] = 0xffff_ffff_ffff_fffc;
        put(&gm, 0, bad, dw);
        write32(&mut dev, 0x0a0, 0);
        submit(&mut dev, if dw { 32 } else { 16 }, dw);
        assert_queue(&mut dev, 0, true);
        assert_eq!(read32(&mut dev, 0x09c), 0);
        assert!(msi.events.lock().is_empty());
        put(&gm, 0, wait(2, SW | IF), dw);
        recover(&mut dev, dw);
        assert_queue(&mut dev, if dw { 32 } else { 16 }, false);
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 2);
        assert_eq!(msi.events.lock().len(), 1);
    }
}

struct CallbackRoute(Box<dyn Fn() + Send + Sync>);

impl iommu_common::RetranslateInterrupts for CallbackRoute {
    fn device_id(&self) -> u16 {
        0
    }
    fn retranslate(&self) {
        (self.0)();
    }
}

#[test]
fn test_route_retranslation_precedes_wait_and_can_reenter_state() {
    for dw in [false, true] {
        let (mut dev, gm, msi) = make_queue(dw, 0);
        let shared = dev.shared.clone();
        let irte = Irte {
            lo: IrteLo::new()
                .with_p(true)
                .with_vector(0x45)
                .with_dst(0x0100),
            hi: IrteHi::new(),
        };
        gm.write_at(0x80000, irte.as_bytes()).unwrap();
        {
            let mut state = shared.state.write();
            state.latched_irta = IrtaReg::new().with_irta(0x80);
            state.gsts.set_ires(true);
        }
        let initial =
            iommu_common::InterruptRemapper::remap_msi(&*shared, 0, 0xfee00010, 0).unwrap();
        assert_eq!(initial.1 & 0xff, 0x45);
        gm.write_at(
            0x80000,
            Irte {
                lo: irte.lo.with_vector(0x46),
                ..irte
            }
            .as_bytes(),
        )
        .unwrap();
        let route: Arc<dyn iommu_common::RetranslateInterrupts> =
            Arc::new(CallbackRoute(Box::new({
                let shared = shared.clone();
                let msi = msi.clone();
                let gm = gm.clone();
                move || {
                    let state = shared
                        .state
                        .try_read()
                        .expect("callback must not hold the write lock");
                    assert!(!state.ics.iwc());
                    drop(state);
                    assert!(shared.state.try_write().is_some());
                    let translated =
                        iommu_common::InterruptRemapper::remap_msi(&*shared, 0, 0xfee00010, 0)
                            .unwrap();
                    assert_eq!(translated.1 & 0xff, 0x46);
                    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
                    assert!(msi.events.lock().is_empty());
                    gm.write_plain(STATUS + 4, &1u32).unwrap();
                }
            })));
        iommu_common::InterruptRemapper::register_route(&*shared, &route);
        write32(&mut dev, 0x0a0, 0);
        let stride = if dw { 32 } else { 16 };
        put(&gm, 0, [4, 0, 0, 0], dw);
        put(&gm, stride, wait(2, SW | IF | FN), dw);
        submit(&mut dev, 2 * stride, dw);
        assert_queue(&mut dev, 2 * stride, false);
        assert_eq!(gm.read_plain::<u32>(STATUS + 4).unwrap(), 1);
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 2);
        assert_eq!(msi.events.lock().len(), 1);
    }
}

#[test]
fn test_fenced_wait_completes_before_following_callback() {
    let (mut dev, gm, msi) = make_queue(true, 1);
    let route: Arc<dyn iommu_common::RetranslateInterrupts> = Arc::new(CallbackRoute(Box::new({
        let gm = gm.clone();
        let msi = msi.clone();
        move || {
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
            assert_eq!(msi.events.lock().len(), 1);
        }
    })));
    iommu_common::InterruptRemapper::register_route(&*dev.shared, &route);
    write32(&mut dev, 0x0a0, 0);
    put(&gm, 0, wait(1, SW | IF | FN), true);
    put(&gm, 32, [4, 0, 0, 0], true);
    put(&gm, 64, wait(2, SW), true);
    run_future_queue(&mut dev, 96);
    assert_queue(&mut dev, 96, false);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 2);
}

#[test]
fn test_valid_interrupt_invalidation_completes_before_later_error() {
    let (mut dev, gm, _) = make_queue(true, 1);
    let route = Arc::new(CountingRoute {
        device_id: 0,
        retranslate_count: AtomicU32::new(0),
    });
    let route_dyn: Arc<dyn iommu_common::RetranslateInterrupts> = route.clone();
    iommu_common::InterruptRemapper::register_route(&*dev.shared, &route_dyn);
    put(&gm, 0, [4, 0, 0, 0], true);
    put(&gm, 32, [0, 0, 0, 0], true);
    put(&gm, 64, wait(1, SW), true);
    run_future_queue(&mut dev, 96);
    assert_queue(&mut dev, 32, true);
    assert_eq!(route.retranslate_count.load(Ordering::SeqCst), 1);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
    put(&gm, 32, [0x11, 0, 0, 0], true);
    recover(&mut dev, true);
    assert_queue(&mut dev, 96, false);
    assert_eq!(route.retranslate_count.load(Ordering::SeqCst), 1);
    assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
}

#[test]
fn test_inflight_dma_drains_before_invalidation_wait() {
    for dw in [false, true] {
        for write in [false, true] {
            let (mut dev, shared) = create_test_device_with_translation();
            let gm = shared.guest_memory.clone();
            {
                let mut state = shared.state.write();
                state.iqa = IqaReg::from(BASE | ((dw as u64) << 11));
                state.gsts.set_qies(true);
                if dw {
                    // PR3 implements scalable translation. Here a translation-
                    // disabled DMA still has to drain across scalable P_IOTLB.
                    state.gsts.set_tes(false);
                    state.latched_rtaddr = RtaddrReg::from(1 << 10);
                }
            }
            let stride = if dw { 32 } else { 16 };
            put(&gm, 0, [if dw { 0x26 } else { 0xd2 }, 0, 0, 0], dw);
            put(&gm, stride, wait(1, SW | FN), dw);
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::scope(|scope| {
                let translator = shared.translator();
                let dma_gm = &gm;
                let dma = scope.spawn(move || {
                    iommu_common::IommuTranslator::translate(
                        &translator,
                        0,
                        if dw { TARGET_GPA } else { 0 },
                        write,
                        |gpa| {
                            assert_eq!(gpa, TARGET_GPA);
                            entered_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                            if write {
                                dma_gm.write_plain(gpa, &0xfeedu32).unwrap();
                            } else {
                                assert_eq!(dma_gm.read_plain::<u32>(gpa).unwrap(), 0);
                            }
                        },
                    )
                    .unwrap();
                });
                entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(
                    shared.state.try_write().is_none(),
                    "DMA op must retain its read lock"
                );
                let invalidation = scope.spawn(|| {
                    submit(&mut dev, 2 * stride, dw);
                    done_tx.send(()).unwrap();
                });
                // parking_lot blocks new readers once this writer is queued.
                // No scheduling delay or sleep stands in for the drain check.
                let deadline = Instant::now() + Duration::from_secs(10);
                while shared.state.try_read().is_some() {
                    assert!(
                        Instant::now() < deadline,
                        "invalidation writer did not queue"
                    );
                    std::thread::yield_now();
                }
                assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
                release_tx.send(()).unwrap();
                dma.join().unwrap();
                invalidation.join().unwrap();
                done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            assert_queue(&mut dev, 2 * stride, false);
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 1);
            if write {
                assert_eq!(gm.read_plain::<u32>(TARGET_GPA).unwrap(), 0xfeed);
            }
        }
    }
}
