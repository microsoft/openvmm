// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::rpc::igvm_agent;
use guid::Guid;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::slice;
use windows_sys::Win32::Foundation::E_FAIL;
use windows_sys::Win32::Foundation::E_INVALIDARG;
use windows_sys::Win32::Foundation::E_OUTOFMEMORY;
use windows_sys::Win32::Foundation::E_POINTER;
use windows_sys::Win32::Foundation::S_OK;
use windows_sys::Win32::System::Rpc::RPC_S_SERVER_UNAVAILABLE;
use windows_sys::Win32::System::Rpc::RpcRaiseException;
use windows_sys::core::HRESULT;

#[unsafe(no_mangle)]
/// Allocator shim invoked by the generated MIDL stubs.
/// # SAFETY
/// Define FFI to fullfil the linker requirement
pub unsafe extern "C" fn MIDL_user_allocate(size: usize) -> *mut c_void {
    use windows_sys::Win32::System::Com::CoTaskMemAlloc;
    // SAFETY: make an FFI call
    unsafe { CoTaskMemAlloc(size) }
}

#[unsafe(no_mangle)]
/// Deallocator shim invoked by the generated MIDL stubs.
/// # SAFETY
/// Define FFI to fullfil the linker requirement
pub unsafe extern "C" fn MIDL_user_free(ptr: *mut c_void) {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    if !ptr.is_null() {
        // SAFETY: make an FFI call
        unsafe {
            CoTaskMemFree(ptr);
        }
    }
}

// Constants from IDL
const GSP_MAX_CLEAR_SIZE: usize = 64;
const GSP_MAX_CIPHER_SIZE: usize = 512;
const GSP_MAX_COUNT: usize = 2;
/// IDL `StatusFlagStateRefreshRequest` — request a TPM state refresh.
/// Maps to `GspExtendedStatusFlags::state_refresh_request` (bit 0).
const STATUS_FLAG_STATE_REFRESH_REQUEST: u32 = 0x1;

/// GSP clear text buffer (matches IDL GspClear)
#[repr(C)]
pub struct GspClear {
    pub length: u32,
    pub buffer: [u8; GSP_MAX_CLEAR_SIZE],
}

/// GSP encrypted buffer (matches IDL GspCipher)
#[repr(C)]
pub struct GspCipher {
    pub length: u32,
    pub buffer: [u8; GSP_MAX_CIPHER_SIZE],
}

/// VM GSP request payload descriptor provided by the RPC caller (matches IDL GspRequestInfo).
#[repr(C)]
pub struct GspRequestInfo {
    pub new_gsp: GspClear,
    pub encrypted_gsp: [GspCipher; GSP_MAX_COUNT],
    pub supported_status_flags: u32,
}

/// VM GSP response buffer descriptor owned by the RPC caller (matches IDL GspResponseInfo).
#[repr(C)]
pub struct GspResponseInfo {
    pub encrypted_gsp: GspCipher,
    pub decrypted_gsp: [GspClear; GSP_MAX_COUNT],
    pub response_status_flags: u32,
}

// Compile-time size checks to ensure structures match IDL definitions
// GspClear: 4 (length) + 64 (buffer) = 68 bytes
const _: () = assert!(size_of::<GspClear>() == 68);
// GspCipher: 4 (length) + 512 (buffer) = 516 bytes
const _: () = assert!(size_of::<GspCipher>() == 516);
// GspRequestInfo: 68 (GspClear) + 1032 (2 x GspCipher) + 4 (flags) = 1104 bytes
const _: () = assert!(size_of::<GspRequestInfo>() == 1104);
// GspResponseInfo: 516 (GspCipher) + 136 (2 x GspClear) + 4 (flags) = 656 bytes
const _: () = assert!(size_of::<GspResponseInfo>() == 656);

fn write_response_size(ptr: *mut u32, value: u32) -> Result<(), HRESULT> {
    if ptr.is_null() {
        Err(E_POINTER)
    } else {
        // SAFETY: memory access
        unsafe {
            *ptr = value;
        }
        Ok(())
    }
}

/// Copies `buffer` to the destination pointer `dest`.
///
/// # Safety Requirements
/// The caller must ensure:
/// - `dest` is valid for writes of `buffer.len()` bytes
/// - `dest` does not overlap with `buffer`
fn copy_to_buffer(buffer: &[u8], dest: *mut u8, dest_size: usize) {
    debug_assert!(
        buffer.len() <= dest_size,
        "buffer length {} exceeds destination size {}",
        buffer.len(),
        dest_size
    );
    debug_assert!(!dest.is_null() || buffer.is_empty());

    if !buffer.is_empty() {
        // SAFETY: Caller guarantees dest has sufficient space (verified by debug_assert above).
        unsafe {
            ptr::copy_nonoverlapping(buffer.as_ptr(), dest, buffer.len());
        }
    }
}

fn format_hresult(hr: HRESULT) -> String {
    format!("{:#010x}", hr as u32)
}

fn read_guid(ptr: *const Guid) -> Option<Guid> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: memory access
        Some(unsafe { *ptr })
    }
}

