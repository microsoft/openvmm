// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific sidecar code.

#![cfg(target_arch = "x86_64")]
// UNSAFETY: Interacting with low level hardware and memory primitives.
#![expect(unsafe_code)]

mod init;
mod temporary_map;
mod vp;

use core::fmt::Write;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Acquire;
use hvdef::HvError;
use hvdef::HypercallCode;
use minimal_rt::arch::msr::write_msr;
use minimal_rt::arch::Serial;
use minimal_rt::isolation::IsolationType;
use x86defs::Exception;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

mod addr_space {
    use super::VpGlobals;
    use memory_range::MemoryRange;

    // These must match their use in entry.S.
    const PTE_SELF: usize = 0;
    const PTE_HYPERCALL_INPUT: usize = 1;
    const PTE_HYPERCALL_OUTPUT: usize = 2;
    const PTE_COMMAND_PAGE: usize = 3;
    const PTE_GLOBALS: usize = 4;
    const PTE_ASSIST_PAGE: usize = 5;
    const PTE_CONTROL_PAGE: usize = 6;
    const PTE_TEMPORARY_MAP: usize = 256;
    const PTE_STACK: usize = PTE_STACK_END - sidecar_defs::STACK_PAGES;
    const PTE_STACK_END: usize = 512;

    const PAGE_SIZE: u64 = 0x1000;

    unsafe extern "C" {
        static __ehdr_start: u8;
    }

    fn pte_data(addr: u64) -> x86defs::Pte {
        x86defs::Pte::new()
            .with_address(addr)
            .with_read_write(true)
            .with_present(true)
            .with_no_execute(true)
    }

    /// Returns the physical address of the globals.
    pub fn init_ap(
        pt: &mut [x86defs::Pte; 512],
        pt_pa: u64,
        control_page_pa: u64,
        command_page_pa: u64,
        memory: &mut impl Iterator<Item = u64>,
    ) -> u64 {
        pt.fill(x86defs::Pte::new());
        for i in 0..sidecar_defs::STACK_PAGES {
            pt[PTE_STACK + i] = pte_data(memory.next().unwrap());
        }
        pt[PTE_SELF] = pte_data(pt_pa);
        pt[PTE_COMMAND_PAGE] = pte_data(command_page_pa);
        let globals_pa = memory.next().unwrap();
        pt[PTE_GLOBALS] = pte_data(globals_pa);
        pt[PTE_ASSIST_PAGE] = pte_data(memory.next().unwrap());
        pt[PTE_HYPERCALL_INPUT] = pte_data(memory.next().unwrap());
        pt[PTE_HYPERCALL_OUTPUT] = pte_data(memory.next().unwrap());
        pt[PTE_CONTROL_PAGE] = pte_data(control_page_pa);
        globals_pa
    }

    fn base_address() -> usize {
        core::ptr::addr_of!(__ehdr_start) as usize
    }

    fn per_vp(page: usize) -> usize {
        base_address() + 0x200000 + page * PAGE_SIZE as usize
    }

    fn pte(page: usize) -> *mut x86defs::Pte {
        (per_vp(PTE_SELF) as *mut x86defs::Pte).wrapping_add(page)
    }

    pub fn temporary_map() -> usize {
        per_vp(PTE_TEMPORARY_MAP)
    }

    pub fn temp_ptes() -> *mut x86defs::Pte {
        pte(PTE_TEMPORARY_MAP)
    }

    pub fn stack() -> MemoryRange {
        MemoryRange::new(per_vp(PTE_STACK) as u64..per_vp(PTE_STACK_END) as u64)
    }

    pub fn stack_base_pa() -> usize {
        // SAFETY: the stack PTE is not changing concurrently.
        unsafe { pte(PTE_STACK).read() }.address() as usize
    }

    pub fn command_page() -> *mut sidecar_defs::CommandPage {
        per_vp(PTE_COMMAND_PAGE) as *mut _
    }

    pub fn globals() -> *mut VpGlobals {
        (per_vp(PTE_GLOBALS)) as *mut _
    }

    pub fn assist_page() -> *mut hvdef::HvVpAssistPage {
        (per_vp(PTE_ASSIST_PAGE)) as *mut _
    }

    pub fn assist_page_pa() -> u64 {
        // SAFETY: the assist page PTE is not changing concurrently.
        unsafe { pte(PTE_ASSIST_PAGE).read() }.address()
    }

    pub fn hypercall_input() -> *mut [u8; 4096] {
        (per_vp(PTE_HYPERCALL_INPUT)) as *mut [u8; 4096]
    }

    pub fn hypercall_input_pa() -> u64 {
        // SAFETY: the hypercall input PTE is not changing concurrently.
        unsafe { pte(PTE_HYPERCALL_INPUT).read() }.address()
    }

    pub fn hypercall_output() -> *mut [u8; 4096] {
        (per_vp(PTE_HYPERCALL_OUTPUT)) as *mut [u8; 4096]
    }

    pub fn hypercall_output_pa() -> u64 {
        // SAFETY: the hypercall output PTE is not changing concurrently.
        unsafe { pte(PTE_HYPERCALL_OUTPUT).read() }.address()
    }

    pub fn control_page() -> *const sidecar_defs::ControlPage {
        (per_vp(PTE_CONTROL_PAGE)) as *const _
    }
}

struct VpGlobals {
    hv_vp_index: u32,
    node_cpu_index: u32,
    reg_page_pa: u64,
    overlays_mapped: bool,
    register_page_mapped: bool,
}

const _: () = assert!(size_of::<VpGlobals>() <= 0x1000);

static mut VTL_RETURN_OFFSET: u16 = 0;
static mut VSM_CAPABILITIES: hvdef::HvRegisterVsmCapabilities =
    hvdef::HvRegisterVsmCapabilities::new();
