// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use chipset_device::interrupt::AcknowledgePicInterrupt;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use std::num::Wrapping;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

const PRIMARY_PIC_COMMAND_PORT: u16 = 0x20;
const PRIMARY_PIC_DATA_PORT: u16 = 0x21;
const SECONDARY_PIC_COMMAND_PORT: u16 = 0xa0;
const SECONDARY_PIC_DATA_PORT: u16 = 0xa1;
const PRIMARY_PIC_ELCR_PORT: u16 = 0x4d0;
const SECONDARY_PIC_ELCR_PORT: u16 = 0x4d1;

// x86 standard dictates we must use IRQ2 for cross-PIC communication.
const PIC_CHAIN_COMMUNICATION_IRQ: u8 = 2;

/// Mask for the IRQ vector within an individual PIC.
const IRQ_MASK: u8 = 0b111;

/// The spurious IRQ offset within an individual PIC.
const SPURIOUS_IRQ: u8 = 7;

#[derive(InspectMut)]
pub struct DualPic {
    // Runtime glue
    ready: LineInterrupt,
    #[inspect(iter_by_index)]
    port_io_regions: [Box<dyn ControlPortIoIntercept>; 3],

    // Runtime book-keeping
    stats: DualPicStats,

    // Volatile state
    #[inspect(flatten, with = r#"|x| inspect::iter_by_index(x).prefix("pic")"#)]
    pics: [Pic; 2],
}

#[derive(Inspect, Default)]
struct DualPicStats {
    #[inspect(iter_by_index)]
    interrupts_per_irq: [Counter; 16],
    interrupts: Counter,
}

impl DualPic {
    pub fn new(ready: LineInterrupt, port_io: &mut dyn RegisterPortIoIntercept) -> Self {
        let mut primary_region = port_io.new_io_region(
            "primary",
            (PRIMARY_PIC_COMMAND_PORT..=PRIMARY_PIC_DATA_PORT).len() as u16,
        );
        let mut secondary_region = port_io.new_io_region(
            "secondary",
            (SECONDARY_PIC_COMMAND_PORT..=SECONDARY_PIC_DATA_PORT).len() as u16,
        );
        let mut elcr_region = port_io.new_io_region(
            "edge_level_control",
            (PRIMARY_PIC_ELCR_PORT..=SECONDARY_PIC_ELCR_PORT).len() as u16,
        );

        primary_region.map(PRIMARY_PIC_COMMAND_PORT);
        secondary_region.map(SECONDARY_PIC_COMMAND_PORT);
        elcr_region.map(PRIMARY_PIC_ELCR_PORT);

        DualPic {
            pics: [Pic::new(true), Pic::new(false)],
            ready,
            port_io_regions: [primary_region, secondary_region, elcr_region],
            stats: Default::default(),
        }
    }

    /// Sync the interrupt outputs of each PIC with their connection (either the
    /// primary PIC or the CPU).
    fn sync_outputs(&mut self) {
        self.pics[0].set_irq(
            PIC_CHAIN_COMMUNICATION_IRQ,
            self.pics[1].interrupt_pending(),
        );
        self.ready.set_level(self.pics[0].interrupt_pending());
    }

    fn set_irq(&mut self, n: u8, high: bool) {
        if n >= 8 {
            self.pics[1].set_irq(n - 8, high);
        } else {
            self.pics[0].set_irq(n, high);
        }
        self.sync_outputs();
    }
}

impl AcknowledgePicInterrupt for DualPic {
    fn acknowledge_interrupt(&mut self) -> Option<u8> {
        let (requested, n) = self.pics[0].acknowledge_interrupt(&mut self.stats);
        let irq = requested.then(|| {
            if n & IRQ_MASK == PIC_CHAIN_COMMUNICATION_IRQ {
                let (requested, m) = self.pics[1].acknowledge_interrupt(&mut self.stats);
                assert!(requested, "pic1 ready was set");
                m
            } else {
                n
            }
        });
        // Sync to ensure the IRQ2 line gets lowered while the secondary PIC's
        // interrupt is in service.
        self.sync_outputs();
        irq
    }
}

