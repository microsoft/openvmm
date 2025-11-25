// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::ptr;
use std::slice;
use crate::rpc::igvm_agent;
use guid::Guid;

type HRESULT = i32;

const S_OK: HRESULT = 0;
const E_FAIL: HRESULT = 0x8000_4005u32 as HRESULT;
const E_INVALIDARG: HRESULT = 0x8007_0057u32 as HRESULT;
const E_POINTER: HRESULT = 0x8000_4003u32 as HRESULT;
const HRESULT_INSUFFICIENT_BUFFER: HRESULT = 0x8007_007Au32 as HRESULT;

#[unsafe(no_mangle)]
/// Allocator shim invoked by the generated MIDL stubs.
pub unsafe extern "C" fn MIDL_user_allocate(size: usize) -> *mut c_void {
    use windows_sys::Win32::System::Com::CoTaskMemAlloc;
    unsafe { CoTaskMemAlloc(size) }
}

#[unsafe(no_mangle)]
/// Deallocator shim invoked by the generated MIDL stubs.
pub unsafe extern "C" fn MIDL_user_free(ptr: *mut c_void) {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    if !ptr.is_null() {
        unsafe {
            CoTaskMemFree(ptr);
        }
    }
}

/// VM GSP request payload descriptor provided by the RPC caller.
#[repr(C)]
pub struct GspRequestInfo {
    /// Pointer to the request payload buffer.
    pub request_buffer: *const u8,
    /// Size of the request payload in bytes.
    pub request_size: u32,
}

/// VM GSP response buffer descriptor owned by the RPC caller.
#[repr(C)]
pub struct GspResponseInfo {
    /// Pointer to the writable response buffer.
    pub response_buffer: *mut u8,
    /// Capacity of the response buffer in bytes.
    pub response_buffer_size: u32,
    /// Actual number of bytes written to the response buffer.
    pub response_size: u32,
}

fn write_response_size(ptr: *mut u32, value: u32) -> Result<(), HRESULT> {
    if ptr.is_null() {
        Err(E_POINTER)
    } else {
        unsafe {
            *ptr = value;
        }
        Ok(())
    }
}

fn copy_to_buffer(buffer: &[u8], dest: *mut u8) {
    if !buffer.is_empty() {
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
        Some(unsafe { *ptr })
    }
}

fn read_utf16(ptr: *const u16) -> Option<String> {
    const MAX_LEN: usize = 1024;

    if ptr.is_null() {
        return None;
    }

    unsafe {
        let mut len = 0usize;
        while len < MAX_LEN {
            if *ptr.add(len) == 0 {
                break;
            }
            len += 1;
        }

        if len == MAX_LEN {
            return None;
        }

        let slice = slice::from_raw_parts(ptr, len);
        String::from_utf16(slice).ok()
    }
}

/// Entry point that services `RpcIGVmAttest` requests for the test agent.
#[unsafe(export_name = "RpcIGVmAttest")]
pub extern "system" fn rpc_igvm_attest(
    _binding_handle: *mut c_void,
    _vm_id: *const Guid,
    _request_id: *const Guid,
    _vm_name: *const u16,
    _agent_data_size: u32,
    _agent_data: *const u8,
    report_size: u32,
    report: *const u8,
    response_buffer_size: u32,
    response_written_size: *mut u32,
    response: *mut u8,
) -> HRESULT {
    let vm_id_str = read_guid(_vm_id).map(|g| g.to_string());
    let request_id_str = read_guid(_request_id).map(|g| g.to_string());
    let vm_name_str = read_utf16(_vm_name);

    tracing::info!(
        vm_id = vm_id_str.as_deref().unwrap_or("<null>"),
        request_id = request_id_str.as_deref().unwrap_or("<null>"),
        vm_name = vm_name_str.as_deref().unwrap_or("<unknown>"),
        report_size,
        response_buffer_size,
        "RpcIGVmAttest request received"
    );

    if let Err(err) = write_response_size(response_written_size, 0) {
        tracing::error!(hresult = format_hresult(err), "failed to clear response size");
        return err;
    }

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

    tracing::debug!(payload_bytes = report_slice.len(), "invoking attest igvm_agent");

    let payload = match igvm_agent::process_igvm_attest(report_slice) {
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
        let _ = write_response_size(response_written_size, payload_len);
        return HRESULT_INSUFFICIENT_BUFFER;
    }

    if payload_len > 0 {
        if response.is_null() {
            tracing::error!("response buffer pointer is null while payload_len > 0");
            return E_INVALIDARG;
        }
        copy_to_buffer(&payload, response);
    }

    if let Err(err) = write_response_size(response_written_size, payload_len) {
        tracing::error!(hresult = format_hresult(err), "failed to set response size");
        return err;
    }

    tracing::info!(response_size = payload_len, "RpcIGVmAttest completed successfully");

    S_OK
}

/// Entry point that services `RpcVmGspRequest` calls for the test agent.
#[unsafe(export_name = "RpcVmGspRequest")]
pub extern "system" fn rpc_vm_gsp_request(
    _binding_handle: *mut c_void,
    _vm_id: *const Guid,
    _vm_name: *const u16,
    request_data: *const GspRequestInfo,
    response_data: *mut GspResponseInfo,
) -> HRESULT {
    if request_data.is_null() || response_data.is_null() {
        tracing::error!("RpcVmGspRequest received null descriptor pointer");
        return E_INVALIDARG;
    }

    let request = unsafe { &*request_data };
    let response = unsafe { &mut *response_data };

    let vm_id_str = read_guid(_vm_id).map(|g| g.to_string());
    let vm_name_str = read_utf16(_vm_name);

    tracing::info!(
        vm_id = vm_id_str.as_deref().unwrap_or("<null>"),
        vm_name = vm_name_str.as_deref().unwrap_or("<unknown>"),
        request_size = request.request_size,
        response_capacity = response.response_buffer_size,
        "RpcVmGspRequest received"
    );

    if request.request_size > 0 && request.request_buffer.is_null() {
        tracing::error!("request buffer pointer is null while request_size > 0");
        return E_INVALIDARG;
    }

    if response.response_buffer_size > 0 && response.response_buffer.is_null() {
        tracing::error!("response buffer pointer is null while response_buffer_size > 0");
        return E_POINTER;
    }

    let request_bytes = if request.request_size == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(request.request_buffer, request.request_size as usize) }
    };

    tracing::debug!(payload_bytes = request_bytes.len(), "invoking VM GSP igvm_agent");

    let payload = igvm_agent::process_vm_gsp_request(request_bytes);
    let payload_len = payload.len() as u32;

    if payload_len > response.response_buffer_size {
        tracing::warn!(
            required = payload_len,
            available = response.response_buffer_size,
            "response buffer too small for VM GSP payload"
        );
        response.response_size = payload_len;
        return HRESULT_INSUFFICIENT_BUFFER;
    }

    if payload_len > 0 {
        copy_to_buffer(&payload, response.response_buffer);
    }

    response.response_size = payload_len;
    tracing::info!(response_size = payload_len, "RpcVmGspRequest completed successfully");

    S_OK
}
