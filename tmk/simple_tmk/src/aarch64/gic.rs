// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

///! GIC interface
///!
///! Enough functionality to send an SGI.
// TODO: dedup with aarch64defs
use super::device_register::DeviceRegister;
use super::device_register::DeviceRegisterArray;
use super::device_register::DeviceRegisterArraySpec;
use super::device_register::DeviceRegisterSpec;
use aarch64defs::gic::GICR_FRAME_SIZE;
use aarch64defs::gic::GicdRegister;
use aarch64defs::gic::GicrRdRegister;
use aarch64defs::gic::GicrSgiRegister;
use bitfield_struct::bitfield;

const GICD_CTLR_OFFSET: usize = GicdRegister::CTLR.0 as usize;
const GICD_TYPER_OFFSET: usize = GicdRegister::TYPER.0 as usize;
const GICD_PIDR2_OFFSET: usize = GicdRegister::PIDR2.0 as usize;
const GICD_ICENABLER_OFFSET: usize = GicdRegister::ICENABLER0.0 as usize;
const GICD_ICPENDR_OFFSET: usize = GicdRegister::ICPENDR0.0 as usize;
const GICD_IGROUPR_OFFSET: usize = GicdRegister::IGROUPR0.0 as usize;
const GICD_IGRPMODR_OFFSET: usize = GicdRegister::IGRPMODR.0 as usize;
const GICD_IROUTER_OFFSET: usize = GicdRegister::IROUTER0.0 as usize;

const GICR_CTLR_OFFSET: usize = GicrRdRegister::CTLR.0 as usize;
const GICR_IIDR_OFFSET: usize = GicrRdRegister::IIDR.0 as usize;
const GICR_TYPER_OFFSET: usize = GicrRdRegister::TYPER.0 as usize;
const GICR_WAKER_OFFSET: usize = GicrRdRegister::WAKER.0 as usize;
const GICR_PIDR2_OFFSET: usize = GicrRdRegister::PIDR2.0 as usize;

const GICR_IPRIORITYR_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::IPRIORITYR0.0 as usize;
const GICR_ISENABLER0_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::ISENABLER0.0 as usize;
const GICR_ICENABLER0_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::ICENABLER0.0 as usize;
const GICR_ISPENDR0_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::ISPENDR0.0 as usize;
const GICR_ICPENDR0_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::ICPENDR0.0 as usize;
const GICR_IGROUPR0_OFFSET: usize = GICR_FRAME_SIZE + GicrSgiRegister::IGROUPR0.0 as usize;

/// Disributor control
#[bitfield(u32)]
pub struct GicdCtrl {
    #[bits(1)]
    pub enable_grp0: u32,
    #[bits(1)]
    pub enable_grp1_ns: u32,
    #[bits(1)]
    pub enable_grp1_s: u32,
    #[bits(1)]
    pub _res0: u32,
    #[bits(1)]
    pub are_s: u32,
    #[bits(1)]
    pub are_ns: u32,
    #[bits(1)]
    pub disable_secure: u32,
    #[bits(1)]
    pub e1_nwf: u32,
    #[bits(23)]
    pub _res1: u32,
    #[bits(1)]
    pub reg_write_pending: u32,
}

impl DeviceRegisterSpec for GicdCtrl {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_CTLR_OFFSET;
}

/// Identification register
#[bitfield(u32)]
pub struct GicdIidr {
    #[bits(12)]
    pub implementer: u32,
    #[bits(4)]
    pub revision: u32,
    #[bits(4)]
    pub variant: u32,
    #[bits(4)]
    pub _res0: u32,
    #[bits(8)]
    pub product_id: u32,
}

