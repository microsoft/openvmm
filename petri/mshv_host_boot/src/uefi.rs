// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Firmware interfaces for the fixed mshv boot sequence.

// UNSAFETY: Installs a firmware protocol and passes owned load-option buffers to EFI images.
#![expect(unsafe_code)]

// xtask-fmt allow-target-arch sys-crate
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("mshv_host_boot supports only x86-64 and AArch64 UEFI");

use alloc::vec::Vec;
use core::ffi::c_void;
use core::fmt::Debug;
use mshv_host_boot::HYPERVISOR_CMDLINE_FILE;
use mshv_host_boot::KERNEL_CMDLINE_FILE;
use mshv_host_boot::MAX_CMDLINE_SIZE;
use uefi::CStr16;
use uefi::CString16;
use uefi::Handle;
use uefi::Identify;
use uefi::Status;
use uefi::boot;
use uefi::boot::LoadImageSource;
use uefi::cstr16;
use uefi::entry;
use uefi::fs::FileSystem;
use uefi::println;
use uefi::proto::BootPolicy;
use uefi::proto::device_path::DevicePath;
use uefi::proto::device_path::build::DevicePathBuilder;
use uefi::proto::device_path::build::media::FilePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::runtime;
use uefi::runtime::ResetType;
use uefi::runtime::VariableVendor;

// Shim uses the native C ABI, which differs from EFI's ABI on x64.
macro_rules! shim_lock {
    ($abi:literal) => {
        #[repr(C)]
        #[uefi::proto::unsafe_protocol("605dab50-e046-4300-abb6-3dd810dd8b23")]
        struct DevelopmentShimLock {
            verify: extern $abi fn(*const u8, u32) -> Status,
            hash: extern $abi fn(*const u8, u32, *mut c_void, *mut u8, *mut u8) -> Status,
            context: extern $abi fn(*const u8, u32, *mut c_void) -> Status,
        }

        extern $abi fn verify(_buffer: *const u8, _size: u32) -> Status {
            Status::SUCCESS
        }

        extern $abi fn hash(
            _buffer: *const u8,
            _size: u32,
            _context: *mut c_void,
            _sha256: *mut u8,
            _sha1: *mut u8,
        ) -> Status {
            println!("mshv host boot: SHIM_LOCK hashing is unsupported");
            Status::UNSUPPORTED
        }

        extern $abi fn context(_buffer: *const u8, _size: u32, _context: *mut c_void) -> Status {
            println!("mshv host boot: SHIM_LOCK image context is unsupported");
            Status::UNSUPPORTED
        }
    };
}

// xtask-fmt allow-target-arch sys-crate
#[cfg(target_arch = "x86_64")]
shim_lock!("sysv64");
// xtask-fmt allow-target-arch sys-crate
#[cfg(target_arch = "aarch64")]
shim_lock!("C");

static SHIM_LOCK: DevelopmentShimLock = DevelopmentShimLock {
    verify,
    hash,
    context,
};

fn report(operation: &str, error: impl Debug) -> Status {
    println!("mshv host boot: {operation}: {error:?}");
    Status::LOAD_ERROR
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("mshv host boot panicked: {info}");
    runtime::reset(ResetType::SHUTDOWN, Status::ABORTED, None)
}

#[entry]
fn main() -> Status {
    let status = match run() {
        Ok(()) => {
            println!("mshv host boot: Linux returned unexpectedly");
            Status::ABORTED
        }
        Err(status) => status,
    };
    println!("mshv host boot failed: {status}");
    // Never return with installed callbacks pointing into an unloaded image.
    runtime::reset(ResetType::SHUTDOWN, status, None)
}