fn read_utf16(ptr: *const u16) -> Option<String> {
    const MAX_LEN: usize = 1024;

    if ptr.is_null() {
        return None;
    }

    // SAFETY: The caller (RPC runtime) is responsible for providing valid pointers.
    // This is a test server, so we trust the RPC infrastructure to provide valid data.
    unsafe {
        let mut len = 0usize;

        // Scan for null terminator with bounds checking
        while len < MAX_LEN {
            if *ptr.add(len) == 0 {
                break;
            }
            len += 1;
        }

        // If we hit MAX_LEN without finding a null terminator, truncate
        if len == MAX_LEN {
            len = MAX_LEN - 1;
        }

        let slice = slice::from_raw_parts(ptr, len);
        String::from_utf16(slice).ok()
    }
}

/// Entry point that services `RpcIGVmAttest` requests for the test agent.
// SAFETY: FFI
#[unsafe(export_name = "RpcIGVmAttest")]
pub extern "system" fn rpc_igvm_attest(
    _binding_handle: *mut c_void,
    vm_id: *const Guid,
    request_id: *const Guid,
    vm_name: *const u16,
    agent_data_size: u32,
    _agent_data: *const u8,
    report_size: u32,
    report: *const u8,
    response_buffer_size: u32,
    response_written_size: *mut u32,
    response: *mut u8,
) -> HRESULT {
    let vm_id_str = read_guid(vm_id).map(|g| g.to_string());
    let request_id_str = read_guid(request_id).map(|g| g.to_string());
    let vm_name_str = read_utf16(vm_name);

    tracing::info!(
        vm_id = vm_id_str.as_deref().unwrap_or("<null>"),
        request_id = request_id_str.as_deref().unwrap_or("<null>"),
        vm_name = vm_name_str.as_deref().unwrap_or("<unknown>"),
        agent_data_size,
        report_size,
        response_buffer_size,
        "RpcIGVmAttest request received"
    );

    if let Err(err) = write_response_size(response_written_size, 0) {
        tracing::error!(
            hresult = format_hresult(err),
            "failed to clear response size"
        );
        return err;
    }

    // SAFETY: memory access
    let report_slice = unsafe {
        if report_size == 0 {
            &[][..]
        } else if report.is_null() {
            tracing::error!("report pointer is null while report_size > 0");
            return E_INVALIDARG;
        } else {
            slice::from_raw_parts(report, report_size as usize)
        }
    };

    tracing::debug!(
        payload_bytes = report_slice.len(),
        "invoking attest igvm_agent"
    );

    let vm_name_ref = vm_name_str.as_deref().unwrap_or("");
    let payload = match igvm_agent::process_igvm_attest(read_guid(vm_id), vm_name_ref, report_slice)
    {
        Ok(payload) => payload,
        Err(err) => {
            tracing::error!(?err, "igvm_agent::process_igvm_attest failed");
            return E_FAIL;
        }
    };

    let payload_len = payload.len() as u32;

    if payload_len > response_buffer_size {
        tracing::warn!(
            required = payload_len,
            available = response_buffer_size,
            "response buffer too small for attest payload"
        );
        return E_OUTOFMEMORY;
    }

    if payload_len > 0 {
        if response.is_null() {
            tracing::error!("response buffer pointer is null while payload_len > 0");
            return E_INVALIDARG;
        }
        copy_to_buffer(&payload, response, response_buffer_size as usize);
    }

    if let Err(err) = write_response_size(response_written_size, payload_len) {
        tracing::error!(hresult = format_hresult(err), "failed to set response size");
        return err;
    }

    tracing::info!(
        response_size = payload_len,
        "RpcIGVmAttest completed successfully"
    );

    S_OK
}

