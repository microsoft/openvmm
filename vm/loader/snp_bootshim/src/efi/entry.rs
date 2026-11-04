// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest-only part of the UEFI environment: the statics that hold the
//! tables and state, the `extern "efiapi"` boot services, and the call into
//! the EFI stub.
//!
//! Each service checks and dereferences the pointers that the stub passes,
//! as the UEFI calling convention requires, and calls the `services` and
//! `tables` modules for everything else.

use super::services;
use super::services::Handles;
use super::services::Interface;
use super::services::LoadFile;
use super::services::POOL_HEADER_SIZE;
use super::services::State;
use super::tables::Boot;
use super::tables::Efi;
use super::tables::Handlers;
use super::tables::VENDOR_NODE_SIZE;
use super::tables::crc32;
use super::tables::field_address;
use crate::Failure;
use crate::accept;
use crate::memory_map::AllocateType;
use crate::memory_map::Error;
use crate::memory_map::MemoryMap;
use crate::terminate;
use core::cell::Cell;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use guid::Guid;
use uefi_specs::uefi::boot::EFI_BOOT_SERVICES_COUNT;
use uefi_specs::uefi::boot::EFI_MEMORY_DESCRIPTOR_VERSION;
use uefi_specs::uefi::boot::EfiMemoryDescriptor;
use uefi_specs::uefi::boot::EfiMemoryType;
use uefi_specs::uefi::boot::LOAD_FILE2_PROTOCOL_GUID;
use uefi_specs::uefi::boot::TPL_APPLICATION;
use uefi_specs::uefi::boot::boot_service_index as bs;
use uefi_specs::uefi::common::EfiStatus;
use zerocopy::IntoBytes;

const DESCRIPTOR_SIZE: usize = size_of::<EfiMemoryDescriptor>();

/// Data that only the boot processor uses.
///
/// The shim and the EFI stub run on one processor with interrupts disabled,
/// so there is no concurrent access. [`BootCell::with`] gives out one mutable
/// reference at a time, and stops the guest if a borrow is reentered. The stub
/// reads the tables only through raw pointers, between service calls.
struct BootCell<T> {
    value: UnsafeCell<T>,
    borrowed: Cell<bool>,
}

// SAFETY: Only the boot processor uses the shim's statics, with interrupts
// disabled.
unsafe impl<T> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            borrowed: Cell::new(false),
        }
    }

    /// Returns the address of the value, for the stub to use.
    fn address(&self) -> u64 {
        self.value.get() as u64
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        if self.borrowed.replace(true) {
            terminate(Failure::EfiReentry);
        }
        // SAFETY: The borrow flag makes this the only reference, and no other
        // processor or interrupt handler uses the value.
        let result = f(unsafe { &mut *self.value.get() });
        self.borrowed.set(false);
        result
    }
}

/// The tables start zeroed. `Efi` is `FromZeros`, so that is a valid value.
static EFI: BootCell<MaybeUninit<Efi>> = BootCell::new(MaybeUninit::zeroed());
static STATE: BootCell<Option<State>> = BootCell::new(None);

fn with_efi<R>(f: impl FnOnce(&mut Efi) -> R) -> R {
    // SAFETY: `EFI` starts zeroed, and `Efi` derives `FromZeros`.
    EFI.with(|efi| f(unsafe { efi.assume_init_mut() }))
}

/// Runs `f` with the boot-services state, which `enter_linux` sets.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| match state {
        Some(state) => f(state),
        None => terminate(Failure::EfiStub),
    })
}

fn handles() -> Handles {
    Handles {
        image: field_address!(EFI.address(), loaded_image),
        initrd: field_address!(EFI.address(), initrd_path),
    }
}

fn code(status: EfiStatus) -> usize {
    status.0 as usize
}

fn error_status(error: Error) -> usize {
    match services::status(error) {
        Some(status) => code(status),
        None => terminate(Failure::Accept),
    }
}

fn exited() -> bool {
    with_state(|state| state.exited())
}

extern "efiapi" fn unsupported() -> usize {
    code(EfiStatus::UNSUPPORTED)
}

extern "efiapi" fn success() -> usize {
    code(EfiStatus::SUCCESS)
}

extern "efiapi" fn not_found() -> usize {
    code(EfiStatus::NOT_FOUND)
}

extern "efiapi" fn raise_tpl(_new_tpl: usize) -> usize {
    TPL_APPLICATION
}

extern "efiapi" fn restore_tpl(_old_tpl: usize) {}