fn run() -> Result<(), Status> {
    uefi::helpers::init().map_err(|e| report("initialize UEFI helpers", e))?;
    let mut secure_boot = [0u8];
    let (secure_boot, _) = runtime::get_variable(
        cstr16!("SecureBoot"),
        &VariableVendor::GLOBAL_VARIABLE,
        &mut secure_boot,
    )
    .map_err(|e| report("read SecureBoot", e))?;
    if secure_boot != [0] {
        println!("mshv host boot: Secure Boot must be disabled");
        return Err(Status::SECURITY_VIOLATION);
    }
    println!("mshv host boot: development boot, signature verification disabled");

    let image = boot::image_handle();
    let device = boot::open_protocol_exclusive::<LoadedImage>(image)
        .map_err(|e| report("open host boot image", e))?
        .device()
        .ok_or_else(|| report("find boot device", Status::NOT_FOUND))?;
    let mut fs = FileSystem::new(
        boot::get_image_file_system(image).map_err(|e| report("open EFI filesystem", e))?,
    );
    let hv_options = read_options(&mut fs, HYPERVISOR_CMDLINE_FILE)?;
    let linux_options = read_options(&mut fs, KERNEL_CMDLINE_FILE)?;
    drop(fs);

    // SAFETY: The GUID matches this protocol's layout. Its static storage and
    // callbacks remain loaded until Linux takes over or the machine shuts down.
    unsafe {
        boot::install_protocol_interface(
            None,
            &DevelopmentShimLock::GUID,
            (&raw const SHIM_LOCK).cast(),
        )
    }
    .map_err(|e| report("install development SHIM_LOCK", e))?;

    println!("mshv host boot: initializing HvLoader");
    let hvloader = load_image(device, cstr16!("\\HvLoader.efi"), &hv_options)?;
    boot::start_image(hvloader).map_err(|e| report("start HvLoader", e))?;

    println!("mshv host boot: starting Linux");
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "x86_64")]
    let kernel = cstr16!("\\bzImage");
    // xtask-fmt allow-target-arch sys-crate
    #[cfg(target_arch = "aarch64")]
    let kernel = cstr16!("\\Image");
    let linux = load_image(device, kernel, &linux_options)?;
    boot::start_image(linux).map_err(|e| report("start Linux EFI stub", e))
}

fn read_options(fs: &mut FileSystem, path: &str) -> Result<CString16, Status> {
    let path = CString16::try_from(path).map_err(|e| report("encode config path", e))?;
    let info = fs
        .metadata(&*path)
        .map_err(|e| report("read command-line metadata", e))?;
    if info.file_size() > MAX_CMDLINE_SIZE as u64 {
        return Err(report("read command line", "file exceeds 16 KiB"));
    }
    let text = fs
        .read_to_string(&*path)
        .map_err(|e| report("read command-line file", e))?;
    let text = mshv_host_boot::command_line(&text).map_err(|e| report("parse command line", e))?;
    println!("mshv host boot: {path}: {text}");
    CString16::try_from(text).map_err(|e| report("encode command line", e))
}

fn load_image(device: Handle, path: &CStr16, options: &CString16) -> Result<Handle, Status> {
    let device_path = boot::open_protocol_exclusive::<DevicePath>(device)
        .map_err(|e| report("open boot device path", e))?;
    let mut buffer = Vec::new();
    let mut builder = DevicePathBuilder::with_vec(&mut buffer);
    for node in device_path.node_iter() {
        builder = builder
            .push(&node)
            .map_err(|e| report("copy boot device path", e))?;
    }
    let image_path = builder
        .push(&FilePath { path_name: path })
        .and_then(DevicePathBuilder::finalize)
        .map_err(|e| report("build image device path", e))?;
    let image = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path: image_path,
            boot_policy: BootPolicy::ExactMatch,
        },
    )
    .map_err(|e| report("load EFI image", e))?;
    {
        let mut loaded = boot::open_protocol_exclusive::<LoadedImage>(image)
            .map_err(|e| report("open loaded image", e))?;
        // SAFETY: Both option strings are owned by run() and remain allocated
        // through HvLoader's return and the Linux EFI stub's execution.
        unsafe {
            loaded.set_load_options(options.as_ptr().cast(), options.num_bytes() as u32);
        }
    }
    Ok(image)
}