impl LineInterruptTarget for DualPic {
    fn set_irq(&mut self, n: u32, high: bool) {
        self.set_irq(n as u8, high);
    }

    fn valid_lines(&self) -> &[std::ops::RangeInclusive<u32>] {
        // IRQ2 is used to cascade the secondary PIC to the primary PIC, so it
        // is not available for use.
        &[0..=1, 3..=15]
    }
}

impl ChangeDeviceState for DualPic {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        for pic in &mut self.pics {
            pic.reset(false);
        }
        self.sync_outputs();
    }
}

impl ChipsetDevice for DualPic {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
        Some(self)
    }

    fn supports_acknowledge_pic_interrupt(&mut self) -> Option<&mut dyn AcknowledgePicInterrupt> {
        Some(self)
    }
}

impl PortIoIntercept for DualPic {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        data[0] = match io_port {
            PRIMARY_PIC_COMMAND_PORT => self.pics[0].read_command(&mut self.stats),
            PRIMARY_PIC_DATA_PORT => self.pics[0].read_data(),
            SECONDARY_PIC_COMMAND_PORT => self.pics[1].read_command(&mut self.stats),
            SECONDARY_PIC_DATA_PORT => self.pics[1].read_data(),
            PRIMARY_PIC_ELCR_PORT => self.pics[0].elcr,
            SECONDARY_PIC_ELCR_PORT => self.pics[1].elcr,
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // Hyper-V-specific optimization: two-byte write to the command
        // port can be used to write to both command ports in a single
        // instruction.
        if data.len() == 2
            && (io_port == PRIMARY_PIC_COMMAND_PORT || io_port == SECONDARY_PIC_COMMAND_PORT)
        {
            let &[mut prim, mut sec] = data else {
                unreachable!()
            };
            if io_port == SECONDARY_PIC_COMMAND_PORT {
                (prim, sec) = (sec, prim);
            }
            self.pics[0].write_command(prim);
            self.pics[1].write_command(sec);
            self.sync_outputs();
            return IoResult::Ok;
        }

        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        match io_port {
            PRIMARY_PIC_COMMAND_PORT => self.pics[0].write_command(data[0]),
            PRIMARY_PIC_DATA_PORT => self.pics[0].write_data(data[0]),
            SECONDARY_PIC_COMMAND_PORT => self.pics[1].write_command(data[0]),
            SECONDARY_PIC_DATA_PORT => self.pics[1].write_data(data[0]),
            PRIMARY_PIC_ELCR_PORT => self.pics[0].elcr = data[0],
            SECONDARY_PIC_ELCR_PORT => self.pics[1].elcr = data[0],
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        self.sync_outputs();
        IoResult::Ok
    }
}

/// The PIC is controlled through two categories of commands:
/// ICWs: Initialization Control Words
/// OCWs: Operation Command Words
#[derive(Debug, Copy, Clone, Inspect)]
struct Pic {
    /// Our current stage of the initialization process.
    init: InitStage,

    /// Whether or not this PIC is the primary in the dual-pic chain.
    #[inspect(skip)]
    primary: bool,

    /// The Interrupt Vector base address, passed in ICW2.
    /// The least significant 3 bits must be 0.
    #[inspect(hex)]
    icw2: u8,

    /// Interrupt Mask Register, also called OCW1
    #[inspect(binary)]
    imr: u8,

    /// The most recently received OCW3
    ocw3: Ocw3,

    /// Interrupt Service Register
    #[inspect(binary)]
    isr: u8,

    /// Edge/Level Control Register
    #[inspect(binary)]
    elcr: u8,

    /// The current status of all the interrupt request lines
    #[inspect(binary)]
    lines: u8,

