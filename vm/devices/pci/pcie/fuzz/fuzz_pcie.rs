// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Unstructured;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use memory_range::MemoryRange;
use pci_bus::GenericPciBusDevice;
use pci_core::msi::MsiTarget;
use pcie::PciePortSettings;
use pcie::root::GenericPcieRootComplex;
use pcie::root::GenericPcieRootPortDefinition;
use pcie::switch::GenericPcieSwitch;
use pcie::switch::GenericPcieSwitchDefinition;
use pcie::test_helpers::TestPcieMmioRegistration;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use vmcore::device_state::ChangeDeviceState;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

const MAX_DEFER_POLL_COUNT: usize = 512;

/// A mock PCI endpoint that returns fuzzer-driven data for reads.
struct FuzzEndpoint {
    /// Value returned for all config reads.
    read_value: u32,
    /// Whether to return None (device offline) instead of a result.
    offline: bool,
}

impl GenericPciBusDevice for FuzzEndpoint {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> Option<IoResult> {
        if self.offline {
            return None;
        }
        *value = self.read_value;
        Some(IoResult::Ok)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
        if self.offline {
            return None;
        }
        Some(IoResult::Ok)
    }
}

/// Adapts a `GenericPcieSwitch` to the `GenericPciBusDevice` trait so it can be
/// attached to a root port as a downstream device.
struct SwitchAdapter(GenericPcieSwitch);

impl GenericPciBusDevice for SwitchAdapter {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
        Some(self.0.pci_cfg_read(offset, value))
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
        Some(self.0.pci_cfg_write(offset, value))
    }

    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        Some(
            self.0
                .pci_cfg_read_with_routing(secondary_bus, target_bus, function, offset, value),
        )
    }

    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        Some(
            self.0
                .pci_cfg_write_with_routing(secondary_bus, target_bus, function, offset, value),
        )
    }
}

/// Topology configurations selected by the fuzzer. Each variant uses fixed,
/// small parameters to avoid wasting entropy on setup.
#[derive(arbitrary::Arbitrary, Debug)]
enum Topology {
    /// 2 root ports, no devices attached.
    RootPortsOnly,
    /// 2 root ports, one with a mock endpoint.
    WithEndpoint { read_value: u32, offline: bool },
    /// 2 root ports, one with a switch (2 downstream ports).
    WithSwitch,
    /// 2 hotplug-enabled root ports, one with a mock endpoint.
    Hotplug { read_value: u32 },
}

struct FuzzRootComplex {
    rc: GenericPcieRootComplex,
}

