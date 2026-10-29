// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The minimal UEFI environment in which the shim runs the Linux EFI stub.
//!
//! The shim provides the boot services that the x86 EFI stub uses: page and
//! pool allocation, the memory map, `LOADED_IMAGE` for the command line,
//! `LoadFile2` on the `LINUX_EFI_INITRD_MEDIA_GUID` device path for the
//! initrd, configuration tables, and `ExitBootServices`. Every protocol
//! lookup that the shim does not implement returns `EFI_NOT_FOUND`, and the
//! stub treats that as an absent feature.
//!
//! The configuration tables describe ACPI, SMBIOS, the SEV-SNP CC blob, and
//! (through the RT properties table) no runtime services. The runtime-services
//! table exists, but every service reports `EFI_UNSUPPORTED`. When
//! `SetVirtualAddressMap` fails, Linux turns off EFI runtime services. This
//! matters: some Linux code checks only that flag, and not the RT properties
//! table, before it calls a runtime service.
//!
//! The stub decompresses the kernel, loads the initrd, builds the E820 map
//! from the memory map, sets up lazy acceptance for unaccepted memory, calls
//! `ExitBootServices`, and enters the kernel. It does not return on success.

use crate::memory_map::Error;
use crate::memory_map::MemoryMap;
use acpi::minimal::Ram;
use loader_defs::linux::SnpBootShimPlatformParams;
use loader_defs::linux::SnpBootShimRange;
use memory_range::MemoryRange;
use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
use uefi_specs::uefi::boot::EfiMemoryType;

/// The shim accepts the omitted RAM below this address before it calls the
/// stub, as firmware does. Linux expects low memory to be usable early.
const LOW_MEMORY_END: u64 = 1 << 20;

/// The regions of the measured image.
pub(crate) struct ImageLayout<'a> {
    /// The host RAM.
    pub ram: &'a [Ram],
    /// The end of the RAM that the image measures.
    pub measured_ram_end: u64,
    /// The RAM that the image does not import.
    pub omitted: &'a [SnpBootShimRange],
    pub platform: &'a SnpBootShimPlatformParams,
    /// The shim's own image, including its BSS.
    pub shim: MemoryRange,
}

fn region(gpa: u64, size: u64) -> MemoryRange {
    MemoryRange::new(gpa..gpa + size.next_multiple_of(EFI_PAGE_SIZE))
}

fn omitted_range(range: &SnpBootShimRange) -> MemoryRange {
    MemoryRange::new(
        range.start_gpn * EFI_PAGE_SIZE..(range.start_gpn + range.page_count) * EFI_PAGE_SIZE,
    )
}