    /// Whether the line has been low since the last interrupt injection. Used
    /// to track whether an edge-triggered interrupt should be requested when
    /// its line is high.
    #[inspect(binary)]
    line_low_latch: u8,
}

#[derive(Copy, Clone, Debug, Inspect)]
enum InitStage {
    Uninitialized,
    ExpectingIcw2,
    ExpectingIcw3,
    ExpectingIcw4,
    Initialized,
}

impl Pic {
    fn new(primary: bool) -> Self {
        Pic {
            imr: 0,
            init: InitStage::Uninitialized,
            // This is a hack to help set_irq_in_service disambiguate pics before initialization.
            // Set this back to always 0 when that goes away.
            icw2: if primary { 0 } else { 8 },
            ocw3: Ocw3(0),
            isr: 0,
            elcr: 0,
            primary,
            lines: 0,
            line_low_latch: !0,
        }
    }

    /// Resets the PIC state, preserving the line state. If `during_icw1`,
    /// preserve ELCR.
    fn reset(&mut self, during_icw1: bool) {
        *self = Self {
            lines: self.lines,
            elcr: if during_icw1 { self.elcr } else { 0 },
            ..Self::new(self.primary)
        };
    }

    fn irr(&self) -> u8 {
        self.lines & (self.elcr | self.line_low_latch)
    }

    fn set_irq(&mut self, n: u8, high: bool) {
        let bit = 1 << n;
        if high {
            if !matches!(self.init, InitStage::Initialized) {
                tracelimit::warn_ratelimited!(
                    primary = self.primary,
                    ?n,
                    "interrupt request sent to uninitialized PIC"
                );
            }
            self.lines |= bit;
        } else {
            self.lines &= !bit;
            self.line_low_latch |= bit;
        }
    }

    fn ready_vec(&self) -> u8 {
        // Restrict to interrupts with higher priority than the highest priority
        // IRQ already being serviced.
        let isr = Wrapping(self.isr);
        let highest_isr = isr & -isr;
        let higher_not_isr = highest_isr - Wrapping(1);
        // Restrict to requested interrupts that are not masked.
        self.irr() & !self.imr & higher_not_isr.0
    }

    fn interrupt_pending(&self) -> bool {
        self.ready_vec() != 0
    }

    fn pending_line(&self) -> Option<u8> {
        let m = self.ready_vec();
        if m != 0 {
            // Find the highest priority IRQ
            Some(m.trailing_zeros() as u8)
        } else {
            None
        }
    }

    fn acknowledge_interrupt(&mut self, stats: &mut DualPicStats) -> (bool, u8) {
        if !matches!(self.init, InitStage::Initialized) {
            tracelimit::warn_ratelimited!(
                primary = self.primary,
                "interrupt servicing sent to uninitialized PIC"
            );
        }

        let (requested, irq) = if let Some(n) = self.pending_line() {
            let bit = 1 << n;
            // Clear the edge latch so that the line must be low again before
            // another interrupt is injected.
            self.line_low_latch &= !bit;
            // Set in service.
            self.isr |= bit;
            stats.interrupts.increment();
            stats.interrupts_per_irq[if self.primary { 0 } else { 8 } + n as usize].increment();
            (true, n)
        } else {
            // spurious interrupt
            (false, SPURIOUS_IRQ)
        };

        // Combine the IRQ with the base address to construct the full interrupt
        // service routine address.
        assert!(self.icw2 & IRQ_MASK == 0);
        (requested, self.icw2 | irq)
    }

    fn eoi(&mut self, n: Option<u8>) {
        tracing::trace!(primary = self.primary, n, "eoi");
        let bit = match n {
            Some(level) => 1 << level,
            // On non-specific EOIs, find the highest priority interrupt in service
            None => self.isr & self.isr.wrapping_neg(),
        };

        self.isr &= !bit;
    }

    fn read_command(&mut self, stats: &mut DualPicStats) -> u8 {
        if self.ocw3.p() {
            self.ocw3.set_p(false);
            let (int, irq) = self.acknowledge_interrupt(stats);
            ((int as u8) << 7) | (irq & IRQ_MASK)
        } else if self.ocw3.rr() {
            if self.ocw3.ris() {
                self.isr
            } else {
                self.irr()
            }
        } else {
            0
        }
    }

