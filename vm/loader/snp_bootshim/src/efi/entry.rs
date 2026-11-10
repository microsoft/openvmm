// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest-only part of the UEFI environment: the statics that hold the
//! tables and state, the `extern "efiapi"` boot services, and the call into
//! the EFI stub.
//!
//! Each service checks and dereferences the pointers that the stub passes,
//! as the UEFI calling convention requires, and calls the `services` and
//! `tables` modules for everything else.

// The guest build allows unsafe code for the whole crate. Host tests build
// this module too, so that Miri can check its pointer handling.
#![cfg_attr(
    not(minimal_rt),
    expect(unsafe_code, reason = "the boot services follow the UEFI ABI")
)]

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
use core::cell::RefCell;
use core::cell::RefMut;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::with_exposed_provenance;
use core::ptr::with_exposed_provenance_mut;
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

/// A wrapper that makes a value `Sync`, so that it can be a `static`.
///
/// This is a copy of `SingleThreaded` in `openhcl_boot`. It is not shared
/// through `minimal_rt`, because a public safe type cannot promise that its
/// users have only one thread. Each copy is private to a program that has
/// only one thread.
struct SingleThreaded<T>(T);

// SAFETY: The shim and the EFI stub run on one processor with interrupts
// disabled, so no two threads use the value. The stub reads the tables only
// through raw pointers, between service calls. Host tests serialize their
// access with a lock.
unsafe impl<T> Sync for SingleThreaded<T> {}

impl<T> Deref for SingleThreaded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The tables start zeroed. `Efi` is `FromZeros`, so that is a valid value.
static EFI: SingleThreaded<RefCell<MaybeUninit<Efi>>> =
    SingleThreaded(RefCell::new(MaybeUninit::zeroed()));
static STATE: SingleThreaded<RefCell<Option<State>>> = SingleThreaded(RefCell::new(None));

/// Borrows a static. A nested borrow stops the guest instead of panicking.
fn borrow<T>(cell: &RefCell<T>) -> RefMut<'_, T> {
    cell.try_borrow_mut()
        .unwrap_or_else(|_| terminate(Failure::EfiReentry))
}

/// Returns the address of the tables, for the stub to use.
fn efi_address() -> u64 {
    EFI.as_ptr().expose_provenance() as u64
}

fn with_efi<R>(f: impl FnOnce(&mut Efi) -> R) -> R {
    let mut efi = borrow(&EFI);
    // SAFETY: `EFI` starts zeroed, and `Efi` derives `FromZeros`.
    f(unsafe { efi.assume_init_mut() })
}

/// Runs `f` with the boot-services state, which `enter_linux` sets.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    match &mut *borrow(&STATE) {
        Some(state) => f(state),
        None => terminate(Failure::EfiStub),
    }
}

/// Each handle is the address of the protocol that it carries, so a handle is
/// unique and needs no handle table.
fn handles() -> Handles {
    Handles {
        image: field_address!(efi_address(), loaded_image),
        initrd: field_address!(efi_address(), initrd_path),
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

/// Used for services that have nothing to do in the shim, such as `Stall`,
/// `SetWatchdogTimer`, and console output, which goes nowhere because the shim
/// cannot do port I/O.
extern "efiapi" fn success() -> usize {
    code(EfiStatus::SUCCESS)
}

/// `LocateHandle`. The shim has no handle database, so no handle matches a
/// search, and the stub treats the searched-for device as absent.
extern "efiapi" fn not_found() -> usize {
    code(EfiStatus::NOT_FOUND)
}

/// The shim has no events or timers, so the task priority level never
/// changes. Return the level that an application runs at.
extern "efiapi" fn raise_tpl(_new_tpl: usize) -> usize {
    TPL_APPLICATION
}

extern "efiapi" fn restore_tpl(_old_tpl: usize) {}

/// `AllocatePages`. The allocator accepts unaccepted pages on demand, so the
/// stub can allocate past the pre-accepted boot pool.
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

/// `FreePages`. Freed pages stay accepted, and become conventional memory.
extern "efiapi" fn free_pages(memory: u64, pages: usize) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    match with_state(|state| state.map.free(memory, pages as u64)) {
        Ok(()) => code(EfiStatus::SUCCESS),
        Err(error) => error_status(error),
    }
}

/// `GetMemoryMap`. The stub first calls it with a small or null buffer to
/// learn the size, so `descriptor_size` and `map_size` are written before the
/// buffer check.
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

/// `AllocatePool`. Each allocation takes whole pages, with a header in front
/// of the buffer, so that `FreePool` can find the page count. The stub makes
/// only a few small pool allocations, so the wasted space does not matter.
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
                let header = with_exposed_provenance_mut::<[u64; 2]>(address as usize);
                header.write(services::pool_header(pages));
                buffer.write(header.byte_add(POOL_HEADER_SIZE as usize).cast());
            }
            code(EfiStatus::SUCCESS)
        }
        Err(error) => error_status(error),
    }
}