/// Builds the memory map before the shim accepts anything.
///
/// Imported memory is boot-services data unless it has a lasting role:
///
/// * The shim is runtime-services code. It holds the EFI tables, the
///   runtime services, and the exception handlers that run until Linux loads
///   its own IDT.
/// * The kernel image is loader code.
/// * SMBIOS is runtime-services data, which Linux maps encrypted.
/// * The ACPI tables and the RSDP are ACPI reclaim memory.
/// * The SNP secrets, CPUID, and CC-blob pages are reserved.
///
/// Omitted RAM and host RAM above the measured RAM are unaccepted.
pub(crate) fn build_memory_map(layout: &ImageLayout<'_>) -> Result<MemoryMap, Error> {
    let measured = MemoryRange::new(0..layout.measured_ram_end);
    if !covers(layout.ram, measured) {
        return Err(Error::InvalidParameter);
    }

    let mut map = MemoryMap::default();
    let above = MemoryRange::new(layout.measured_ram_end..MemoryRange::MAX_ADDRESS);
    for ram in layout.ram {
        map.set(
            ram.range.intersection(&measured),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        )?;
        map.set(
            ram.range.intersection(&above),
            EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
        )?;
    }
    let p = layout.platform;
    for (range, ty) in [
        (layout.shim, EfiMemoryType::EFI_RUNTIME_SERVICES_CODE),
        (
            region(p.kernel_gpa, p.kernel_size),
            EfiMemoryType::EFI_LOADER_CODE,
        ),
        (
            region(p.smbios_gpa, p.smbios_size),
            EfiMemoryType::EFI_RUNTIME_SERVICES_DATA,
        ),
        (
            region(p.acpi_output_gpa, p.acpi_output_size),
            EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        ),
        (
            region(p.rsdp_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        ),
        (
            region(p.secrets_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
        (
            region(p.cpuid_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
        (
            region(p.cc_blob_gpa, EFI_PAGE_SIZE),
            EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
        ),
    ] {
        map.set(range, ty)?;
    }
    // Apply these last. A typed region can contain omitted pages, such as a
    // hole between the shim's ELF segments, and those pages stay unaccepted.
    for range in layout.omitted {
        map.set(
            omitted_range(range),
            EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
        )?;
    }
    Ok(map)
}

/// Returns `true` if `ram` covers every page of `range`.
fn covers(ram: &[Ram], range: MemoryRange) -> bool {
    let mut cursor = range.start();
    while cursor < range.end() {
        let Some(next) = ram
            .iter()
            .find(|ram| ram.range.contains_addr(cursor))
            .map(|ram| ram.range.end())
        else {
            return false;
        };
        cursor = next;
    }
    true
}

/// Returns the memory that the shim accepts before it calls the stub, and the
/// type of each part.
///
/// This is the ACPI heap, the EFI stack, the boot pool for the decompressed
/// kernel, and the omitted RAM below 1 MiB. All other omitted memory stays
/// unaccepted until the stub or Linux needs it.
pub(crate) fn initial_acceptance<'a>(
    layout: &'a ImageLayout<'a>,
) -> impl Iterator<Item = (MemoryRange, EfiMemoryType)> + 'a {
    let p = layout.platform;
    let low = MemoryRange::new(0..LOW_MEMORY_END);
    [
        (
            region(p.heap_gpa, p.heap_size),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        ),
        (
            region(p.stack_gpa, p.stack_size),
            EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        ),
        (
            region(p.boot_pool_gpa, p.boot_pool_size),
            EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
        ),
    ]
    .into_iter()
    .chain(layout.omitted.iter().filter_map(move |range| {
        let range = omitted_range(range).intersection(&low);
        (!range.is_empty()).then_some((range, EfiMemoryType::EFI_CONVENTIONAL_MEMORY))
    }))
}

/// Computes the CRC-32 that UEFI uses for table headers.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32_parts(&[data])
}

/// Computes the CRC-32 of the concatenation of `parts`.
pub(crate) fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(minimal_rt)]
pub(crate) use guest::Boot;
#[cfg(minimal_rt)]
pub(crate) use guest::enter_linux;

#[cfg(minimal_rt)]
mod guest {
    use super::crc32;
    use super::crc32_parts;
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
    use core::mem::offset_of;
    use guid::Guid;
    use memory_range::MemoryRange;
    use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
    use uefi_specs::uefi::boot::DEVICE_PATH_PROTOCOL_GUID;
    use uefi_specs::uefi::boot::EFI_2_70_SYSTEM_TABLE_REVISION;
    use uefi_specs::uefi::boot::EFI_BOOT_SERVICES_SIGNATURE;
    use uefi_specs::uefi::boot::EFI_CC_BLOB_GUID;
    use uefi_specs::uefi::boot::EFI_LOADED_IMAGE_PROTOCOL_REVISION;
    use uefi_specs::uefi::boot::EFI_MEMORY_DESCRIPTOR_VERSION;
    use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
    use uefi_specs::uefi::boot::EFI_RT_PROPERTIES_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_RUNTIME_SERVICES_SIGNATURE;
    use uefi_specs::uefi::boot::EFI_SYSTEM_TABLE_SIGNATURE;
    use uefi_specs::uefi::boot::EfiDevicePathProtocol;
    use uefi_specs::uefi::boot::EfiDeviceType;
    use uefi_specs::uefi::boot::EfiEndDeviceSubType;
    use uefi_specs::uefi::boot::EfiMediaDeviceSubType;
    use uefi_specs::uefi::boot::EfiMemoryDescriptor;
    use uefi_specs::uefi::boot::EfiMemoryType;
    use uefi_specs::uefi::boot::EfiRtPropertiesTable;
    use uefi_specs::uefi::boot::EfiSystemTable;
    use uefi_specs::uefi::boot::LINUX_EFI_INITRD_MEDIA_GUID;
    use uefi_specs::uefi::boot::LOAD_FILE2_PROTOCOL_GUID;
    use uefi_specs::uefi::boot::LOADED_IMAGE_PROTOCOL_GUID;
    use uefi_specs::uefi::boot::SMBIOS3_TABLE_GUID;
    use uefi_specs::uefi::boot::TPL_APPLICATION;
    use uefi_specs::uefi::common::EfiStatus;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;

    /// The number of functions in EFI_BOOT_SERVICES (UEFI spec 4.4).
    const BOOT_SERVICE_COUNT: usize = 44;
    /// The number of functions in EFI_RUNTIME_SERVICES (UEFI spec 4.5).
    const RUNTIME_SERVICE_COUNT: usize = 14;
    const MAX_CONFIGURATION_TABLES: usize = 32;
    const DESCRIPTOR_SIZE: usize = size_of::<EfiMemoryDescriptor>();
    const POOL_HEADER_SIZE: u64 = 16;
    const POOL_MAGIC: u64 = u64::from_le_bytes(*b"SHIMPOOL");
    const VENDOR_NODE_SIZE: usize = size_of::<EfiDevicePathProtocol>() + size_of::<Guid>();
    const END_NODE_SIZE: usize = size_of::<EfiDevicePathProtocol>();

    /// Indexes into the boot-services function table (UEFI spec 4.4).
    mod bs {
        pub const RAISE_TPL: usize = 0;
        pub const RESTORE_TPL: usize = 1;
        pub const ALLOCATE_PAGES: usize = 2;
        pub const FREE_PAGES: usize = 3;
        pub const GET_MEMORY_MAP: usize = 4;
        pub const ALLOCATE_POOL: usize = 5;
        pub const FREE_POOL: usize = 6;
        pub const HANDLE_PROTOCOL: usize = 16;
        pub const LOCATE_HANDLE: usize = 19;
        pub const LOCATE_DEVICE_PATH: usize = 20;
        pub const INSTALL_CONFIGURATION_TABLE: usize = 21;
        pub const EXIT: usize = 24;
        pub const EXIT_BOOT_SERVICES: usize = 26;
        pub const GET_NEXT_MONOTONIC_COUNT: usize = 27;
        pub const STALL: usize = 28;
        pub const SET_WATCHDOG_TIMER: usize = 29;
        pub const LOCATE_HANDLE_BUFFER: usize = 36;
        pub const LOCATE_PROTOCOL: usize = 37;
        pub const CALCULATE_CRC32: usize = 40;
        pub const COPY_MEM: usize = 41;
        pub const SET_MEM: usize = 42;
    }

    #[repr(C)]
    #[derive(IntoBytes, Immutable)]
    struct TableHeader {
        signature: u64,
        revision: u32,
        header_size: u32,
        crc32: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct ServiceTable<const N: usize> {
        header: TableHeader,
        functions: [usize; N],
    }

    #[repr(C)]
    struct ConfigurationTable {
        guid: Guid,
        table: u64,
    }

    #[repr(C)]
    struct LoadedImage {
        revision: u32,
        parent_handle: u64,
        system_table: u64,
        device_handle: u64,
        file_path: u64,
        reserved: u64,
        load_options_size: u32,
        load_options: u64,
        image_base: u64,
        image_size: u64,
        image_code_type: u32,
        image_data_type: u32,
        unload: usize,
    }

    #[repr(C)]
    struct TextOutput {
        reset: usize,
        output_string: usize,
        test_string: usize,
        query_mode: usize,
        set_mode: usize,
        set_attribute: usize,
        clear_screen: usize,
        set_cursor_position: usize,
        enable_cursor: usize,
        mode: u64,
    }

    #[repr(C)]
    struct TextInput {
        reset: usize,
        read_key_stroke: usize,
        wait_for_key: u64,
    }

    /// A vendor media node with `LINUX_EFI_INITRD_MEDIA_GUID`, then an end
    /// node.
    #[repr(C)]
    struct InitrdDevicePath {
        vendor: [u8; VENDOR_NODE_SIZE],
        end: [u8; END_NODE_SIZE],
    }

    /// Every table and protocol that the shim hands to the EFI stub. The
    /// shim's image is runtime-services memory, so these stay valid after
    /// ExitBootServices.
    #[repr(C)]
    struct Efi {
        system_table: EfiSystemTable,
        boot_services: ServiceTable<BOOT_SERVICE_COUNT>,
        runtime_services: ServiceTable<RUNTIME_SERVICE_COUNT>,
        configuration_tables: [ConfigurationTable; MAX_CONFIGURATION_TABLES],
        loaded_image: LoadedImage,
        load_file2: usize,
        initrd_path: InitrdDevicePath,
        con_out: TextOutput,
        con_out_mode: [i32; 6],
        con_in: TextInput,
        firmware_vendor: [u16; 8],
        rt_properties: EfiRtPropertiesTable,
    }

    struct State {
        map: MemoryMap,
        initrd: Option<MemoryRange>,
        initrd_size: u64,
        exited: bool,
        monotonic_count: u64,
    }

    /// Data that only the boot processor uses.
    ///
    /// The shim and the EFI stub run on one processor with interrupts
    /// disabled, so there is no concurrent access. [`BootCell::with`] gives
    /// out one mutable reference at a time, and stops the guest if a borrow
    /// is reentered. The stub reads the tables only through raw pointers,
    /// between service calls.
    struct BootCell<T> {
        value: UnsafeCell<T>,
        borrowed: Cell<bool>,
    }

    // SAFETY: Only the boot processor uses the shim's statics, with
    // interrupts disabled.
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
            // SAFETY: The borrow flag makes this the only reference, and no
            // other processor or interrupt handler uses the value.
            let result = f(unsafe { &mut *self.value.get() });
            self.borrowed.set(false);
            result
        }
    }

    /// The tables start zeroed, which is a valid `Efi`: every field is an
    /// integer, an array of integers, or a GUID.
    static EFI: BootCell<MaybeUninit<Efi>> = BootCell::new(MaybeUninit::zeroed());
    static STATE: BootCell<Option<State>> = BootCell::new(None);

    fn with_efi<R>(f: impl FnOnce(&mut Efi) -> R) -> R {
        // SAFETY: `EFI` starts zeroed, and all zeros is a valid `Efi`.
        EFI.with(|efi| f(unsafe { efi.assume_init_mut() }))
    }

    /// Returns the address of a field of the EFI tables.
    macro_rules! efi_address {
        ($field:ident) => {
            EFI.address() + offset_of!(Efi, $field) as u64
        };
    }

    /// Runs `f` with the boot-services state, which `enter_linux` sets.
    fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
        STATE.with(|state| match state {
            Some(state) => f(state),
            None => terminate(Failure::EfiStub),
        })
    }

    fn code(status: EfiStatus) -> usize {
        status.0 as usize
    }

    fn error_status(error: Error) -> usize {
        match error {
            Error::OutOfResources => code(EfiStatus::OUT_OF_RESOURCES),
            Error::InvalidParameter => code(EfiStatus::INVALID_PARAMETER),
            Error::NotFound => code(EfiStatus::NOT_FOUND),
            Error::Accept => terminate(Failure::Accept),
        }
    }

    /// Returns `true` once boot services have ended.
    fn exited() -> bool {
        with_state(|state| state.exited)
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
        // SAFETY: The caller passes a valid pointer to an address.
        let address = unsafe { memory.read() };
        let allocate_type = match allocate_type {
            0 => AllocateType::Any,
            1 => AllocateType::Max(address),
            2 => AllocateType::Address(address),
            _ => return code(EfiStatus::INVALID_PARAMETER),
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
                // SAFETY: As above.
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

    extern "efiapi" fn allocate_pool(
        memory_type: u32,
        size: usize,
        buffer: *mut *mut c_void,
    ) -> usize {
        if exited() {
            return code(EfiStatus::UNSUPPORTED);
        }
        if buffer.is_null() {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        let Some(pages) = (size as u64)
            .checked_add(POOL_HEADER_SIZE)
            .map(|size| size.div_ceil(EFI_PAGE_SIZE))
        else {
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
                // SAFETY: The allocator handed out these accepted,
                // identity-mapped pages. The caller passes a valid pointer.
                unsafe {
                    let header = address as *mut u64;
                    header.write(pages);
                    header.add(1).write(POOL_MAGIC);
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
        let address = (buffer as u64).wrapping_sub(POOL_HEADER_SIZE);
        if buffer.is_null() || !address.is_multiple_of(EFI_PAGE_SIZE) {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        let header = address as *mut u64;
        // SAFETY: A pool buffer starts one header after its first page. The
        // magic check rejects other pointers before the map frees anything.
        let pages = unsafe {
            if header.add(1).read() != POOL_MAGIC {
                return code(EfiStatus::INVALID_PARAMETER);
            }
            header.read()
        };
        match with_state(|state| state.map.free(address, pages)) {
            Ok(()) => {
                // SAFETY: As above. The pages are still accepted and mapped.
                unsafe { header.add(1).write(0) };
                code(EfiStatus::SUCCESS)
            }
            Err(_) => code(EfiStatus::INVALID_PARAMETER),
        }
    }

    fn image_handle() -> usize {
        efi_address!(loaded_image) as usize
    }

    fn initrd_handle() -> usize {
        efi_address!(initrd_path) as usize
    }

    fn has_initrd() -> bool {
        with_state(|state| state.initrd.is_some())
    }

    extern "efiapi" fn handle_protocol(
        handle: usize,
        protocol: *const Guid,
        interface: *mut *mut c_void,
    ) -> usize {
        if protocol.is_null() || interface.is_null() {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        // SAFETY: The caller passes valid pointers.
        let protocol = unsafe { protocol.read_unaligned() };
        let found = if handle == image_handle() && protocol == LOADED_IMAGE_PROTOCOL_GUID {
            Some(efi_address!(loaded_image))
        } else if handle == initrd_handle() && has_initrd() {
            if protocol == LOAD_FILE2_PROTOCOL_GUID {
                Some(efi_address!(load_file2))
            } else if protocol == DEVICE_PATH_PROTOCOL_GUID {
                Some(efi_address!(initrd_path))
            } else {
                None
            }
        } else {
            None
        };
        match found {
            Some(found) => {
                // SAFETY: The caller passes a valid pointer.
                unsafe { interface.write(found as *mut c_void) };
                code(EfiStatus::SUCCESS)
            }
            None => code(EfiStatus::UNSUPPORTED),
        }
    }

    extern "efiapi" fn locate_device_path(
        protocol: *const Guid,
        device_path: *mut *const u8,
        device: *mut usize,
    ) -> usize {
        if protocol.is_null() || device_path.is_null() || device.is_null() {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        let expected = with_efi(|efi| efi.initrd_path.vendor);
        // SAFETY: The caller passes valid pointers, and a device path that
        // ends with an end node, so its first node has at least four bytes.
        unsafe {
            if protocol.read_unaligned() != LOAD_FILE2_PROTOCOL_GUID || !has_initrd() {
                return code(EfiStatus::NOT_FOUND);
            }
            let path = device_path.read();
            if path.is_null() {
                return code(EfiStatus::INVALID_PARAMETER);
            }
            let header = path.cast::<[u8; 4]>().read_unaligned();
            if header != expected[..4] {
                return code(EfiStatus::NOT_FOUND);
            }
            let node = path.cast::<[u8; VENDOR_NODE_SIZE]>().read_unaligned();
            if node != expected {
                return code(EfiStatus::NOT_FOUND);
            }
            device_path.write(path.add(VENDOR_NODE_SIZE));
            device.write(initrd_handle());
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
        let Some((initrd, size)) = with_state(|state| {
            state
                .initrd
                .map(|initrd| (initrd, state.initrd_size as usize))
        }) else {
            return code(EfiStatus::NOT_FOUND);
        };
        // SAFETY: The caller passes a buffer of `*buffer_size` bytes. The
        // initrd is measured, accepted memory.
        unsafe {
            if buffer.is_null() || buffer_size.read() < size {
                buffer_size.write(size);
                return code(EfiStatus::BUFFER_TOO_SMALL);
            }
            core::ptr::copy_nonoverlapping(initrd.start() as *const u8, buffer, size);
            buffer_size.write(size);
        }
        code(EfiStatus::SUCCESS)
    }

    /// Recomputes the CRC of the system table header.
    fn update_system_table_crc(table: &mut EfiSystemTable) {
        table.crc32 = 0;
        table.crc32 = crc32(table.as_bytes());
    }

    extern "efiapi" fn install_configuration_table(guid: *const Guid, table: *mut c_void) -> usize {
        if guid.is_null() {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        // SAFETY: The caller passes a valid GUID.
        let guid = unsafe { guid.read_unaligned() };
        with_efi(|efi| {
            let count = efi.system_table.number_of_table_entries as usize;
            let tables = &mut efi.configuration_tables;
            let index = tables[..count].iter().position(|entry| entry.guid == guid);
            match (index, table.is_null()) {
                (Some(index), false) => tables[index].table = table as u64,
                (Some(index), true) => {
                    tables[index..count].rotate_left(1);
                    efi.system_table.number_of_table_entries -= 1;
                }
                (None, false) => {
                    let Some(entry) = tables.get_mut(count) else {
                        return code(EfiStatus::OUT_OF_RESOURCES);
                    };
                    *entry = ConfigurationTable {
                        guid,
                        table: table as u64,
                    };
                    efi.system_table.number_of_table_entries += 1;
                }
                (None, true) => return code(EfiStatus::NOT_FOUND),
            }
            update_system_table_crc(&mut efi.system_table);
            code(EfiStatus::SUCCESS)
        })
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
        with_state(|state| {
            if state.exited {
                return code(EfiStatus::UNSUPPORTED);
            }
            if map_key as u64 != state.map.key() {
                return code(EfiStatus::INVALID_PARAMETER);
            }
            state.exited = true;
            code(EfiStatus::SUCCESS)
        })
    }

    extern "efiapi" fn get_next_monotonic_count(count: *mut u64) -> usize {
        if count.is_null() {
            return code(EfiStatus::INVALID_PARAMETER);
        }
        let next = with_state(|state| {
            state.monotonic_count += 1;
            state.monotonic_count
        });
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

    /// What the stub needs to boot Linux.
    pub struct Boot {
        pub kernel: MemoryRange,
        pub kernel_entry: u64,
        pub initrd: Option<MemoryRange>,
        pub initrd_size: u64,
        pub cmdline_gpa: u64,
        pub cmdline_size: u64,
        pub rsdp_gpa: u64,
        pub smbios_gpa: u64,
        pub cc_blob_gpa: u64,
        /// The top of the stack for the stub. It must be 16-byte aligned.
        pub stack_top: u64,
    }

    /// Builds the UEFI environment and calls the Linux EFI stub. Does not
    /// return.
    pub fn enter_linux(map: MemoryMap, boot: &Boot) -> ! {
        STATE.with(|state| {
            *state = Some(State {
                map,
                initrd: boot.initrd,
                initrd_size: boot.initrd_size,
                exited: false,
                monotonic_count: 0,
            })
        });
        with_efi(|efi| build_tables(efi, boot));

        let image_handle = image_handle() as u64;
        let system_table = efi_address!(system_table);
        // SAFETY: The kernel image is measured and accepted, its entry point
        // follows the UEFI image-entry ABI, and the stack is accepted memory
        // that nothing else uses. The previous RSP is saved on the new stack
        // and restored if the stub returns.
        unsafe {
            core::arch::asm!(
                "cld",
                "mov r11, rsp",
                "mov rsp, r9",
                "push r11",
                // Reserve the 32-byte shadow space and keep RSP 16-byte
                // aligned at the call.
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

    /// Fills in every table and protocol.
    fn build_tables(efi: &mut Efi, boot: &Boot) {
        for (index, c) in "OpenVMM\0".encode_utf16().enumerate() {
            efi.firmware_vendor[index] = c;
        }

        efi.con_out = TextOutput {
            reset: success as *const () as usize,
            output_string: success as *const () as usize,
            test_string: success as *const () as usize,
            query_mode: unsupported as *const () as usize,
            set_mode: unsupported as *const () as usize,
            set_attribute: success as *const () as usize,
            clear_screen: success as *const () as usize,
            set_cursor_position: unsupported as *const () as usize,
            enable_cursor: unsupported as *const () as usize,
            mode: efi_address!(con_out_mode),
        };
        efi.con_in = TextInput {
            reset: success as *const () as usize,
            read_key_stroke: read_key_stroke as *const () as usize,
            wait_for_key: 0,
        };

        let functions = &mut efi.boot_services.functions;
        functions.fill(unsupported as *const () as usize);
        functions[bs::RAISE_TPL] = raise_tpl as *const () as usize;
        functions[bs::RESTORE_TPL] = restore_tpl as *const () as usize;
        functions[bs::ALLOCATE_PAGES] = allocate_pages as *const () as usize;
        functions[bs::FREE_PAGES] = free_pages as *const () as usize;
        functions[bs::GET_MEMORY_MAP] = get_memory_map as *const () as usize;
        functions[bs::ALLOCATE_POOL] = allocate_pool as *const () as usize;
        functions[bs::FREE_POOL] = free_pool as *const () as usize;
        functions[bs::HANDLE_PROTOCOL] = handle_protocol as *const () as usize;
        functions[bs::LOCATE_HANDLE] = not_found as *const () as usize;
        functions[bs::LOCATE_DEVICE_PATH] = locate_device_path as *const () as usize;
        functions[bs::INSTALL_CONFIGURATION_TABLE] =
            install_configuration_table as *const () as usize;
        functions[bs::EXIT] = exit as *const () as usize;
        functions[bs::EXIT_BOOT_SERVICES] = exit_boot_services as *const () as usize;
        functions[bs::GET_NEXT_MONOTONIC_COUNT] = get_next_monotonic_count as *const () as usize;
        functions[bs::STALL] = success as *const () as usize;
        functions[bs::SET_WATCHDOG_TIMER] = success as *const () as usize;
        functions[bs::LOCATE_HANDLE_BUFFER] = locate_handle_buffer as *const () as usize;
        functions[bs::LOCATE_PROTOCOL] = locate_protocol as *const () as usize;
        functions[bs::CALCULATE_CRC32] = calculate_crc32 as *const () as usize;
        functions[bs::COPY_MEM] = copy_mem as *const () as usize;
        functions[bs::SET_MEM] = set_mem as *const () as usize;
        efi.boot_services.header = header(
            EFI_BOOT_SERVICES_SIGNATURE,
            size_of::<ServiceTable<BOOT_SERVICE_COUNT>>(),
        );
        efi.boot_services.header.crc32 = table_crc(&efi.boot_services);

        efi.runtime_services
            .functions
            .fill(unsupported as *const () as usize);
        efi.runtime_services.header = header(
            EFI_RUNTIME_SERVICES_SIGNATURE,
            size_of::<ServiceTable<RUNTIME_SERVICE_COUNT>>(),
        );
        efi.runtime_services.header.crc32 = table_crc(&efi.runtime_services);

        efi.rt_properties = EfiRtPropertiesTable::NONE_SUPPORTED;
        let mut count = 0;
        for (guid, table) in [
            (ACPI_20_TABLE_GUID, boot.rsdp_gpa),
            (SMBIOS3_TABLE_GUID, boot.smbios_gpa),
            (EFI_CC_BLOB_GUID, boot.cc_blob_gpa),
            (EFI_RT_PROPERTIES_TABLE_GUID, efi_address!(rt_properties)),
        ] {
            efi.configuration_tables[count] = ConfigurationTable { guid, table };
            count += 1;
        }

        let mut vendor = [0; VENDOR_NODE_SIZE];
        vendor[0] = EfiDeviceType::MEDIA.0;
        vendor[1] = EfiMediaDeviceSubType::VENDOR.0;
        vendor[2..4].copy_from_slice(&(VENDOR_NODE_SIZE as u16).to_le_bytes());
        vendor[4..].copy_from_slice(LINUX_EFI_INITRD_MEDIA_GUID.as_bytes());
        efi.initrd_path = InitrdDevicePath {
            vendor,
            end: [
                EfiDeviceType::END.0,
                EfiEndDeviceSubType::ENTIRE.0,
                END_NODE_SIZE as u8,
                0,
            ],
        };
        efi.load_file2 = load_file as *const () as usize;

        let con_out = efi_address!(con_out);
        let con_in = efi_address!(con_in);
        efi.loaded_image = LoadedImage {
            revision: EFI_LOADED_IMAGE_PROTOCOL_REVISION,
            parent_handle: 0,
            system_table: efi_address!(system_table),
            device_handle: 0,
            file_path: 0,
            reserved: 0,
            load_options_size: boot.cmdline_size as u32,
            load_options: boot.cmdline_gpa,
            image_base: boot.kernel.start(),
            image_size: boot.kernel.len(),
            image_code_type: EfiMemoryType::EFI_LOADER_CODE.0,
            image_data_type: EfiMemoryType::EFI_LOADER_DATA.0,
            unload: unsupported as *const () as usize,
        };

        efi.system_table = EfiSystemTable {
            signature: EFI_SYSTEM_TABLE_SIGNATURE,
            revision: EFI_2_70_SYSTEM_TABLE_REVISION,
            header_size: size_of::<EfiSystemTable>() as u32,
            firmware_vendor: efi_address!(firmware_vendor),
            firmware_revision: 1,
            console_in_handle: con_in,
            con_in,
            console_out_handle: con_out,
            con_out,
            standard_error_handle: con_out,
            std_err: con_out,
            runtime_services: efi_address!(runtime_services),
            boot_services: efi_address!(boot_services),
            number_of_table_entries: count as u64,
            configuration_table: efi_address!(configuration_tables),
            ..Default::default()
        };
        update_system_table_crc(&mut efi.system_table);
    }

    fn header(signature: u64, size: usize) -> TableHeader {
        TableHeader {
            signature,
            revision: EFI_2_70_SYSTEM_TABLE_REVISION,
            header_size: size as u32,
            crc32: 0,
            reserved: 0,
        }
    }

    fn table_crc<const N: usize>(table: &ServiceTable<N>) -> u32 {
        crc32_parts(&[table.header.as_bytes(), table.functions.as_bytes()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    const MB: u64 = 1 << 20;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32_parts(&[b"1234", b"", b"56789"]), 0xcbf4_3926);
    }

    fn test_platform() -> SnpBootShimPlatformParams {
        SnpBootShimPlatformParams {
            secrets_gpa: 0x10000,
            cpuid_gpa: 0x11000,
            cc_blob_gpa: 0x12000,
            smbios_gpa: 0x13000,
            smbios_size: 0x1000,
            rsdp_gpa: 0x14000,
            acpi_output_gpa: 0x15000,
            acpi_output_size: 0x10000,
            kernel_gpa: MB,
            kernel_size: 4 * MB,
            heap_gpa: 6 * MB,
            heap_size: MB,
            stack_gpa: 7 * MB,
            stack_size: 0x40000,
            boot_pool_gpa: 16 * MB,
            boot_pool_size: 16 * MB,
            ..FromZeros::new_zeroed()
        }
    }

    fn range(start: u64, end: u64) -> SnpBootShimRange {
        SnpBootShimRange {
            start_gpn: start / EFI_PAGE_SIZE,
            page_count: (end - start) / EFI_PAGE_SIZE,
        }
    }

    fn types(map: &MemoryMap) -> Vec<(u64, u64, EfiMemoryType)> {
        map.descriptors()
            .map(|d| {
                (
                    d.physical_start,
                    d.physical_start + d.number_of_pages * EFI_PAGE_SIZE,
                    d.typ,
                )
            })
            .collect()
    }

    #[test]
    fn builds_typed_map_and_accepts_work_areas() {
        let platform = test_platform();
        let ram = [
            Ram {
                range: MemoryRange::new(0..64 * MB),
                vnode: 0,
            },
            Ram {
                range: MemoryRange::new(4096 * MB..4160 * MB),
                vnode: 0,
            },
        ];
        // The second range is a hole between the shim's ELF segments.
        let omitted = [
            range(0, 0x10000),
            range(5 * MB + 0x1000, 5 * MB + 0x2000),
            range(6 * MB, 48 * MB),
        ];
        let layout = ImageLayout {
            ram: &ram,
            measured_ram_end: 48 * MB,
            omitted: &omitted,
            platform: &platform,
            shim: MemoryRange::new(5 * MB..6 * MB),
        };
        let mut map = build_memory_map(&layout).unwrap();
        let mut accepted = Vec::new();
        for (range, ty) in initial_acceptance(&layout) {
            map.accept(range, ty, &mut |range| {
                accepted.push(range);
                true
            })
            .unwrap();
        }

        use EfiMemoryType as T;
        assert_eq!(
            types(&map),
            [
                (0, 0x10000, T::EFI_CONVENTIONAL_MEMORY),
                (0x10000, 0x13000, T::EFI_RESERVED_MEMORY_TYPE),
                (0x13000, 0x14000, T::EFI_RUNTIME_SERVICES_DATA),
                (0x14000, 0x25000, T::EFI_ACPI_RECLAIM_MEMORY),
                (0x25000, MB, T::EFI_BOOT_SERVICES_DATA),
                (MB, 5 * MB, T::EFI_LOADER_CODE),
                (5 * MB, 5 * MB + 0x1000, T::EFI_RUNTIME_SERVICES_CODE),
                (
                    5 * MB + 0x1000,
                    5 * MB + 0x2000,
                    T::EFI_UNACCEPTED_MEMORY_TYPE
                ),
                (5 * MB + 0x2000, 6 * MB, T::EFI_RUNTIME_SERVICES_CODE),
                (6 * MB, 7 * MB + 0x40000, T::EFI_BOOT_SERVICES_DATA),
                (7 * MB + 0x40000, 16 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
                (16 * MB, 32 * MB, T::EFI_CONVENTIONAL_MEMORY),
                (32 * MB, 64 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
                (4096 * MB, 4160 * MB, T::EFI_UNACCEPTED_MEMORY_TYPE),
            ]
        );
        accepted.sort_by_key(|range| range.start());
        assert_eq!(
            accepted,
            [
                MemoryRange::new(0..0x10000),
                MemoryRange::new(6 * MB..7 * MB),
                MemoryRange::new(7 * MB..7 * MB + 0x40000),
                MemoryRange::new(16 * MB..32 * MB),
            ]
        );
    }

    #[test]
    fn rejects_host_ram_that_misses_measured_ram() {
        let platform = test_platform();
        let ram = [Ram {
            range: MemoryRange::new(MB..64 * MB),
            vnode: 0,
        }];
        let layout = ImageLayout {
            ram: &ram,
            measured_ram_end: 48 * MB,
            omitted: &[],
            platform: &platform,
            shim: MemoryRange::new(5 * MB..6 * MB),
        };
        assert_eq!(
            build_memory_map(&layout).err(),
            Some(Error::InvalidParameter)
        );
    }
}