/// Distributor information
#[bitfield(u32)]
pub struct GicdTyper {
    #[bits(5)]
    pub it_lines: u32,
    #[bits(3)]
    pub cpu_number: u32,
    #[bits(1)]
    pub espi: u32,
    #[bits(1)]
    pub nmi: u32,
    #[bits(1)]
    pub security_extn: u32,
    #[bits(5)]
    pub lpi_lines: u32,
    #[bits(1)]
    pub mbis: u32,
    #[bits(1)]
    pub lpis: u32,
    #[bits(1)]
    pub dvis: u32,
    #[bits(5)]
    pub id_bits: u32,
    #[bits(1)]
    pub a3v: u32,
    #[bits(1)]
    pub no1n: u32,
    #[bits(1)]
    pub rss: u32,
    #[bits(5)]
    pub espi_range: u32,
}

impl DeviceRegisterSpec for GicdTyper {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_TYPER_OFFSET;
}

#[bitfield(u32)]
/// Peripheral ID2 Register
pub struct GicdPidr2 {
    #[bits(4)]
    pub _impl_def0: u32,
    #[bits(4)]
    pub gic_version: u32,
    #[bits(24)]
    pub _impl_def1: u32,
}

impl DeviceRegisterSpec for GicdPidr2 {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_PIDR2_OFFSET;
}

/// Clear enabled interrupts
#[bitfield(u32)]
pub struct GicdIcenabler {
    pub icenable: u32,
}

impl DeviceRegisterSpec for GicdIcenabler {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_ICENABLER_OFFSET;
}

impl DeviceRegisterArraySpec for GicdIcenabler {
    const COUNT: usize = 32;
}

/// Clear pending interrupts
#[bitfield(u32)]
pub struct GicdIcpendr {
    pub icpend: u32,
}

impl DeviceRegisterSpec for GicdIcpendr {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_ICPENDR_OFFSET;
}

impl DeviceRegisterArraySpec for GicdIcpendr {
    const COUNT: usize = 32;
}

/// Interrupt Group Registers
#[bitfield(u32)]

pub struct GicdIgroupr {
    pub igroup: u32,
}

impl DeviceRegisterSpec for GicdIgroupr {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_IGROUPR_OFFSET;
}

impl DeviceRegisterArraySpec for GicdIgroupr {
    const COUNT: usize = 32;
}

/// Interrupt Group Modifier Registers
#[bitfield(u32)]
pub struct GicdIgrpmodr {
    pub igrpmod: u32,
}

impl DeviceRegisterSpec for GicdIgrpmodr {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_IGRPMODR_OFFSET;
}

impl DeviceRegisterArraySpec for GicdIgrpmodr {
    const COUNT: usize = 32;
}

/// Interrupt Routing Registers
#[bitfield(u32)]
pub struct GicdIrouter {
    pub iroute: u32,
}

impl DeviceRegisterSpec for GicdIrouter {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICD_IROUTER_OFFSET;
}

impl DeviceRegisterArraySpec for GicdIrouter {
    const COUNT: usize = 1984;
}

/// GICR control
#[bitfield(u32)]
pub struct GicrCtlr {
    #[bits(1)]
    pub enable_lpis: u32,
    #[bits(1)]
    pub ces: u32,
    #[bits(1)]
    pub ir: u32,
    #[bits(1)]
    pub reg_write_pending: u32,
    #[bits(20)]
    _res0: u32,
    #[bits(1)]
    pub dpg0: u32,
    #[bits(1)]
    pub dpg1ns: u32,
    #[bits(1)]
    pub dpg1s: u32,
    #[bits(4)]
    _res1: u32,
    #[bits(1)]
    pub upstream_write_pending: u32,
}

impl DeviceRegisterSpec for GicrCtlr {
    type Raw = u32;
    type Value = GicrCtlr;
    const OFFSET: usize = GICR_CTLR_OFFSET;
}

/// GICR Identification register
#[bitfield(u32)]
pub struct GicrIidr {
    #[bits(12)]
    pub implementer: u32,
    #[bits(4)]
    pub revision: u32,
    #[bits(4)]
    pub variant: u32,
    #[bits(4)]
    pub _res0: u32,
    #[bits(8)]
    pub product_id: u32,
}