impl FuzzRootComplex {
    fn new(topology: &Topology, ecam_range: MemoryRange) -> Self {
        let hotplug = matches!(topology, Topology::Hotplug { .. });

        let mut register_mmio = TestPcieMmioRegistration {};
        let port_defs: Vec<GenericPcieRootPortDefinition> = (0..NUM_PORTS)
            .map(|i| GenericPcieRootPortDefinition {
                name: format!("rp{}", i).into(),
                hotplug,
                settings: PciePortSettings::default(),
            })
            .collect();
        let msi_conn =
            pci_core::msi::MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);
        let rc = GenericPcieRootComplex::new(
            &mut register_mmio,
            START_BUS,
            END_BUS,
            ecam_range,
            port_defs,
            msi_conn.target(),
        );
        Self { rc }
    }

    pub fn add_pcie_device(
        &mut self,
        port: u8,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        self.rc.add_pcie_device(port, name, dev)
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> Result<(), IoError> {
        match self.rc.mmio_read(addr, data) {
            IoResult::Defer(t) => {
                // Poll the deferred read and panic if it doesn't complete. This keeps the
                // fuzzing logic simple by avoiding the need to track pending deferred operations.
                self.defer_read_now_or_never(t, data)
                    .expect("deferred read should complete after polling");
                Ok(())
            }
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
        }
    }

    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) -> Result<(), IoError> {
        match self.rc.mmio_write(addr, data) {
            IoResult::Defer(t) => {
                // Poll the deferred write and panic if it doesn't complete. This keeps the
                // fuzzing logic simple by avoiding the need to track pending deferred operations.
                self.defer_write_now_or_never(t)
                    .expect("deferred write should complete after polling");
                Ok(())
            }
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
        }
    }

    pub async fn reset(&mut self) {
        self.rc.reset().await;
    }

    /// Poll a deferred read, panic if it isn't complete afterwards.
    fn defer_read_now_or_never(
        &mut self,
        mut t: DeferredToken,
        data: &mut [u8],
    ) -> Result<(), IoError> {
        let mut cx = Context::from_waker(Waker::noop());
        let dev = self
            .rc
            .supports_poll_device()
            .expect("objects returning a DeferredToken support polling");
        // Some devices might limit the amount of work they perform in a single poll
        // even though forward progress is still possible. We poll the device multiple times
        // to let these actions complete. If the action is still pending after all these polls
        // we know that something is actually wrong.
        for _ in 0..MAX_DEFER_POLL_COUNT {
            dev.poll_device(&mut cx);
            match t.poll_read(&mut cx, data) {
                Poll::Ready(r) => return r,
                Poll::Pending => {}
            }
        }
        if MAX_DEFER_POLL_COUNT == 0 {
            panic!("Device operation returned a deferred read.");
        } else {
            panic!(
                "Device operation returned a deferred read that didn't complete after {} polls",
                MAX_DEFER_POLL_COUNT
            )
        }
    }

    /// Poll a deferred write, panic if it isn't complete afterwards.
    fn defer_write_now_or_never(&mut self, mut t: DeferredToken) -> Result<(), IoError> {
        let mut cx = Context::from_waker(Waker::noop());
        let dev = self
            .rc
            .supports_poll_device()
            .expect("objects returning a DeferredToken support polling");
        // Some devices might limit the amount of work they perform in a single poll
        // even though forward progress is still possible. We poll the device multiple times
        // to let these actions complete. If the action is still pending after all these polls
        // we know that something is actually wrong.
        for _ in 0..MAX_DEFER_POLL_COUNT {
            dev.poll_device(&mut cx);
            match t.poll_write(&mut cx) {
                Poll::Ready(r) => return r,
                Poll::Pending => {}
            }
        }
        if MAX_DEFER_POLL_COUNT == 0 {
            panic!("Device operation returned a deferred write.");
        } else {
            panic!(
                "Device operation returned a deferred write that didn't complete after {} polls",
                MAX_DEFER_POLL_COUNT
            )
        }
    }
}

// Fixed constants for the fuzzer — small bus range to keep ECAM small.
const START_BUS: u8 = 0;
const END_BUS: u8 = 3;
const ECAM_BASE: u64 = 0;
const NUM_PORTS: u8 = 2;

