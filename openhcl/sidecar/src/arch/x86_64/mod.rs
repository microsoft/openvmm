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
use minimal_rt::arch::com3;
use minimal_rt::arch::msr::write_msr;
use uart_16550::Uart16550Tty;
use uart_16550::backend::PioBackend;
use x86defs::Exception;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

mod addr_space {
    use super::VpGlobals;
    use memory_range::MemoryRange;

    // These must match the bootstrap page tables in the global assembly below.
    const PTE_SELF: usize = 0;
    const PTE_HYPERCALL_INPUT: usize = 1;
    const PTE_HYPERCALL_OUTPUT: usize = 2;
    const PTE_COMMAND_PAGE: usize = 3;
    const PTE_GLOBALS: usize = 4;
    const PTE_ASSIST_PAGE: usize = 5;
    const PTE_CONTROL_PAGE: usize = 6;
    const PTE_REGISTER_PAGE: usize = 7;
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
        reg_page_pa: u64,
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
        pt[PTE_REGISTER_PAGE] = pte_data(reg_page_pa);
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

    pub fn register_page() -> *mut hvdef::HvX64RegisterPage {
        (per_vp(PTE_REGISTER_PAGE)) as *mut _
    }

    pub fn register_page_pa() -> u64 {
        // SAFETY: the register page PTE is not changing concurrently.
        unsafe { pte(PTE_REGISTER_PAGE).read() }.address()
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
    overlays_mapped: bool,
    register_page_mapped: bool,
}

const _: () = assert!(size_of::<VpGlobals>() <= 0x1000);

static mut VTL_RETURN_OFFSET: u16 = 0;
static mut VSM_CAPABILITIES: hvdef::HvRegisterVsmCapabilities =
    hvdef::HvRegisterVsmCapabilities::new();
static AFTER_INIT: AtomicBool = AtomicBool::new(false);
static ENABLE_LOG: AtomicBool = AtomicBool::new(false);
// The boot shim already initialized the device.
static SERIAL: spin::Mutex<Uart16550Tty<PioBackend>> =
    spin::Mutex::new(Uart16550Tty::from_inner(com3()));

macro_rules! log {
    () => {};
    ($($arg:tt)*) => {
        if $crate::arch::x86_64::ENABLE_LOG.load(core::sync::atomic::Ordering::Relaxed) {
            $crate::arch::x86_64::log_fmt(format_args!($($arg)*));
        }
    };
}
use core::mem::size_of;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::hypercall::HvInputVtl;
pub(crate) use log;

fn log_fmt(args: core::fmt::Arguments<'_>) {
    if ENABLE_LOG.load(Acquire) {
        let mut serial = SERIAL.lock();
        if AFTER_INIT.load(Acquire) {
            // SAFETY: `hv_vp_index` is not being concurrently modified.
            // TODO: improve how per-VP globals work.
            let vp_index = unsafe { &*addr_space::globals() }.hv_vp_index;
            let _ = writeln!(serial, "sidecar#{vp_index}: {}", args);
        } else {
            let _ = writeln!(serial, "sidecar: {}", args);
        }
    }
}

#[cfg_attr(minimal_rt, panic_handler)]
#[cfg_attr(not(minimal_rt), expect(dead_code))]
fn panic(panic: &core::panic::PanicInfo<'_>) -> ! {
    let stack_va_to_pa = |ptr| {
        addr_space::stack()
            .offset_of(ptr as u64)
            .map(|offset| addr_space::stack_base_pa() + offset as usize)
    };
    minimal_rt::enlightened_panic::report(*b"SIDECARK", panic, stack_va_to_pa);
    if !AFTER_INIT.load(Acquire) {
        if let Some(mut serial) = SERIAL.try_lock() {
            let _ = writeln!(serial, "{panic}");
        } else {
            // We may have panicked while holding the lock, so trying to acquire it may deadlock.
            // Best effort, we can't guarantee no tearing but print the message anyways.
            let _ = writeln!(Uart16550Tty::from_inner(com3()), "{panic}");
        }
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

#[cfg_attr(not(minimal_rt), expect(dead_code))]
extern "C" fn irq_handler() {
    eoi();
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
extern "C" fn exception_handler(exception: Exception, rsp: u64) -> ! {
    // SAFETY: reading cr2 has no safety requirements.
    let cr2 = unsafe {
        let cr2: u64;
        core::arch::asm!("mov {}, cr2", out(reg) cr2);
        cr2
    };
    panic!("unexpected exception {exception:?} cr2 = {cr2:#x} rsp = {rsp:#x}");
}

/// Relocates the sidecar, switches to its page tables and stack, and restores the caller on return.
///
/// # Safety
/// Must be entered with the sidecar parameter-page GPA in RDI, the output-page GPA in RSI,
/// identity mappings, and a valid caller stack. Returns the initialization result in AL.
#[cfg(minimal_rt)]
// SAFETY: This is the sole `_start` exported by the minimal_rt sidecar image.
#[unsafe(no_mangle)]
// SAFETY: Assembly implements the entire C ABI entry and return path, preserving
// the caller's nonvolatile registers, stack, and address space across initialization.
#[unsafe(naked)]
unsafe extern "C" fn _start() -> bool {
    core::arch::naked_asm!(
        "BASE = 0xffffff8000000000",
        "PML4_INDEX = 511",
        "STACK_TOP = 0x400000",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rax", // align the caller's stack
        "mov r14, rdi",
        "mov r15, rsi",
        "lea rdi, __bss_start[rip]",
        "lea rcx, _end[rip]",
        "sub rcx, rdi",
        "xor eax, eax",
        "cld",
        "rep stosb",
        "mov r12, cr3",
        "mov rbx, BASE",
        "lea rax, __ehdr_start[rip]",
        "sub rbx, rax",
        "lea rdx, _DYNAMIC[rip]",
        "mov rsi, BASE",
        "lea rdi, __ehdr_start[rip]",
        "call {relocate}",
        // Relocation produced VAs in the page tables; convert them back to PAs.
        "lea rdi, pt_start[rip]",
        "lea rcx, pt_end[rip]",
        "2:",
        "sub [rdi], rbx",
        "add rdi, 8",
        "cmp rdi, rcx",
        "jne 2b",
        // Splice the sidecar mapping into the caller's page tables.
        "mov r13, [r12 + PML4_INDEX * 8]",
        "lea rax, 3 + pdpt[rip]",
        "mov [r12 + PML4_INDEX * 8], rax",
        "mov cr3, r12",
        "lea rax, 2f[rip]",
        "add rax, rbx",
        "jmp rax",
        "2:",
        "mov rbp, rsp",
        "lea rsp, STACK_TOP + __ehdr_start[rip]",
        "lea rax, pml4[rip]",
        "sub rax, rbx",
        "mov cr3, rax",
        "mov rdi, r14",
        "mov rsi, r15",
        "call {start}",
        // Restore the caller's physical stack, address space, and registers.
        "mov cr3, r12",
        "mov rsp, rbp",
        "lea rcx, 3f[rip]",
        "sub rcx, rbx",
        "jmp rcx",
        "3:",
        "mov [r12 + PML4_INDEX * 8], r13",
        "mov cr3, r12",
        "out:",
        "pop r15",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        start = sym init::start,
        relocate = sym minimal_rt::reloc::relocate,
    );
}

#[cfg(minimal_rt)]
core::arch::global_asm!(
    r#"
    STACK_PAGES = 4

    PTE_PRESENT = 1
    PTE_RW = 2
    PTE_LARGE = 0x80
    PTE_NX = 0x8000000000000000
    PTE_TABLE = PTE_PRESENT | PTE_RW
    # The image uses a single RWX 2MB mapping.
    # FUTURE: split the sections to apply W/X permissions appropriately.
    PTE_IMAGE = PTE_PRESENT | PTE_LARGE | PTE_RW
    PTE_DATA = PTE_PRESENT | PTE_RW | PTE_NX

    .pushsection .data
    .align 4096
    .globl pt_start, pml4
    pt_start:
    pml4:
    .fill 511, 8, 0
    .quad pdpt + PTE_TABLE

    .align 4096
    .globl pdpt
    pdpt:
    .quad pd + PTE_TABLE

    .align 4096
    pd:
    .global IMAGE_PDE
    IMAGE_PDE:
    .quad __ehdr_start + PTE_IMAGE
    .quad pt + PTE_TABLE

    .align 4096
    # The data page table layout must match addr_space.
    pt:
    .quad pt + PTE_DATA
    .quad hypercall_input + PTE_DATA
    .quad hypercall_output + PTE_DATA
    .fill 509 - STACK_PAGES, 8, 0
    .set OFFSET, 0
    .rept STACK_PAGES
    .quad stack + OFFSET + PTE_DATA
    .set OFFSET, OFFSET + 0x1000
    .endr

    .align 4096
    .globl pt_end
    pt_end:

    .align 4096
    hypercall_input:
    .fill 4096, 1, 0
    hypercall_output:
    .fill 4096, 1, 0

    .popsection

    .pushsection .bss
    .align 4096
    stack:
    .fill STACK_PAGES * 0x1000, 1, 0
    .popsection
    "#,
);