/// Entry point that services `RpcVmGspRequest` calls for the test agent.
///
/// By default this is disabled and raises `RPC_S_SERVER_UNAVAILABLE`, so the
/// host falls back to its own GSP handling (the original behavior).
///
/// When the VM's resolved test config is `IgvmAttestTestConfig::StateRefresh`,
/// the handler instead returns `S_OK` with `response_status_flags` carrying
/// `StatusFlagStateRefreshRequest`. OpenHCL propagates this into
/// `refresh_tpm_seeds`, regenerating the vTPM seeds (and the AK) on the next
/// boot.
///
/// NOTE: This is a test stub that acts as the GSP authority using an *identity
/// cipher*: the new cleartext seed is echoed back verbatim as the "encrypted"
/// blob, and each previously-stored "encrypted" blob is echoed back verbatim as
/// its "decrypted" seed. This is self-consistent across boots because this stub
/// is the sole producer and consumer of those blobs. Returning real, non-empty
/// GSP seed data is required: OpenHCL treats an empty `encrypted_gsp` as
/// "GSP-by-key unavailable" (`no_gsp_available = encrypted_gsp.length == 0`),
/// which would both drop the `state_refresh_request` flag from the GSP-by-key
/// response and prevent the VMGS from being unlocked across boots.
// SAFETY: FFI
#[unsafe(export_name = "RpcVmGspRequest")]
pub extern "system" fn rpc_vm_gsp_request(
    _binding_handle: *mut c_void,
    _vm_id: *const Guid,
    _vm_name: *const u16,
    request_data: *const GspRequestInfo,
    response_data: *mut GspResponseInfo,
) -> HRESULT {
    // Log the request parameters
    let vm_id = read_guid(_vm_id);
    let vm_id_str = vm_id.map(|g| g.to_string());
    let vm_name_str = read_utf16(_vm_name);

    // Now we can safely dereference the structures since they match the IDL definitions
    let (new_gsp_len, encrypted_gsp_count, supported_flags) = if !request_data.is_null() {
        // SAFETY: memory access
        let request = unsafe { &*request_data };
        let encrypted_count = request
            .encrypted_gsp
            .iter()
            .filter(|gsp| gsp.length > 0)
            .count();
        (
            request.new_gsp.length,
            encrypted_count,
            request.supported_status_flags,
        )
    } else {
        (0, 0, 0)
    };

    // Determine whether the resolved test config wants a state refresh.
    //
    // The GSP RPC's `VmName` is the VM's runtime GUID rather than the
    // descriptive Hyper-V name, so the config is looked up by `VmId` (recorded
    // by the IGVM attest path, which runs earlier in the same boot).
    let gsp_state_refresh = vm_id
        .as_ref()
        .is_some_and(igvm_agent::gsp_state_refresh_requested);

    if !gsp_state_refresh {
        tracing::warn!(
            vm_id = vm_id_str.as_deref().unwrap_or("<null>"),
            vm_name = vm_name_str.as_deref().unwrap_or("<unknown>"),
            new_gsp_length = new_gsp_len,
            encrypted_gsp_count = encrypted_gsp_count,
            supported_status_flags = supported_flags,
            "RpcVmGspRequest called but support is disabled - raising RPC_S_SERVER_UNAVAILABLE"
        );

        // Raise RPC_S_SERVER_UNAVAILABLE exception
        // SAFETY: Make an FFI call
        unsafe {
            RpcRaiseException(RPC_S_SERVER_UNAVAILABLE);
        }

        // This line is never reached due to RpcRaiseException
        unreachable!();
    }

    if request_data.is_null() {
        tracing::error!("request_data pointer is null");
        return E_POINTER;
    }

    if response_data.is_null() {
        tracing::error!("response_data pointer is null");
        return E_POINTER;
    }

    // Build a self-consistent GSP-by-key response using an identity cipher so
    // that OpenHCL sees GSP-by-key as available, carries the state-refresh flag
    // through the GSP-by-key response, and can unlock the VMGS across boots.
    //
    // A real IGVM agent encrypts the request's `new_gsp` seed and decrypts the
    // request's stored `encrypted_gsp` blobs. Here we use an identity transform
    // (ciphertext == cleartext), which round-trips correctly because this stub
    // is the only entity that produces and consumes these blobs.
    //
    // SAFETY: The RPC runtime provides valid, correctly-sized `request_data`
    // and `response_data` pointers (both verified non-null above) matching the
    // IDL `GspRequestInfo` / `GspResponseInfo` layouts.
    unsafe {
        let request = &*request_data;
        let response = &mut *response_data;

        response.encrypted_gsp.length = 0;
        response.encrypted_gsp.buffer = [0; GSP_MAX_CIPHER_SIZE];
        for clear in response.decrypted_gsp.iter_mut() {
            clear.length = 0;
            clear.buffer = [0; GSP_MAX_CLEAR_SIZE];
        }

        // "Encrypt" the new cleartext seed: echo it back verbatim as the
        // ciphertext blob that OpenHCL persists and replays next boot.
        let new_gsp_copy_len = (request.new_gsp.length as usize).min(GSP_MAX_CLEAR_SIZE);
        response.encrypted_gsp.length = new_gsp_copy_len as u32;
        response.encrypted_gsp.buffer[..new_gsp_copy_len]
            .copy_from_slice(&request.new_gsp.buffer[..new_gsp_copy_len]);

        // "Decrypt" each previously-stored ciphertext back into its
        // cleartext seed (identity transform).
        for (clear, cipher) in response
            .decrypted_gsp
            .iter_mut()
            .zip(request.encrypted_gsp.iter())
        {
            let len = (cipher.length as usize).min(GSP_MAX_CLEAR_SIZE);
            clear.length = len as u32;
            clear.buffer[..len].copy_from_slice(&cipher.buffer[..len]);
        }

        response.response_status_flags = STATUS_FLAG_STATE_REFRESH_REQUEST;
    }

    tracing::info!(
        vm_id = vm_id_str.as_deref().unwrap_or("<null>"),
        vm_name = vm_name_str.as_deref().unwrap_or("<unknown>"),
        new_gsp_length = new_gsp_len,
        encrypted_gsp_count = encrypted_gsp_count,
        supported_status_flags = supported_flags,
        "RpcVmGspRequest returning state_refresh_request per test config"
    );

    S_OK
}