/// `FreePool`. Checks the header magic, so a pointer that `AllocatePool` did
/// not return cannot free pages.
extern "efiapi" fn free_pool(buffer: *mut c_void) -> usize {
    if exited() {
        return code(EfiStatus::UNSUPPORTED);
    }
    let Some(address) = services::pool_header_address(buffer as u64) else {
        return code(EfiStatus::INVALID_PARAMETER);
    };
    let header = with_exposed_provenance_mut::<[u64; 2]>(address as usize);
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

/// `HandleProtocol`. Only the image handle (`LOADED_IMAGE`) and the initrd
/// handle (`LoadFile2` and its device path) have protocols.
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
    let base = efi_address();
    let found = match services::find_interface(&handles(), has_initrd, handle as u64, protocol) {
        Some(Interface::LoadedImage) => field_address!(base, loaded_image),
        Some(Interface::LoadFile2) => field_address!(base, load_file2),
        Some(Interface::InitrdDevicePath) => field_address!(base, initrd_path),
        None => return code(EfiStatus::UNSUPPORTED),
    };
    // SAFETY: The caller passes a valid pointer.
    unsafe { interface.write(with_exposed_provenance_mut(found as usize)) };
    code(EfiStatus::SUCCESS)
}

/// `LocateDevicePath`. The stub calls this with
/// `LINUX_EFI_INITRD_MEDIA_GUID`'s vendor media path to find the handle that
/// provides its initrd. That is the only path that the shim knows. On success,
/// `device_path` moves past the matched node, as the spec requires.
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

/// `LoadFile2.LoadFile` on the initrd handle.
///
/// This is how the Linux EFI stub loads its initrd: the initrd handle stands
/// for one fixed file, so `this` and `file_path` (the rest of the device path,
/// which is only an end node) carry no information. GRUB and systemd-boot
/// implement it the same way. The stub calls it once without a buffer to learn
/// the size, then again to copy the initrd. `boot_policy` is for boot-option
/// loading, which `LoadFile2` does not support.
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
                core::ptr::copy_nonoverlapping(
                    with_exposed_provenance(source as usize),
                    buffer,
                    size,
                );
                buffer_size.write(size);
            }
            code(EfiStatus::SUCCESS)
        }
    }
}

/// `InstallConfigurationTable`. The stub installs tables such as the
/// unaccepted-memory bitmap here. This does not change the memory-map key,
/// because the stub calls it between `GetMemoryMap` and `ExitBootServices`.
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

/// `Exit`. The stub calls this only when it fails, and there is nothing to
/// return to, so the guest stops.
extern "efiapi" fn exit(
    _image_handle: usize,
    _exit_status: usize,
    _exit_data_size: usize,
    _exit_data: *mut u16,
) -> usize {
    terminate(Failure::EfiExit)
}

/// `ExitBootServices`. Fails if the memory map changed since the caller read
/// it. After this, the allocation services fail, but the tables stay valid.
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

