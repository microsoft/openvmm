use uefi::{boot::{exit_boot_services, MemoryType}, guid, CStr16, Status};

use super::alloc::ALLOCATOR;

const EFI_GUID: uefi::Guid = guid!("610b9e98-c6f6-47f8-8b47-2d2da0d52a91");
const OS_LOADER_INDICATIONS: &'static str = "OsLoaderIndications";

fn enable_uefi_vtl_protection() {
    let mut buf = vec![0u8; 1024];
    let mut str_buff = vec![0u16; 1024];
    let os_loader_indications_key =
        CStr16::from_str_with_buf(OS_LOADER_INDICATIONS, str_buff.as_mut_slice()).unwrap();

    let os_loader_indications_result = uefi::runtime::get_variable(
        os_loader_indications_key,
        &uefi::runtime::VariableVendor(EFI_GUID),
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
        &uefi::runtime::VariableVendor(EFI_GUID),
        os_loader_indications_result.1,
        &os_loader_indications,
    )
    .expect("Failed to set OsLoaderIndications");

    let _os_loader_indications_result = uefi::runtime::get_variable(
        os_loader_indications_key,
        &uefi::runtime::VariableVendor(EFI_GUID),
        buf.as_mut(),
    )
    .expect("Failed to get OsLoaderIndications");

    let _memory_map = unsafe { exit_boot_services(MemoryType::BOOT_SERVICES_DATA) };
}

pub fn init() -> Result<(), Status> {
    let r: bool = ALLOCATOR.init(2048);
    if r == false {
        return Err(Status::ABORTED);
    }
    crate::tmk_logger::init().expect("Failed to init logger");
    enable_uefi_vtl_protection();
    Ok(())
}