    fn read_data(&self) -> u8 {
        self.imr
    }

    fn write_command(&mut self, data: u8) {
        const INIT_BIT: u8 = 0b10000;
        const COMMAND_BIT: u8 = 0b1000;

        if data & INIT_BIT != 0 {
            // ICW1
            // We do not support Single PIC mode or Level Triggered mode.
            // We require IC4 to be set so we can be put in x86 mode in ICW4.
            if data != 0b00010001 {
                tracelimit::error_ratelimited!(primary = self.primary, ?data, "unsupported ICW1");
            }

            self.reset(true);
            self.init = InitStage::ExpectingIcw2;
        } else {
            if !matches!(self.init, InitStage::Initialized) {
                tracelimit::warn_ratelimited!(
                    primary = self.primary,
                    ?data,
                    "OCW sent to uninitialized PIC"
                );
            }
            if data & COMMAND_BIT == 0 {
                let ocw2 = Ocw2(data);

                match (ocw2.r(), ocw2.sl(), ocw2.eoi()) {
                    (true, _, _) | (false, false, false) => {
                        tracelimit::error_ratelimited!(
                            primary = self.primary,
                            ?data,
                            "unsupported OCW2"
                        )
                    }
                    (false, true, true) => self.eoi(Some(ocw2.level())),
                    (false, false, true) => self.eoi(None),
                    (false, true, false) => {} // No-op
                }
            } else {
                self.ocw3 = Ocw3(data);
                // We do not support Special Mask Mode.
                if self.ocw3.esmm() || self.ocw3.smm() {
                    tracelimit::error_ratelimited!(
                        primary = self.primary,
                        ?data,
                        "unsupported OCW3"
                    );
                }
            }
        }
    }

    fn write_data(&mut self, data: u8) {
        match self.init {
            InitStage::Uninitialized | InitStage::Initialized => {
                self.imr = data; // OCW1
            }
            InitStage::ExpectingIcw2 => {
                if data & IRQ_MASK != 0 {
                    tracelimit::error_ratelimited!(primary = self.primary, ?data, "invalid ICW2");
                }
                self.icw2 = data & !IRQ_MASK;
                self.init = InitStage::ExpectingIcw3;
            }
            InitStage::ExpectingIcw3 => {
                // x86 standard dictates we must use IRQ2 for cross-PIC communication.
                if self.primary {
                    if data != (1 << PIC_CHAIN_COMMUNICATION_IRQ) {
                        tracelimit::error_ratelimited!(
                            primary = self.primary,
                            ?data,
                            "invalid primary ICW3"
                        );
                    }
                } else {
                    if data != PIC_CHAIN_COMMUNICATION_IRQ {
                        tracelimit::error_ratelimited!(
                            primary = self.primary,
                            ?data,
                            "invalid secondary ICW3"
                        );
                    }
                }

                self.init = InitStage::ExpectingIcw4;
            }
            InitStage::ExpectingIcw4 => {
                // We do not support any advanced operating modes controlled by ICW4.
                // We require being put into x86 mode.
                if data != 1 {
                    // Linux sends an ICW4 of 3 during boot, then very quickly overwrites it
                    if data == 3 {
                        tracing::debug!(
                            primary = self.primary,
                            "got ICW4 of 3, this is expected for Linux boot but not any other time"
                        );
                    } else {
                        tracelimit::error_ratelimited!(
                            primary = self.primary,
                            ?data,
                            "unsupported ICW4"
                        );
                    }
                };
                self.init = InitStage::Initialized;
            }
        }
    }
}

#[bitfield(u8)]
/// Operation Command Word 2
struct Ocw2 {
    /// Interrupt level
    #[bits(3)]
    level: u8,

    #[bits(2)]
    _command: u8,

    /// End of Interrupt
    eoi: bool,

    /// Selection
    sl: bool,

    /// Rotation
    r: bool,
}

#[derive(Inspect)]
#[bitfield(u8)]
/// Operation Command Word 3
struct Ocw3 {
    /// Read Selector
    ris: bool,