static AFTER_INIT: AtomicBool = AtomicBool::new(false);
static ENABLE_LOG: AtomicBool = AtomicBool::new(false);

macro_rules! log {
    () => {};
    ($($arg:tt)*) => {
        if $crate::arch::x86_64::ENABLE_LOG.load(core::sync::atomic::Ordering::Relaxed) {
            $crate::arch::x86_64::log_fmt(format_args!($($arg)*));
        }
    };
}
use core::mem::size_of;
use hvdef::hypercall::HvInputVtl;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
pub(crate) use log;
use minimal_rt::arch::InstrIoAccess;

fn log_fmt(args: core::fmt::Arguments<'_>) {
    if ENABLE_LOG.load(Acquire) {
        if AFTER_INIT.load(Acquire) {
            // SAFETY: `hv_vp_index` is not being concurrently modified.
            // TODO: improve how per-VP globals work.
            let vp_index = unsafe { &*addr_space::globals() }.hv_vp_index;
            let _ = writeln!(Serial::new(InstrIoAccess), "sidecar#{vp_index}: {}", args);
        } else {
            let _ = writeln!(Serial::new(InstrIoAccess), "sidecar: {}", args);
        }
    }
}

#[cfg_attr(minimal_rt, panic_handler)]
#[cfg_attr(not(minimal_rt), allow(dead_code))]
fn panic(panic: &core::panic::PanicInfo<'_>) -> ! {
    let stack_va_to_pa = |ptr| {
        addr_space::stack()
            .offset_of(ptr as u64)
            .map(|offset| addr_space::stack_base_pa() + offset as usize)
    };
    minimal_rt::enlightened_panic::report(panic, stack_va_to_pa);
    if !AFTER_INIT.load(Acquire) {
        let _ = writeln!(Serial::new(InstrIoAccess), "{panic}");
    }
    minimal_rt::arch::fault();
}

struct CommandErrorWriter<'a>(&'a mut sidecar_defs::CommandError);

impl Write for CommandErrorWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let s = s.as_bytes();
        let buf = &mut self.0.buf[self.0.len as usize..];
        let n = buf.len().min(s.len());
        buf[..n].copy_from_slice(&s[..n]);
        self.0.len += n as u8;
        Ok(())
    }
}

fn hypercall(code: HypercallCode, rep_count: usize) -> Result<(), HvError> {
    let control = hvdef::hypercall::Control::new()
        .with_code(code.0)
        .with_rep_count(rep_count);

    // SAFETY: the caller guarantees the safety of the hypercall, including that
    // the input and output pages are not concurrently accessed.
    unsafe {
        minimal_rt::arch::hypercall::invoke_hypercall(
            IsolationType::None,
            control,
            addr_space::hypercall_input_pa(),
            addr_space::hypercall_output_pa(),
        )
        .result()
    }
}

fn get_hv_vp_register(
    target_vtl: HvInputVtl,
    name: HvRegisterName,
) -> Result<HvRegisterValue, HvError> {
    {
        // SAFETY: the input page is not concurrently accessed.
        let input = unsafe { &mut *addr_space::hypercall_input() };

        hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl,
            rsvd: [0; 3],
        }
        .write_to_prefix(input)
        .unwrap();

        name.write_to_prefix(&mut input[size_of::<hvdef::hypercall::GetSetVpRegisters>()..])
            .unwrap();
    }

    hypercall(HypercallCode::HvCallGetVpRegisters, 1)?;
    // SAFETY: the output is not concurrently accessed.
    let output = unsafe { &*addr_space::hypercall_output() };
    Ok(HvRegisterValue::read_from_prefix(output).unwrap().0) // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
}

fn set_hv_vp_register(
    target_vtl: HvInputVtl,
    name: HvRegisterName,
    value: HvRegisterValue,
) -> Result<(), HvError> {
    {
        // SAFETY: the input page is not concurrently accessed.
        let input = unsafe { &mut *addr_space::hypercall_input() };

        hvdef::hypercall::GetSetVpRegisters {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index: hvdef::HV_VP_INDEX_SELF,
            target_vtl,
            rsvd: [0; 3],
        }
        .write_to_prefix(input)
        .unwrap();

        hvdef::hypercall::HvRegisterAssoc {
            name,
            pad: [0; 3],
            value,
        }
        .write_to_prefix(&mut input[size_of::<hvdef::hypercall::GetSetVpRegisters>()..])
        .unwrap();
    }

    hypercall(HypercallCode::HvCallSetVpRegisters, 1)?;
    Ok(())
}

fn eoi() {
    // SAFETY: no safety requirements for EOI.
    unsafe {
        write_msr(x86defs::apic::ApicRegister::EOI.x2apic_msr(), 0);
    }
}

#[cfg_attr(not(minimal_rt), allow(dead_code))]
extern "C" fn irq_handler() {
    eoi();
    log!("irq");
}

#[cfg_attr(not(minimal_rt), allow(dead_code))]
extern "C" fn exception_handler(exception: Exception, rsp: u64) -> ! {
    // SAFETY: reading cr2 has no safety requirements.
    let cr2 = unsafe {
        let cr2: u64;
        core::arch::asm!("mov {}, cr2", out(reg) cr2);
        cr2
    };
    panic!("unexpected exception {exception:?} cr2 = {cr2:#x} rsp = {rsp:#x}");
}

#[cfg(minimal_rt)]
core::arch::global_asm! {
    include_str!("entry.S"),
    start = sym init::start,
    relocate = sym minimal_rt::reloc::relocate,
    irq_handler = sym irq_handler,
    exception_handler = sym exception_handler,
}