extern "efiapi" fn allocate_pages(
    allocate_type: u32,
    memory_type: u32,
    pages: usize,
    memory: *mut u64,
) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    if memory.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    let allocate_type = match services::allocate_type(allocate_type, || {
        // SAFETY: The caller passes a valid pointer. This runs only for the
        // types for which `*memory` is an input.
        unsafe { memory.read() }
    }) {
        Ok(allocate_type) => allocate_type,
        Err(error) => return error_status(error),
    };
    match with_state(|state| {
        state.map.allocate(
            allocate_type,
            EfiMemoryType(memory_type),
            pages as u64,
            &mut accept,
        )
    }) {
        Ok(address) => {
            // SAFETY: The caller passes a valid pointer.
            unsafe { memory.write(address) };
            code(EfiStatus::SUCCESS)
        }
        Err(error) => error_status(error),
    }
}

extern "efiapi" fn free_pages(memory: u64, pages: usize) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    match with_state(|state| state.map.free(memory, pages as u64)) {
        Ok(()) => code(EfiStatus::SUCCESS),
        Err(error) => error_status(error),
    }
}

extern "efiapi" fn get_memory_map(
    map_size: *mut usize,
    map: *mut u8,
    map_key: *mut usize,
    descriptor_size: *mut usize,
    descriptor_version: *mut u32,
) -> usize {
    if map_size.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    with_state(|state| {
        let memory_map = &state.map;
        let needed = memory_map.descriptors().len() * DESCRIPTOR_SIZE;
        // SAFETY: The caller passes valid pointers, and a buffer of
        // `*map_size` bytes at `map`.
        unsafe {
            if !descriptor_size.is_null() {
                descriptor_size.write(DESCRIPTOR_SIZE);
            }
            if !descriptor_version.is_null() {
                descriptor_version.write(EFI_MEMORY_DESCRIPTOR_VERSION);
            }
            if map.is_null() || map_size.read() < needed {
                map_size.write(needed);
                return code(EfiStatus::BUFFER_TOO_SMALL);
            }
            if map_key.is_null() {
                return code(EfiStatus::INVALID_PARAMETER);
            }
            for (index, descriptor) in memory_map.descriptors().enumerate() {
                core::ptr::copy_nonoverlapping(
                    descriptor.as_bytes().as_ptr(),
                    map.add(index * DESCRIPTOR_SIZE),
                    DESCRIPTOR_SIZE,
                );
            }
            map_size.write(needed);
            map_key.write(memory_map.key() as usize);
        }
        code(EfiStatus::SUCCESS)
    })
}

extern "efiapi" fn allocate_pool(memory_type: u32, size: usize, buffer: *mut *mut c_void) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    if buffer.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    let Some(pages) = services::pool_pages(size) else {
        return code(EfiStatus::OUT_OF_RESOURCES);
    };
    match with_state(|state| {
        state.map.allocate(
            AllocateType::Any,
            EfiMemoryType(memory_type),
            pages,
            &mut accept,
        )
    }) {
        Ok(address) => {
            // SAFETY: The allocator handed out these accepted, identity-mapped
            // pages. The caller passes a valid pointer.
            unsafe {
                (address as *mut [u64; 2]).write(services::pool_header(pages));
                buffer.write((address + POOL_HEADER_SIZE) as *mut c_void);
            }
            code(EfiStatus::SUCCESS)
        }
        Err(error) => error_status(error),
    }
}

extern "efiapi" fn free_pool(buffer: *mut c_void) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    let Some(address) = services::pool_header_address(buffer as u64) else {
        return code(EfiStatus::INVALID_PARAMETER);
    };
    let header = address as *mut [u64; 2];
    // SAFETY: A pool buffer starts one header after its first page, which is
    // mapped. The magic check rejects other pointers before the map frees
    // anything.
    let Some(pages) = services::pool_header_pages(unsafe { header.read() }) else {
        return code(EfiStatus::INVALID_PARAMETER);
    };
    match with_state(|state| state.map.free(address, pages)) {
        Ok(()) => {
            // SAFETY: As above. The pages are still accepted and mapped.
            unsafe { header.write([0; 2]) };
            code(EfiStatus::SUCCESS)
        }
        Err(_) => code(EfiStatus::INVALID_PARAMETER),
    }
}

extern "efiapi" fn handle_protocol(
    handle: usize,
    protocol: *const Guid,
    interface: *mut *mut c_void,
) -> usize {
    if protocol.is_null() || interface.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    // SAFETY: The caller passes a valid pointer.
    let protocol = unsafe { protocol.read_unaligned() };
    let has_initrd = with_state(|state| state.has_initrd());
    let base = EFI.address();
    let found = match services::find_interface(&handles(), has_initrd, handle as u64, protocol) {
        Some(Interface::LoadedImage) => field_address!(base, loaded_image),
        Some(Interface::LoadFile2) => field_address!(base, load_file2),
        Some(Interface::InitrdDevicePath) => field_address!(base, initrd_path),
        None => return code(EfiStatus::UNSUPPORTED),
    };
    // SAFETY: The caller passes a valid pointer.
    unsafe { interface.write(found as *mut c_void) };
    code(EfiStatus::SUCCESS)
}