    /// Read Register
    rr: bool,

    /// Polling
    p: bool,

    #[bits(2)]
    _command: u8,

    /// Special Mask Mode
    smm: bool,

    /// Enable Special Mask Mode
    esmm: bool,

    _reserved: bool,
}

mod save_restore {
    use super::DualPic;
    use super::InitStage;
    use super::Ocw3;
    use super::Pic;
    use super::IRQ_MASK;
    use thiserror::Error;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.pic")]
        pub struct SavedState {
            #[mesh(1)]
            pub(super) primary: SavedPic,
            #[mesh(2)]
            pub(super) secondary: SavedPic,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.pic")]
        pub struct SavedPic {
            #[mesh(1)]
            pub init: SavedInitStage,
            #[mesh(2)]
            pub icw2: u8,
            #[mesh(3)]
            pub imr: u8,
            #[mesh(4)]
            pub ocw3: u8,
            #[mesh(5)]
            pub isr: u8,
            #[mesh(6)]
            pub elcr: u8,
            #[mesh(7)]
            pub line_low_latch: u8,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.pic")]
        pub enum SavedInitStage {
            #[mesh(1)]
            Uninitialized,
            #[mesh(2)]
            ExpectingIcw2,
            #[mesh(3)]
            ExpectingIcw3,
            #[mesh(4)]
            ExpectingIcw4,
            #[mesh(5)]
            Initialized,
        }
    }

    #[derive(Debug, Error)]
    enum Error {
        #[error("invalid icw2 value {0:#x}")]
        InvalidIcw2(u8),
    }

    impl state::SavedPic {
        fn restore(self, pic: &mut Pic) -> Result<(), Error> {
            let Self {
                init,
                icw2,
                imr,
                ocw3,
                isr,
                elcr,
                line_low_latch,
            } = self;
            pic.init = match init {
                state::SavedInitStage::Uninitialized => InitStage::Uninitialized,
                state::SavedInitStage::ExpectingIcw2 => InitStage::ExpectingIcw2,
                state::SavedInitStage::ExpectingIcw3 => InitStage::ExpectingIcw3,
                state::SavedInitStage::ExpectingIcw4 => InitStage::ExpectingIcw4,
                state::SavedInitStage::Initialized => InitStage::Initialized,
            };
            if icw2 & IRQ_MASK != 0 {
                return Err(Error::InvalidIcw2(icw2));
            }
            pic.icw2 = icw2;
            pic.imr = imr;
            pic.ocw3 = Ocw3(ocw3);
            pic.isr = isr;
            pic.elcr = elcr;
            pic.line_low_latch = line_low_latch;
            Ok(())
        }

        fn save(pic: &Pic) -> Self {
            let &Pic {
                init,
                primary: _,
                icw2,
                imr,
                ocw3,
                isr,
                elcr,
                lines: _,
                line_low_latch,
            } = pic;
            Self {
                init: match init {
                    InitStage::Uninitialized => state::SavedInitStage::Uninitialized,
                    InitStage::ExpectingIcw2 => state::SavedInitStage::ExpectingIcw2,
                    InitStage::ExpectingIcw3 => state::SavedInitStage::ExpectingIcw3,
                    InitStage::ExpectingIcw4 => state::SavedInitStage::ExpectingIcw4,
                    InitStage::Initialized => state::SavedInitStage::Initialized,
                },
                icw2,
                imr,
                ocw3: ocw3.0,
                isr,
                elcr,
                line_low_latch,
            }
        }
    }

    impl SaveRestore for DualPic {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                ready: _,
                stats: _,
                port_io_regions: _,
                pics: [primary, secondary],
            } = &self;