impl DeviceRegisterSpec for GicrIidr {
    type Raw = u32;
    type Value = GicrIidr;
    const OFFSET: usize = GICR_IIDR_OFFSET;
}

/// GICR Type register
#[bitfield(u64)]
pub struct GicrTyper {
    #[bits(1)]
    pub plpis: u64,
    #[bits(1)]
    pub vlpis: u64,
    #[bits(1)]
    pub dirty: u64,
    #[bits(1)]
    pub direct_lpi: u64,
    #[bits(1)]
    pub last: u64,
    #[bits(1)]
    pub dpgs: u64,
    #[bits(1)]
    pub mpam: u64,
    #[bits(1)]
    pub rvpeid: u64,
    #[bits(16)]
    pub processor_number: u64,
    #[bits(2)]
    pub common_lpi_aff: u64,
    #[bits(1)]
    pub vsgi: u64,
    #[bits(5)]
    pub ppi_num: u64,
    #[bits(8)]
    pub aff0: u64,
    #[bits(8)]
    pub aff1: u64,
    #[bits(8)]
    pub aff2: u64,
    #[bits(8)]
    pub aff3: u64,
}

impl DeviceRegisterSpec for GicrTyper {
    type Raw = u64;
    type Value = GicrTyper;
    const OFFSET: usize = GICR_TYPER_OFFSET;
}

/// GICR Wake register
#[bitfield(u32)]
pub struct GicrWaker {
    #[bits(1)]
    pub _impl_def0: u32,
    #[bits(1)]
    pub processor_sleep: u32,
    #[bits(1)]
    pub children_asleep: u32,
    #[bits(28)]
    _res0: u32,
    #[bits(1)]
    pub _impl_def1: u32,
}

impl DeviceRegisterSpec for GicrWaker {
    type Raw = u32;
    type Value = GicrWaker;
    const OFFSET: usize = GICR_WAKER_OFFSET;
}

#[bitfield(u32)]
/// Peripheral ID2 Register
pub struct GicrPidr2 {
    #[bits(4)]
    pub _impl_def0: u32,
    #[bits(4)]
    pub gic_version: u32,
    #[bits(24)]
    pub _impl_def1: u32,
}

impl DeviceRegisterSpec for GicrPidr2 {
    type Raw = u32;
    type Value = Self;
    const OFFSET: usize = GICR_PIDR2_OFFSET;
}

#[bitfield(u32)]
pub struct GicrIpriorityr {
    p0: u8,
    p1: u8,
    p2: u8,
    p3: u8,
}

impl DeviceRegisterSpec for GicrIpriorityr {
    type Raw = u32;
    type Value = GicrIpriorityr;
    const OFFSET: usize = GICR_IPRIORITYR_OFFSET;
}

impl DeviceRegisterArraySpec for GicrIpriorityr {
    const COUNT: usize = 8;
}

#[bitfield(u32)]
pub struct GicrIsenabler {
    sgis: u16,
    ppis: u16,
}

impl DeviceRegisterSpec for GicrIsenabler {
    type Raw = u32;
    type Value = GicrIsenabler;
    const OFFSET: usize = GICR_ISENABLER0_OFFSET;
}

#[bitfield(u32)]
pub struct GicrIcenabler {
    sgis: u16,
    ppis: u16,
}

impl DeviceRegisterSpec for GicrIcenabler {
    type Raw = u32;
    type Value = GicrIcenabler;
    const OFFSET: usize = GICR_ICENABLER0_OFFSET;
}

#[bitfield(u32)]
pub struct GicrIspendr {
    sgis: u16,
    ppis: u16,
}

impl DeviceRegisterSpec for GicrIspendr {
    type Raw = u32;
    type Value = GicrIspendr;
    const OFFSET: usize = GICR_ISPENDR0_OFFSET;
}

#[bitfield(u32)]
pub struct GicrIcpendr {
    sgis: u16,
    ppis: u16,
}

