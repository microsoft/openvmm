use core::alloc::{GlobalAlloc, Layout};

use uefi::{boot::{exit_boot_services, MemoryType}, guid, println, CStr16, Status};

use crate::infolog;

use super::{alloc::ALLOCATOR};


fn enable_uefi_vtl_protection() {
    let mut buf = vec![0u8; 1024];
    let mut str_buff = vec![0u16; 1024];
    let os_loader_indications_key =
        CStr16::from_str_with_buf(&"OsLoaderIndications", str_buff.as_mut_slice()).unwrap();

    let os_loader_indications_result = uefi::runtime::get_variable(
        os_loader_indications_key,
        &uefi::runtime::VariableVendor(guid!("610b9e98-c6f6-47f8-8b47-2d2da0d52a91")),
        buf.as_mut(),
    )
    .expect("Failed to get OsLoaderIndications");

    let mut os_loader_indications = u32::from_le_bytes(
        os_loader_indications_result.0[0..4]
            .try_into()
            .expect("error in output"),
    );
    os_loader_indications |= 0x1u32;

    let os_loader_indications = os_loader_indications.to_le_bytes();

    let _ = uefi::runtime::set_variable(
        os_loader_indications_key,
        &uefi::runtime::VariableVendor(guid!("610b9e98-c6f6-47f8-8b47-2d2da0d52a91")),
        os_loader_indications_result.1,
        &os_loader_indications,
    )
    .expect("Failed to set OsLoaderIndications");

    let os_loader_indications_result = uefi::runtime::get_variable(
        os_loader_indications_key,
        &uefi::runtime::VariableVendor(guid!("610b9e98-c6f6-47f8-8b47-2d2da0d52a91")),
        buf.as_mut(),
    )
    .expect("Failed to get OsLoaderIndications");

    let _ = unsafe { exit_boot_services(MemoryType::BOOT_SERVICES_DATA) };
}

pub fn init() -> Result<(), Status> {
    let r: bool = unsafe { ALLOCATOR.init(2048) };
    if r == false {
        return Err(Status::ABORTED);
    }
    enable_uefi_vtl_protection();
    Ok(())
}