            Ok(state::SavedState {
                primary: state::SavedPic::save(primary),
                secondary: state::SavedPic::save(secondary),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { primary, secondary } = state;
            primary
                .restore(&mut self.pics[0])
                .map_err(|err| RestoreError::Other(err.into()))?;
            secondary
                .restore(&mut self.pics[1])
                .map_err(|err| RestoreError::Other(err.into()))?;
            self.sync_outputs();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::pio::ExternallyManagedPortIoIntercepts;
    use chipset_device::pio::PortIoIntercept;
    use vmcore::line_interrupt::test_helpers::TestLineInterruptTarget;

    const IV_BASE: u8 = 0x30;

    fn create_pic() -> (impl Fn() -> bool, DualPic) {
        let ready = TestLineInterruptTarget::new_arc();
        let mut pic = DualPic::new(
            LineInterrupt::new_with_target("ready", ready.clone(), 0),
            &mut ExternallyManagedPortIoIntercepts,
        );

        // Initialization sequence copied by logging what Linux Direct does:
        pic.io_write(0x20, &[0x11]).unwrap();
        pic.io_write(0x21, &[IV_BASE]).unwrap(); // Primary ICW2
        pic.io_write(0x21, &[0x04]).unwrap();
        pic.io_write(0x21, &[0x01]).unwrap();
        pic.io_write(0xa0, &[0x11]).unwrap();
        pic.io_write(0xa1, &[IV_BASE + 8]).unwrap(); // Secondary ICW2
        pic.io_write(0xa1, &[0x02]).unwrap();
        pic.io_write(0xa1, &[0x01]).unwrap();

        (move || ready.is_high(0), pic)
    }

    fn send_eoi(pic: &mut DualPic, consts: PicTestConstants) {
        let (v, _) = consts;
        match v {
            0..=7 => pic
                .io_write(
                    PRIMARY_PIC_COMMAND_PORT,
                    &[Ocw2::new().with_eoi(true).with_sl(true).with_level(v).0],
                )
                .unwrap(),
            8..=15 => {
                pic.io_write(
                    PRIMARY_PIC_COMMAND_PORT,
                    &[Ocw2::new()
                        .with_eoi(true)
                        .with_sl(true)
                        .with_level(PIC_CHAIN_COMMUNICATION_IRQ)
                        .0],
                )
                .unwrap();
                pic.io_write(
                    SECONDARY_PIC_COMMAND_PORT,
                    &[Ocw2::new().with_eoi(true).with_sl(true).with_level(v - 8).0],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_create_pic() {
        let (ready, pic) = create_pic();
        assert!(matches!(pic.pics[0].init, InitStage::Initialized));
        assert!(matches!(pic.pics[1].init, InitStage::Initialized));
        assert!(!ready());
    }

    // (Vector, pics index)
    type PicTestConstants = (u8, usize);
    const PRIMARY: PicTestConstants = (0, 0);
    const SECONDARY: PicTestConstants = (8, 1);

    #[test]
    fn test_edge_interrupt_primary() {
        test_edge_interrupt(PRIMARY);
    }

    #[test]
    fn test_edge_interrupt_secondary() {
        test_edge_interrupt(SECONDARY);
    }

    fn test_edge_interrupt(consts: PicTestConstants) {
        let (ready, mut pic) = create_pic();
        let (v, i) = consts;

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);
        pic.set_irq(v, false);
        assert!(pic.pics[i].irr() == 0b0);
        let pending = pic.acknowledge_interrupt();
        assert!(pending.is_none());

        assert!(!ready());

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);

        let pending = pic.acknowledge_interrupt();
        assert_eq!(pending, Some(IV_BASE + v));
        assert!(pic.pics[i].irr() == 0);
        assert!(pic.pics[i].isr == 0b1);

        send_eoi(&mut pic, consts);

        assert!(pic.pics[i].irr() == 0);
        assert!(pic.pics[i].isr == 0);
        let pending = pic.acknowledge_interrupt();
        assert!(pending.is_none());

        assert!(!ready());

        pic.set_irq(v, true);
        assert!(pic.pics[i].irr() == 0b0);
        pic.set_irq(v, false);
        pic.set_irq(v, true);
        assert!(pic.pics[i].irr() == 0b1);
        assert!(ready());

        pic.set_irq(v, false);
        assert!(pic.pics[i].irr() == 0b0);
    }

    #[test]
    fn test_level_interrupts_primary() {
        test_level_interrupts(PRIMARY);
    }

    #[test]
    fn test_level_interrupts_secondary() {
        test_level_interrupts(SECONDARY);
    }

    fn test_level_interrupts(consts: PicTestConstants) {
        let (ready, mut pic) = create_pic();
        let (v, i) = consts;

        pic.io_write(
            match i {
                0 => PRIMARY_PIC_ELCR_PORT,
                1 => SECONDARY_PIC_ELCR_PORT,
                _ => unreachable!(),
            },
            &[0b1],
        )
        .unwrap();

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);
        pic.set_irq(v, false);
        assert!(pic.pics[i].irr() == 0b0);
        let pending = pic.acknowledge_interrupt();

        // No spurious interrupt is possible with the current interface.
        assert!(pending.is_none());

        assert!(!ready());

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);

        let pending = pic.acknowledge_interrupt();
        assert_eq!(pending, Some(IV_BASE + v));
        assert!(pic.pics[i].irr() == 0b1);
        assert!(pic.pics[i].isr == 0b1);

        send_eoi(&mut pic, consts);
        assert!(pic.pics[i].irr() == 0b1);
        assert!(pic.pics[i].isr == 0);

        let pending = pic.acknowledge_interrupt();
        assert_eq!(pending, Some(IV_BASE + v));
        pic.set_irq(v, false);
        send_eoi(&mut pic, consts);
        assert!(pic.pics[i].irr() == 0);
        let pending = pic.acknowledge_interrupt();
        assert!(pending.is_none());
    }

    #[test]
    fn test_multiple_edge_interrupt_primary() {
        test_multiple_edge_interrupt(PRIMARY);
    }

    #[test]
    fn test_multiple_edge_interrupt_secondary() {
        test_multiple_edge_interrupt(SECONDARY);
    }

    fn test_multiple_edge_interrupt(consts: PicTestConstants) {
        let (ready, mut pic) = create_pic();
        let (v, i) = consts;

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);
        pic.set_irq(v, false);
        assert!(pic.pics[i].irr() == 0b0);

        pic.set_irq(v + 1, true);
        assert!(pic.pics[i].irr() == 0b10);
        pic.set_irq(v + 1, false);
        assert!(pic.pics[i].irr() == 0b00);

        assert!(!ready());

        pic.set_irq(v, true);
        assert!(ready());
        assert!(pic.pics[i].irr() == 0b1);
        pic.set_irq(v + 1, true);
        assert!(pic.pics[i].irr() == 0b11);

        let pending = pic.acknowledge_interrupt();
        assert_eq!(pending, Some(IV_BASE + v));
        assert!(pic.pics[i].irr() == 0b10);
        assert!(pic.pics[i].isr == 0b01);

        send_eoi(&mut pic, consts);
        assert!(pic.pics[i].irr() == 0b10);
        assert!(pic.pics[i].isr == 0b00);
        assert!(ready());
        let pending = pic.acknowledge_interrupt();
        assert_eq!(pending, Some(IV_BASE + 1 + v));
        assert!(!ready());
    }

    #[test]
    fn test_non_specific_eois() {
        let (_, mut pic) = create_pic();

        pic.set_irq(5, true);
        assert_eq!(pic.acknowledge_interrupt(), Some(IV_BASE + 5));

        pic.set_irq(3, true);
        assert_eq!(pic.acknowledge_interrupt(), Some(IV_BASE + 3));

        pic.set_irq(1, true);
        assert_eq!(pic.acknowledge_interrupt(), Some(IV_BASE + 1));

        assert_eq!(pic.pics[0].isr, 0b101010);

        pic.io_write(
            PRIMARY_PIC_COMMAND_PORT,
            &[Ocw2::new().with_eoi(true).with_sl(false).0],
        )
        .unwrap();

        assert_eq!(pic.pics[0].isr, 0b101000);
    }
}