impl DeviceRegisterSpec for GicrIcpendr {
    type Raw = u32;
    type Value = GicrIcpendr;
    const OFFSET: usize = GICR_ICPENDR0_OFFSET;
}

#[bitfield(u32)]
pub struct GicrIgroupr {
    sgis: u16,
    ppis: u16,
}

impl DeviceRegisterSpec for GicrIgroupr {
    type Raw = u32;
    type Value = GicrIgroupr;
    const OFFSET: usize = GICR_IGROUPR0_OFFSET;
}

/// GIC version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicVersion {
    /// Version 3: GICD and GICR with two frames per CPU.
    GicV3,
    /// Version 4: GICD and GICR with four frames per CPU.
    GicV4,
}

/// GIC v3 or v4
pub struct Gic {
    gicd_base: usize,
    gicr_base: usize,
    version: GicVersion,
    max_spi: usize,
    num_cpus: usize,
    redist_size: usize,
}

/// GIC
///
/// Initalization and configurations are described in "4. Configuring the GIC" of
/// [GICv3 and GICv4 Software Overview](https://developer.arm.com/documentation/dai0492/b/)
///
/// Might be better to refactor to use type states.
impl Gic {
    /// Initialize the GIC interface
    pub fn new(gicd_base: usize, gicr_base: usize, num_cpus: usize) -> Self {
        // Run some basic (in)sanity checks

        let gicd_pidr2 = DeviceRegister::<GicdPidr2>::new(gicd_base);
        let gicd_ver = gicd_pidr2.load().gic_version();
        assert!(
            gicd_ver == 3 || gicd_ver == 4,
            "Expected GIC v3 or GIC v4, got {gicd_ver}"
        );

        let (redist_size, version) = if gicd_ver == 3 {
            // Got LPI and the SGI+PPI frames
            (2 * GICR_FRAME_SIZE, GicVersion::GicV3)
        } else if gicd_ver == 4 {
            // The redistributor in GICv4 has two additional frames: VLPI and Reserved
            (4 * GICR_FRAME_SIZE, GicVersion::GicV4)
        } else {
            unreachable!();
        };

        for i in 0..num_cpus {
            let gicr_pidr2 = DeviceRegister::<GicrPidr2>::new(gicr_base + i * redist_size);
            let gicr_ver = gicr_pidr2.load().gic_version();
            assert!(
                gicr_ver == 3 || gicr_ver == 4,
                "Expected GIC v3 or GIC v4, got {gicr_ver}"
            );

            let gicr_typer = DeviceRegister::<GicrTyper>::new(gicr_base + i * redist_size);
            let vlpis = gicr_typer.load().vlpis();
            assert!(
                vlpis == 0 || (gicr_ver == 4 && gicd_ver == 4),
                "Expected VLPIs in GIC v4, CPU {i}"
            );
        }

        // Initialize the instance

        let gicd_typer = DeviceRegister::<GicdTyper>::new(gicd_base);
        let max_spi = (32 * gicd_typer.load().it_lines() + 1) as usize;

        Self {
            gicd_base,
            gicr_base,
            version,
            max_spi,
            num_cpus,
            redist_size,
        }
    }