extern "efiapi" fn locate_device_path(
    protocol: *const Guid,
    device_path: *mut *const u8,
    device: *mut usize,
) -> usize {
    if protocol.is_null() || device_path.is_null() || device.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    // SAFETY: The caller passes valid pointers.
    let (protocol, path) = unsafe { (protocol.read_unaligned(), device_path.read()) };
    if protocol != LOAD_FILE2_PROTOCOL_GUID || !with_state(|state| state.has_initrd()) {
        return code(EfiStatus::NOT_FOUND);
    }
    if path.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    // SAFETY: A device path ends with an end node, so its first node has at
    // least a four-byte header. Read the whole vendor node only if the header
    // says that the node is that long.
    let header = unsafe { path.cast::<[u8; 4]>().read_unaligned() };
    if u16::from_le_bytes([header[2], header[3]]) as usize != VENDOR_NODE_SIZE {
        return code(EfiStatus::NOT_FOUND);
    }
    // SAFETY: As above.
    let node = unsafe { path.cast::<[u8; VENDOR_NODE_SIZE]>().read_unaligned() };
    if !services::is_initrd_node(&node) {
        return code(EfiStatus::NOT_FOUND);
    }
    // SAFETY: The caller passes valid pointers. The next node follows.
    unsafe {
        device_path.write(path.add(VENDOR_NODE_SIZE));
        device.write(handles().initrd as usize);
    }
    code(EfiStatus::SUCCESS)
}

extern "efiapi" fn load_file(
    _this: *mut c_void,
    _file_path: *const c_void,
    boot_policy: u8,
    buffer_size: *mut usize,
    buffer: *mut u8,
) -> usize {
    if buffer_size.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    if boot_policy != 0 {
        return code(EfiStatus::UNSUPPORTED);
    }
    // SAFETY: The caller passes a valid pointer.
    let available = (!buffer.is_null()).then(|| unsafe { buffer_size.read() });
    match with_state(|state| state.load_file(available)) {
        LoadFile::NotFound => code(EfiStatus::NOT_FOUND),
        LoadFile::TooSmall { size } => {
            // SAFETY: The caller passes a valid pointer.
            unsafe { buffer_size.write(size) };
            code(EfiStatus::BUFFER_TOO_SMALL)
        }
        LoadFile::Copy { source, size } => {
            // SAFETY: The caller passes a buffer of at least `size` bytes. The
            // initrd is measured, accepted memory.
            unsafe {
                core::ptr::copy_nonoverlapping(source as *const u8, buffer, size);
                buffer_size.write(size);
            }
            code(EfiStatus::SUCCESS)
        }
    }
}

extern "efiapi" fn install_configuration_table(guid: *const Guid, table: *mut c_void) -> usize {
    if guid.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    // SAFETY: The caller passes a valid GUID.
    let guid = unsafe { guid.read_unaligned() };
    code(with_efi(|efi| {
        efi.install_configuration_table(guid, table as u64)
    }))
}

extern "efiapi" fn exit(
    _image_handle: usize,
    _exit_status: usize,
    _exit_data_size: usize,
    _exit_data: *mut u16,
) -> usize {
    terminate(Failure::EfiExit)
}

extern "efiapi" fn exit_boot_services(_image_handle: usize, map_key: usize) -> usize {
    code(with_state(|state| state.exit_boot_services(map_key as u64)))
}