/// `LocateProtocol`. The shim installs no global protocols, so every optional
/// feature that the stub looks for (for example RNG, TCG, or GOP) is absent.
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

/// `LocateHandleBuffer`. As for `LocateHandle`, no handle matches. The stub
/// uses this to look for optional devices, such as a GOP framebuffer.
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

/// `ConIn.ReadKeyStroke`. There is no keyboard, so no key is ever ready.
extern "efiapi" fn read_key_stroke(_this: *mut c_void, _key: *mut c_void) -> usize {
    code(EfiStatus::NOT_READY)
}

/// Returns the addresses of the service functions. Every boot service that the
/// shim does not implement returns `EFI_UNSUPPORTED`.
fn handlers() -> Handlers {
    let address = |f: *const ()| f.expose_provenance() as u64;
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

/// Sets the boot-services state and builds the tables.
fn install(map: MemoryMap, boot: &Boot) {
    *borrow(&STATE) = Some(State::new(map, boot.initrd, boot.initrd_size));
    let base = efi_address();
    with_efi(|efi| efi.build(base, boot, &handlers()));
}

/// Builds the UEFI environment and calls the Linux EFI stub. Does not
/// return.
#[cfg(minimal_rt)]
pub fn enter_linux(map: MemoryMap, boot: &Boot) -> ! {
    install(map, boot);
    let image_handle = handles().image;
    let system_table = field_address!(efi_address(), system_table);
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

/// These tests call the boot services as the EFI stub does, through raw
/// pointers into a host buffer that stands in for guest RAM. Run them under
/// Miri to check the services' unsafe code:
///
/// ```bash
/// cargo +nightly miri test -p snp_bootshim efi::entry
/// ```
#[cfg(test)]
mod tests {
    use super::*;
    use memory_range::MemoryRange;
    use parking_lot::Mutex;
    use parking_lot::MutexGuard;
    use test_with_tracing::test;
    use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
    use uefi_specs::uefi::boot::EfiDevicePathProtocol;
    use uefi_specs::uefi::boot::LOADED_IMAGE_PROTOCOL_GUID;
    use uefi_specs::uefi::boot::boot_service_index;

    const PAGES: usize = 32;
    const INITRD: &[u8] = b"initrd contents";

    #[repr(C, align(4096))]
    #[derive(Clone)]
    struct Page([u8; EFI_PAGE_SIZE as usize]);

    /// Guest RAM for one test. The services use the global tables and state,
    /// so tests run one at a time.
    struct Guest {
        /// A raw allocation, like guest RAM: the services reach it only
        /// through addresses, so no reference may own it.
        ram: *mut [Page],
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for Guest {
        fn drop(&mut self) {
            // SAFETY: `ram` came from `Box::into_raw`, and nothing uses it
            // after the test.
            drop(unsafe { Box::from_raw(self.ram) });
        }
    }

    impl Guest {
        /// Installs the services over `PAGES` pages of host memory: the first
        /// page holds the initrd, the next 15 are free, and the rest are
        /// unaccepted.
        fn new() -> Self {
            static LOCK: Mutex<()> = Mutex::new(());
            let lock = LOCK.lock();

            let mut ram = vec![Page([0; EFI_PAGE_SIZE as usize]); PAGES].into_boxed_slice();
            ram[0].0[..INITRD.len()].copy_from_slice(INITRD);
            let ram = Box::into_raw(ram);
            let base = ram.cast::<Page>().expose_provenance() as u64;
            let page = |index: u64| base + index * EFI_PAGE_SIZE;
            let mut map = MemoryMap::default();
            for (range, ty) in [
                (page(0)..page(1), EfiMemoryType::EFI_LOADER_DATA),
                (page(1)..page(16), EfiMemoryType::EFI_CONVENTIONAL_MEMORY),
                (
                    page(16)..page(PAGES as u64),
                    EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
                ),
            ] {
                map.set(MemoryRange::new(range), ty).unwrap();
            }
            install(
                map,
                &Boot {
                    kernel: MemoryRange::new(page(0)..page(1)),
                    kernel_entry: page(0),
                    initrd: Some(MemoryRange::new(page(0)..page(1))),
                    initrd_size: INITRD.len() as u64,
                    cmdline_gpa: 0,
                    cmdline_size: 0,
                    rsdp_gpa: 0x1000,
                    smbios_gpa: 0x2000,
                    cc_blob_gpa: 0x3000,
                    stack_top: 0,
                },
            );
            Self { ram, _lock: lock }
        }

        fn page(&self, index: usize) -> u64 {
            self.ram.cast::<Page>().addr() as u64 + (index * size_of::<Page>()) as u64
        }

        fn contains(&self, address: u64, len: usize) -> bool {
            address >= self.page(0) && address + len as u64 <= self.page(PAGES)
        }
    }

    fn status(status: EfiStatus) -> usize {
        code(status)
    }

    /// Returns the memory map, as the stub reads it: first the size, then the
    /// descriptors.
    fn memory_map() -> (Vec<EfiMemoryDescriptor>, usize) {
        let mut size = 0;
        let mut key = 0;
        let mut descriptor_size = 0;
        let mut version = 0;
        assert_eq!(
            get_memory_map(
                &mut size,
                core::ptr::null_mut(),
                &mut key,
                &mut descriptor_size,
                &mut version,
            ),
            status(EfiStatus::BUFFER_TOO_SMALL)
        );
        assert_eq!(descriptor_size, DESCRIPTOR_SIZE);
        assert_eq!(version, EFI_MEMORY_DESCRIPTOR_VERSION);
        let mut buffer = vec![0u8; size + DESCRIPTOR_SIZE];
        size = buffer.len();
        assert_eq!(
            get_memory_map(
                &mut size,
                buffer.as_mut_ptr(),
                &mut key,
                &mut descriptor_size,
                &mut version,
            ),
            status(EfiStatus::SUCCESS)
        );
        let descriptors = buffer[..size]
            .chunks_exact(DESCRIPTOR_SIZE)
            // SAFETY: Each chunk is one descriptor that `get_memory_map` wrote.
            .map(|chunk| unsafe {
                chunk
                    .as_ptr()
                    .cast::<EfiMemoryDescriptor>()
                    .read_unaligned()
            })
            .collect();
        (descriptors, key)
    }

    #[test]
    fn allocates_and_frees_pages() {
        let guest = Guest::new();
        // `AllocateAnyPages` must not read the uninitialized address.
        let mut address = MaybeUninit::<u64>::uninit();
        assert_eq!(
            allocate_pages(
                0,
                EfiMemoryType::EFI_LOADER_DATA.0,
                20,
                address.as_mut_ptr()
            ),
            status(EfiStatus::SUCCESS)
        );
        // SAFETY: `allocate_pages` succeeded, so it wrote the address.
        let address = unsafe { address.assume_init() };
        assert!(guest.contains(address, 20 * EFI_PAGE_SIZE as usize));
        assert_eq!(free_pages(address, 20), status(EfiStatus::SUCCESS));
        assert_eq!(free_pages(address, 20), status(EfiStatus::NOT_FOUND));

        let mut address = 0;
        assert_eq!(
            allocate_pages(3, EfiMemoryType::EFI_LOADER_DATA.0, 1, &mut address),
            status(EfiStatus::INVALID_PARAMETER)
        );
        assert_eq!(
            allocate_pages(0, 0, 1, core::ptr::null_mut()),
            status(EfiStatus::INVALID_PARAMETER)
        );
    }

    #[test]
    fn allocates_and_frees_pool() {
        let guest = Guest::new();
        let mut buffer = core::ptr::null_mut();
        assert_eq!(
            allocate_pool(EfiMemoryType::EFI_LOADER_DATA.0, 100, &mut buffer),
            status(EfiStatus::SUCCESS)
        );
        assert!(guest.contains(buffer.addr() as u64, 100));
        // SAFETY: The pool buffer has 100 writable bytes.
        unsafe { buffer.cast::<u8>().write_bytes(0xaa, 100) };
        assert_eq!(free_pool(buffer), status(EfiStatus::SUCCESS));
        // The header is cleared, so a second free fails.
        assert_eq!(free_pool(buffer), status(EfiStatus::INVALID_PARAMETER));

        // A page that `AllocatePool` did not return has no header.
        let not_pool = with_exposed_provenance_mut::<c_void>(guest.page(2) as usize + 16);
        assert_eq!(free_pool(not_pool), status(EfiStatus::INVALID_PARAMETER));
        assert_eq!(
            free_pool(core::ptr::null_mut()),
            status(EfiStatus::INVALID_PARAMETER)
        );
    }

    #[test]
    fn reads_the_memory_map_and_exits_boot_services() {
        let _guest = Guest::new();
        let (descriptors, key) = memory_map();
        assert_eq!(descriptors.len(), 3);
        assert_eq!(
            descriptors[2].typ,
            EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE
        );

        // A stale key fails.
        assert_eq!(
            exit_boot_services(0, key + 1),
            status(EfiStatus::INVALID_PARAMETER)
        );
        assert_eq!(exit_boot_services(0, key), status(EfiStatus::SUCCESS));
        // Allocation stops after ExitBootServices.
        let mut buffer = core::ptr::null_mut();
        assert_eq!(
            allocate_pool(EfiMemoryType::EFI_LOADER_DATA.0, 1, &mut buffer),
            status(EfiStatus::UNSUPPORTED)
        );
    }

    #[test]
    fn loads_the_initrd_as_the_stub_does() {
        let _guest = Guest::new();
        let path = with_efi(|efi| efi.initrd_path);
        let mut remaining = path.as_bytes().as_ptr();
        let mut device = 0;
        assert_eq!(
            locate_device_path(&LOAD_FILE2_PROTOCOL_GUID, &mut remaining, &mut device),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(device as u64, handles().initrd);
        // The rest of the path is the end node.
        // SAFETY: `remaining` points into `path`, at its end node.
        let end = unsafe { remaining.cast::<EfiDevicePathProtocol>().read_unaligned() };
        assert_eq!(end.device_type, uefi_specs::uefi::boot::EfiDeviceType::END);

        let mut load_file2 = core::ptr::null_mut();
        assert_eq!(
            handle_protocol(device, &LOAD_FILE2_PROTOCOL_GUID, &mut load_file2),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(
            load_file2.addr() as u64,
            field_address!(efi_address(), load_file2)
        );

        let mut size = 0;
        assert_eq!(
            load_file(
                load_file2,
                remaining.cast(),
                0,
                &mut size,
                core::ptr::null_mut()
            ),
            status(EfiStatus::BUFFER_TOO_SMALL)
        );
        assert_eq!(size, INITRD.len());
        let mut buffer = vec![0; size];
        assert_eq!(
            load_file(
                load_file2,
                remaining.cast(),
                0,
                &mut size,
                buffer.as_mut_ptr()
            ),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(buffer, INITRD);
    }

    #[test]
    fn rejects_other_device_paths_and_protocols() {
        let _guest = Guest::new();
        let mut path = with_efi(|efi| efi.initrd_path);
        path.vendor[VENDOR_NODE_SIZE - 1] ^= 1;
        let mut remaining = path.as_bytes().as_ptr();
        let mut device = 0;
        assert_eq!(
            locate_device_path(&LOAD_FILE2_PROTOCOL_GUID, &mut remaining, &mut device),
            status(EfiStatus::NOT_FOUND)
        );
        // A short first node is not read past its end.
        let end_only = [0x7f_u8, 0xff, 4, 0];
        let mut remaining = end_only.as_ptr();
        assert_eq!(
            locate_device_path(&LOAD_FILE2_PROTOCOL_GUID, &mut remaining, &mut device),
            status(EfiStatus::NOT_FOUND)
        );

        let mut interface = core::ptr::null_mut();
        assert_eq!(
            handle_protocol(
                handles().image as usize,
                &LOADED_IMAGE_PROTOCOL_GUID,
                &mut interface
            ),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(interface.addr() as u64, handles().image);
        assert_eq!(
            handle_protocol(
                handles().image as usize,
                &LOAD_FILE2_PROTOCOL_GUID,
                &mut interface
            ),
            status(EfiStatus::UNSUPPORTED)
        );
    }

    #[test]
    fn installs_configuration_tables_and_runs_the_helpers() {
        let _guest = Guest::new();
        assert_eq!(
            install_configuration_table(&ACPI_20_TABLE_GUID, core::ptr::null_mut()),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(with_efi(|efi| efi.system_table.number_of_table_entries), 3);

        let data = *b"123456789";
        let mut crc = 0;
        assert_eq!(
            calculate_crc32(data.as_ptr(), data.len(), &mut crc),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(crc, 0xcbf4_3926);

        let mut buffer = *b"abcdef";
        // CopyMem allows overlapping buffers.
        copy_mem(buffer.as_mut_ptr().wrapping_add(1), buffer.as_ptr(), 4);
        assert_eq!(&buffer, b"aabcdf");
        set_mem(buffer.as_mut_ptr(), 2, b'z');
        assert_eq!(&buffer, b"zzbcdf");

        let mut count = 0;
        assert_eq!(
            get_next_monotonic_count(&mut count),
            status(EfiStatus::SUCCESS)
        );
        assert_eq!(count, 1);
        let mut handles = 5;
        let mut list = core::ptr::dangling_mut();
        assert_eq!(
            locate_handle_buffer(
                0,
                core::ptr::null(),
                core::ptr::null_mut(),
                &mut handles,
                &mut list
            ),
            status(EfiStatus::NOT_FOUND)
        );
        assert_eq!((handles, list), (0, core::ptr::null_mut()));
    }

    #[test]
    fn calls_services_through_the_boot_services_table() {
        let guest = Guest::new();
        // The stub finds each service through the system table.
        let allocate = with_efi(|efi| {
            assert_eq!(
                efi.system_table.boot_services,
                field_address!(efi_address(), boot_services)
            );
            efi.boot_services.functions[boot_service_index::ALLOCATE_PAGES]
        });
        // SAFETY: The table holds the address of `allocate_pages`, whose type
        // this is.
        let allocate: extern "efiapi" fn(u32, u32, usize, *mut u64) -> usize =
            unsafe { core::mem::transmute(with_exposed_provenance::<()>(allocate as usize)) };
        let mut address = 0;
        assert_eq!(
            allocate(0, EfiMemoryType::EFI_LOADER_DATA.0, 1, &mut address),
            status(EfiStatus::SUCCESS)
        );
        assert!(guest.contains(address, EFI_PAGE_SIZE as usize));
    }

    #[test]
    fn allocates_from_unaccepted_memory() {
        let guest = Guest::new();
        // More than the 15 free pages, so the allocation spans unaccepted
        // memory.
        let mut address = 0;
        assert_eq!(
            allocate_pages(0, EfiMemoryType::EFI_LOADER_DATA.0, 20, &mut address),
            status(EfiStatus::SUCCESS)
        );
        assert!(guest.contains(address, 20 * EFI_PAGE_SIZE as usize));
        // SAFETY: The allocation is 20 pages of the test's RAM.
        unsafe {
            with_exposed_provenance_mut::<u8>(address as usize)
                .write_bytes(0x55, 20 * EFI_PAGE_SIZE as usize)
        };
    }

    #[test]
    #[should_panic(expected = "EfiReentry")]
    fn reentry_stops_the_guest() {
        let _guest = Guest::new();
        with_state(|_| with_state(|_| ()));
    }
}