fn ecam_size(start_bus: u8, end_bus: u8) -> u64 {
    let bus_count = (end_bus as u64) - (start_bus as u64) + 1;
    bus_count * 256 * 4096
}

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let topology: Topology = u.arbitrary()?;
    fuzz_eprintln!("topology: {:?}", topology);

    // Use a small bus range to keep ECAM size (and thus memory usage) small.
    let ecam_len = ecam_size(START_BUS, END_BUS);
    let ecam_range = MemoryRange::new(ECAM_BASE..ECAM_BASE + ecam_len);

    let mut rc = FuzzRootComplex::new(&topology, ecam_range);

    // Port keys: port 0 = device_number 0, port 1 = device_number 8 (1 << 3)
    let port0_key: u8 = 0;

    match topology {
        Topology::RootPortsOnly => {}
        Topology::WithEndpoint {
            read_value,
            offline,
        } => {
            let ep = FuzzEndpoint {
                read_value,
                offline,
            };
            rc.add_pcie_device(port0_key, "ep0", Box::new(ep))
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        }
        Topology::WithSwitch => {
            let switch = GenericPcieSwitch::new(GenericPcieSwitchDefinition {
                name: "sw0".into(),
                downstream_port_count: 2,
                hotplug: false,
                msi_target: MsiTarget::disconnected(),
                dsp_settings: PciePortSettings::default(),
            });
            rc.add_pcie_device(port0_key, "sw0", Box::new(SwitchAdapter(switch)))
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        }
        Topology::Hotplug { read_value } => {
            let ep = FuzzEndpoint {
                read_value,
                offline: false,
            };
            rc.add_pcie_device(port0_key, "hp-ep0", Box::new(ep))
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        }
    }

    // Program bus numbers on root port 0 so config forwarding is reachable.
    // Without this, all downstream accesses hit the "bus range 0..=0" early
    // return and the routing code is never exercised.
    //
    // Root port 0 is at ECAM offset 0 (device 0, function 0, bus 0).
    // Secondary bus number register is at config space offset 0x19.
    // Subordinate bus number register is at config space offset 0x1A.
    // We assign secondary=1, subordinate=3 so buses 1-3 route through port 0.
    let rp0_ecam_base = ECAM_BASE; // bus 0, device 0, function 0
    rc.mmio_write(rp0_ecam_base + 0x19, &[1u8]).unwrap(); // secondary = 1
    rc.mmio_write(rp0_ecam_base + 0x1A, &[END_BUS]).unwrap(); // subordinate = END_BUS

    // For the switch topology, also program the switch's upstream port bus
    // numbers so its routing logic is reachable. The switch's upstream port
    // config space is on bus 1 (the secondary bus of root port 0), device 0.
    // Then program downstream port 0's bus numbers to enable deep routing.
    if matches!(topology, Topology::WithSwitch) {
        let switch_ecam = ECAM_BASE + (256 * 4096); // bus 1, dev 0, fn 0
        rc.mmio_write(switch_ecam + 0x19, &[2u8]).unwrap(); // switch secondary = 2
        rc.mmio_write(switch_ecam + 0x1A, &[END_BUS]).unwrap(); // switch subordinate = END_BUS

        // Program downstream port 0 (bus 2, device 0) with secondary=3
        let ds_port0_ecam = ECAM_BASE + (2u64 * 256 * 4096); // bus 2, dev 0, fn 0
        rc.mmio_write(ds_port0_ecam + 0x19, &[3u8]).unwrap(); // ds port secondary = 3
        rc.mmio_write(ds_port0_ecam + 0x1A, &[END_BUS]).unwrap(); // ds port subordinate = END_BUS
    }

    // Drive MMIO reads and writes directly on the root complex.
    // The ECAM region spans [ECAM_BASE .. ECAM_BASE + ecam_len).
    while !u.is_empty() {
        let action: FuzzAction = u.arbitrary()?;
        fuzz_eprintln!("{:x?}", action);

        match action {
            FuzzAction::MmioRead { offset, size } => {
                let addr = ECAM_BASE + (offset as u64 % ecam_len);
                let mut buf = [0u8; 16];
                let len = size.byte_count();
                let _ = rc.mmio_read(addr, &mut buf[..len]);
            }
            FuzzAction::MmioWrite { offset, size, data } => {
                let addr = ECAM_BASE + (offset as u64 % ecam_len);
                let len = size.byte_count();
                let _ = rc.mmio_write(addr, &data.to_le_bytes()[..len]);
            }
            FuzzAction::Reset => {
                // Use a dummy async context — reset needs async but the PCIe
                // root complex's reset is synchronous internally.
                pal_async::DefaultPool::run_with(async |_driver| {
                    rc.reset().await;
                });
            }
        }
    }

    Ok(())
}

/// Access sizes the fuzzer can select. Sizes 1, 2, and 4 are the valid ECAM
/// config access sizes. The rest exercise rejection paths — they are real
/// MMIO access sizes on some platforms but are always rejected by ECAM.
#[derive(arbitrary::Arbitrary, Debug)]
enum AccessSize {
    One,
    Two,
    Three, // always rejected by ECAM
    Four,
    Eight,   // valid MMIO size on x86/aarch64, but rejected by ECAM
    Sixteen, // valid MMIO size on aarch64 (LDR Q), but rejected by ECAM
}

impl AccessSize {
    fn byte_count(&self) -> usize {
        match self {
            AccessSize::One => 1,
            AccessSize::Two => 2,
            AccessSize::Three => 3,
            AccessSize::Four => 4,
            AccessSize::Eight => 8,
            AccessSize::Sixteen => 16,
        }
    }
}

#[derive(arbitrary::Arbitrary, Debug)]
enum FuzzAction {
    MmioRead {
        offset: u32,
        size: AccessSize,
    },
    MmioWrite {
        offset: u32,
        size: AccessSize,
        data: u128, // large enough for 16-byte writes
    },
    Reset,
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