extern "efiapi" fn get_next_monotonic_count(count: *mut u64) -> usize {
    if count.is_null() {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    let next = with_state(|state| state.next_monotonic_count());
    // SAFETY: The caller passes a valid pointer.
    unsafe { count.write(next) };
    code(EfiStatus::SUCCESS)
}

extern "efiapi" fn locate_protocol(
    _protocol: *const Guid,
    _registration: *mut c_void,
    interface: *mut *mut c_void,
) -> usize {
    if !interface.is_null() {
        // SAFETY: The caller passes a valid pointer.
        unsafe { interface.write(core::ptr::null_mut()) };
    }
    code(EfiStatus::NOT_FOUND)
}

extern "efiapi" fn locate_handle_buffer(
    _search_type: u32,
    _protocol: *const Guid,
    _search_key: *mut c_void,
    handle_count: *mut usize,
    buffer: *mut *mut usize,
) -> usize {
    // SAFETY: The caller passes valid pointers.
    unsafe {
        if !handle_count.is_null() {
            handle_count.write(0);
        }
        if !buffer.is_null() {
            buffer.write(core::ptr::null_mut());
        }
    }
    code(EfiStatus::NOT_FOUND)
}

extern "efiapi" fn calculate_crc32(data: *const u8, size: usize, crc: *mut u32) -> usize {
    if data.is_null() || crc.is_null() || size == 0 {
        return code(EfiStatus::INVALID_PARAMETER);
    }
    // SAFETY: The caller passes `size` readable bytes and a valid output.
    unsafe { crc.write(crc32(core::slice::from_raw_parts(data, size))) };
    code(EfiStatus::SUCCESS)
}

extern "efiapi" fn copy_mem(destination: *mut u8, source: *const u8, length: usize) {
    // SAFETY: The caller passes valid, possibly overlapping, buffers.
    unsafe { core::ptr::copy(source, destination, length) };
}

extern "efiapi" fn set_mem(buffer: *mut u8, size: usize, value: u8) {
    // SAFETY: The caller passes a valid buffer.
    unsafe { core::ptr::write_bytes(buffer, value, size) };
}

extern "efiapi" fn read_key_stroke(_this: *mut c_void, _key: *mut c_void) -> usize {
    code(EfiStatus::NOT_READY)
}

fn handlers() -> Handlers {
    let address = |f: *const ()| f as u64;
    let mut functions = [address(unsupported as *const ()); EFI_BOOT_SERVICES_COUNT];
    for (index, function) in [
        (bs::RAISE_TPL, raise_tpl as *const ()),
        (bs::RESTORE_TPL, restore_tpl as *const ()),
        (bs::ALLOCATE_PAGES, allocate_pages as *const ()),
        (bs::FREE_PAGES, free_pages as *const ()),
        (bs::GET_MEMORY_MAP, get_memory_map as *const ()),
        (bs::ALLOCATE_POOL, allocate_pool as *const ()),
        (bs::FREE_POOL, free_pool as *const ()),
        (bs::HANDLE_PROTOCOL, handle_protocol as *const ()),
        (bs::LOCATE_HANDLE, not_found as *const ()),
        (bs::LOCATE_DEVICE_PATH, locate_device_path as *const ()),
        (
            bs::INSTALL_CONFIGURATION_TABLE,
            install_configuration_table as *const (),
        ),
        (bs::EXIT, exit as *const ()),
        (bs::EXIT_BOOT_SERVICES, exit_boot_services as *const ()),
        (
            bs::GET_NEXT_MONOTONIC_COUNT,
            get_next_monotonic_count as *const (),
        ),
        (bs::STALL, success as *const ()),
        (bs::SET_WATCHDOG_TIMER, success as *const ()),
        (bs::LOCATE_HANDLE_BUFFER, locate_handle_buffer as *const ()),
        (bs::LOCATE_PROTOCOL, locate_protocol as *const ()),
        (bs::CALCULATE_CRC32, calculate_crc32 as *const ()),
        (bs::COPY_MEM, copy_mem as *const ()),
        (bs::SET_MEM, set_mem as *const ()),
    ] {
        functions[index] = address(function);
    }
    Handlers {
        boot_services: functions,
        unsupported: address(unsupported as *const ()),
        success: address(success as *const ()),
        read_key_stroke: address(read_key_stroke as *const ()),
        load_file: address(load_file as *const ()),
    }
}

/// Builds the UEFI environment and calls the Linux EFI stub. Does not
/// return.
pub fn enter_linux(map: MemoryMap, boot: &Boot) -> ! {
    STATE.with(|state| *state = Some(State::new(map, boot.initrd, boot.initrd_size)));
    let base = EFI.address();
    with_efi(|efi| efi.build(base, boot, &handlers()));

    let image_handle = handles().image;
    let system_table = field_address!(base, system_table);
    // SAFETY: The kernel image is measured and accepted, its entry point
    // follows the UEFI image-entry ABI, and the stack is accepted memory that
    // nothing else uses. The previous RSP is saved on the new stack and
    // restored if the stub returns.
    unsafe {
        core::arch::asm!(
            "cld",
            "mov r11, rsp",
            "mov rsp, r9",
            "push r11",
            // Reserve the 32-byte shadow space and keep RSP 16-byte aligned
            // at the call.
            "sub rsp, 40",
            "call r8",
            "add rsp, 40",
            "pop rsp",
            in("rcx") image_handle,
            in("rdx") system_table,
            in("r8") boot.kernel_entry,
            in("r9") boot.stack_top,
            clobber_abi("efiapi"),
        );
    }
    terminate(Failure::EfiStub)
}