    /// Initialize the distributor, route all SPIs to the BSP.
    pub fn init_gicd(&mut self) {
        let mut gicd_ctrl = DeviceRegister::<GicdCtrl>::new(self.gicd_base);

        // Reset
        gicd_ctrl.store(GicdCtrl::new().with_disable_secure(0));
        while gicd_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        // Mask and clear all SPIs
        let max_spi = self.max_spi;

        DeviceRegisterArray::<GicdIcenabler>::new(self.gicd_base)
            .fill(1..max_spi / 32, GicdIcenabler::from(!0));
        DeviceRegisterArray::<GicdIcpendr>::new(self.gicd_base)
            .fill(1..max_spi / 32, GicdIcpendr::from(!0));
        DeviceRegisterArray::<GicdIgroupr>::new(self.gicd_base)
            .fill(1..max_spi / 32, GicdIgroupr::from(!0));
        DeviceRegisterArray::<GicdIgrpmodr>::new(self.gicd_base)
            .fill(1..max_spi / 32, GicdIgrpmodr::from(!0));
        while gicd_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        gicd_ctrl.store(
            GicdCtrl::new()
                .with_enable_grp0(1)
                .with_enable_grp1_ns(1)
                .with_are_ns(1),
        );
        while gicd_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        unsafe { core::arch::asm!("isb sy", options(nostack)) };

        // CPU 0, affinity 0.0.0.0
        DeviceRegisterArray::<GicdIrouter>::new(self.gicd_base)
            .fill(32..max_spi, GicdIrouter::from(0));
        while gicd_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        unsafe { core::arch::asm!("isb sy", options(nostack)) };
    }

    /// Wake up the CPU and initialize its redistributor.
    pub fn wakeup_cpu_and_init_gicr(&mut self, cpu: usize) {
        let gicr_base = self.gicr_base + cpu * self.redist_size;

        // Wake up the CPU

        let mut waker = DeviceRegister::<GicrWaker>::new(gicr_base);
        waker.store(waker.load().with_processor_sleep(0));
        while waker.load().children_asleep() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        // Configure interrupts

        let mut ipriorityr = DeviceRegisterArray::<GicrIpriorityr>::new(gicr_base);

        // SGI priorities, implementation defined
        let sgi_prio = GicrIpriorityr::new()
            .with_p0(0x90)
            .with_p1(0x90)
            .with_p2(0x90)
            .with_p3(0x90);
        ipriorityr.fill(0..4, sgi_prio);

        // PPI priorities, implementation defined
        let ppi_prio = GicrIpriorityr::new()
            .with_p0(0xa0)
            .with_p1(0xa0)
            .with_p2(0xa0)
            .with_p3(0xa0);
        ipriorityr.fill(4..8, ppi_prio);

        // Disable forwarding all PPI and SGI to the CPU interface
        DeviceRegister::<GicrIcenabler>::new(gicr_base).store(GicrIcenabler::new());
        DeviceRegister::<GicrIsenabler>::new(gicr_base).store(GicrIsenabler::new());

        // Set SGI and PPI as non-secure group 1 (set `GICD_CTLR.DS = 0` in GICD).
        DeviceRegister::<GicrIgroupr>::new(gicr_base)
            .store(GicrIgroupr::new().with_ppis(0xffff).with_sgis(0xffff));

        let gicr_ctrl = DeviceRegister::<GicrCtlr>::new(gicr_base);
        while gicr_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }

        unsafe { core::arch::asm!("isb sy", options(nostack)) };
    }

    /// Enables a local (SGI or PPI interrupt).
    fn enable_interrupt(&mut self, irq_num: u64, enable: bool, cpu: usize) {
        let gicr_base = self.gicr_base + cpu * self.redist_size;

        let mut icenable = DeviceRegister::<GicrIcenabler>::new(gicr_base);
        let mut isenable = DeviceRegister::<GicrIsenabler>::new(gicr_base);
        let mask = 1 << irq_num;

        if enable {
            icenable.fetch_and(GicrIcenabler::from(!mask));
            isenable.fetch_or(GicrIsenabler::from(mask));
        } else {
            isenable.fetch_and(GicrIsenabler::from(!mask));
            icenable.fetch_or(GicrIcenabler::from(mask));
        }

        let gicr_ctrl = DeviceRegister::<GicrCtlr>::new(gicr_base);
        while gicr_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }
    }

    /// Pends a local (SGI or PPI interrupt).
    fn pend_interrupt(&mut self, irq_num: u64, pend: bool, cpu: usize) {
        let gicr_base = self.gicr_base + cpu * self.redist_size;

        let mut icpend = DeviceRegister::<GicrIcpendr>::new(gicr_base);
        let mut ispend = DeviceRegister::<GicrIspendr>::new(gicr_base);
        let mask = 1 << irq_num;

        if pend {
            icpend.fetch_and(GicrIcpendr::from(!mask));
            ispend.fetch_or(GicrIspendr::from(mask));
        } else {
            ispend.fetch_and(GicrIspendr::from(!mask));
            icpend.fetch_or(GicrIcpendr::from(mask));
        }

        let gicr_ctrl = DeviceRegister::<GicrCtlr>::new(gicr_base);
        while gicr_ctrl.load().reg_write_pending() != 0 {
            unsafe { core::arch::asm!("yield", options(nostack)) }
        }
    }

    /// Enables an SGI.
    #[must_use]
    pub fn enable_sgi(&mut self, irq_num: u64, enable: bool, cpu: usize) -> bool {
        if !(0..16).contains(&irq_num) {
            return false;
        }

        self.enable_interrupt(irq_num, enable, cpu);
        true
    }

    /// Enables a PPI.
    #[must_use]
    pub fn enable_ppi(&mut self, irq_num: u64, enable: bool, cpu: usize) -> bool {
        if !(16..32).contains(&irq_num) {
            return false;
        }

        self.enable_interrupt(irq_num, enable, cpu);
        true
    }

    /// Pends an SGI.
    #[must_use]
    pub fn pend_sgi(&mut self, irq_num: u64, pend: bool, cpu: usize) -> bool {
        if !(0..16).contains(&irq_num) {
            return false;
        }

        self.pend_interrupt(irq_num, pend, cpu);
        true
    }

    /// Pends a PPI.
    #[must_use]
    pub fn pend_ppi(&mut self, irq_num: u64, pend: bool, cpu: usize) -> bool {
        if !(16..32).contains(&irq_num) {
            return false;
        }

        self.pend_interrupt(irq_num, pend, cpu);
        true
    }

    /// Initialize the control interface to the CPU
    /// through the ICC_* system registers.
    ///
    /// TODO: not hardcode the mask and the target.
    pub fn init_icc(&mut self) {
        // TODO: definitions for the registers
        let enable = 1u64;
        let disable = 0u64;
        let mask: u64 = 0xffu64;

        // SAFETY: not accesiing the memory.
        unsafe {
            core::arch::asm!(
                // Enable access to the system regster interface
                "msr ICC_SRE_EL1, {enable}",
                // EOI will deactivate the interrupt so don't need
                // to flip the bits in GICR separately
                "msr ICC_CTLR_EL1, {disable}",
                // Interrupt priority filter mask. Only interrupts with a higher priority than the value in this
                // register are signaled
                "msr ICC_PMR_EL1, {mask}",
                // Enable group 1 (we're in the non-secure world)
                "msr ICC_IGRPEN1_EL1, {enable}",
                enable = in(reg) enable, disable = in(reg) disable, mask = in(reg) mask,
                options(nostack, nomem))
        };
    }

    #[must_use]
    pub fn generate_sgi(&self, int_id: u64) -> bool {
        if !(0..16).contains(&int_id) {
            return false;
        }

        // Send to all (IRM aka Interrupt Routing Mode set to 1).
        // TODO: define a struct and provide an ability to send to
        // an individual CPU/PE.
        let route_sgi = (1u64 << 40) | (int_id << 24);

        // SAFETY: not accesiing the memory.
        unsafe {
            core::arch::asm!(
                // Generates a software interrupt
                "msr ICC_SGI1R_EL1, {route_sgi}",
                route_sgi = in(reg) route_sgi,
                options(nostack, nomem))
        };

        true
    }

    /// Get the GIC version.
    pub fn version(&self) -> GicVersion {
        self.version
    }

    /// Get the maximum SPI line.
    pub fn max_spi_id(&self) -> usize {
        self.max_spi
    }

    /// Number of CPUs
    pub fn num_cpus(&self) -> usize {
        self.num_cpus
    }
}
