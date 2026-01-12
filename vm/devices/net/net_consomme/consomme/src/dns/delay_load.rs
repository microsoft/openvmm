// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Delay-load helpers for dnsapi.dll functions.
//!
//! This module provides runtime loading of DNS API functions, allowing
//! the code to gracefully handle systems where certain APIs are not available.

// UNSAFETY: FFI calls to Windows API for library loading.
#![expect(unsafe_code)]

use std::ptr::null_mut;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::ERROR_PROC_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::WIN32_ERROR;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_CANCEL;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_REQUEST;
use windows_sys::Win32::NetworkManagement::Dns::DNS_QUERY_RAW_RESULT;
use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;

/// Sentinel value used in the function pointer cache to indicate that
/// a function was looked up but not found. This distinguishes between
/// "not yet loaded" (0) and "loaded but not found" (this value).
const FN_NOT_FOUND_SENTINEL: usize = 1;

/// Get handle to dnsapi.dll, loading it if necessary.
pub fn get_module() -> Result<isize, WIN32_ERROR> {
    static MODULE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());

    let mut module = MODULE.load(Ordering::Acquire);
    if module.is_null() {
        // SAFETY: FFI call to load dnsapi.dll
        module = unsafe { LoadLibraryA(c"dnsapi.dll".as_ptr().cast()).cast::<core::ffi::c_void>() };
        if module.is_null() {
            // SAFETY: FFI call to get last error code
            return Err(unsafe { GetLastError() });
        }
        // Use compare_exchange to prevent duplicate loading and ensure proper synchronization.
        // Release ordering ensures LoadLibraryA completes before other threads see the pointer.
        if let Err(current) =
            MODULE.compare_exchange(null_mut(), module, Ordering::Release, Ordering::Acquire)
        {
            // Another thread already loaded the module, use their handle.
            // Note: We could call FreeLibrary on our duplicate handle, but Windows
            // reference-counts library handles, so this small leak is harmless.
            module = current;
        }
    }
    Ok(module as isize)
}

/// Get a function pointer from dnsapi.dll, caching the result.
///
/// The cache uses three states:
/// - `0`: Not yet loaded
/// - `FN_NOT_FOUND_SENTINEL`: Function was looked up but not found in the DLL
/// - Any other value: The actual function pointer address
fn get_proc_address(name: &[u8], cache: &AtomicUsize) -> Result<usize, WIN32_ERROR> {
    let mut fnval = cache.load(Ordering::Acquire);
    if fnval == 0 {
        let module = get_module()?;
        // SAFETY: FFI call to get function address from module
        fnval = unsafe { GetProcAddress(module as _, name.as_ptr()) }
            .map(|f| f as usize)
            .unwrap_or(0);
        // Store sentinel for "not found" to distinguish from "not yet loaded"
        let store_val = if fnval == 0 {
            FN_NOT_FOUND_SENTINEL
        } else {
            fnval
        };
        // Use compare_exchange to prevent duplicate loading and ensure proper synchronization.
        // Release ordering ensures GetProcAddress completes before other threads see the value.
        if let Err(current) =
            cache.compare_exchange(0, store_val, Ordering::Release, Ordering::Acquire)
        {
            // Another thread already cached the value, use theirs
            fnval = current;
        }
    }
    if fnval == FN_NOT_FOUND_SENTINEL {
        Err(ERROR_PROC_NOT_FOUND)
    } else {
        Ok(fnval)
    }
}

/// Macro to define a delay-loaded function getter.
macro_rules! define_dns_api {
    ($fn_name:ident, $api_name:literal) => {
        pub fn $fn_name() -> Result<usize, WIN32_ERROR> {
            static CACHE: AtomicUsize = AtomicUsize::new(0);
            get_proc_address(concat!($api_name, "\0").as_bytes(), &CACHE)
        }
    };
}

// DnsQueryRaw APIs (Windows 11+)
define_dns_api!(get_dns_query_raw, "DnsQueryRaw");
define_dns_api!(get_dns_cancel_query_raw, "DnsCancelQueryRaw");
define_dns_api!(get_dns_query_raw_result_free, "DnsQueryRawResultFree");

/// Check if DnsQueryRaw APIs are available (Windows 11+).
pub fn is_dns_raw_apis_supported() -> bool {
    get_dns_query_raw().is_ok()
        && get_dns_cancel_query_raw().is_ok()
        && get_dns_query_raw_result_free().is_ok()
}

/// Function signature for DnsQueryRaw.
pub type DnsQueryRawFn =
    unsafe extern "system" fn(*const DNS_QUERY_RAW_REQUEST, *mut DNS_QUERY_RAW_CANCEL) -> i32;

/// Function signature for DnsCancelQueryRaw.
pub type DnsCancelQueryRawFn = unsafe extern "system" fn(*const DNS_QUERY_RAW_CANCEL) -> i32;

/// Function signature for DnsQueryRawResultFree.
pub type DnsQueryRawResultFreeFn = unsafe extern "system" fn(*mut DNS_QUERY_RAW_RESULT);

/// Get DnsQueryRaw as a typed function pointer.
///
/// # Safety
///
/// The returned function pointer must only be called with valid arguments.
pub unsafe fn get_dns_query_raw_fn() -> Result<DnsQueryRawFn, WIN32_ERROR> {
    let fnval = get_dns_query_raw()?;
    // SAFETY: Function pointer has the correct signature for DnsQueryRaw
    Ok(unsafe { std::mem::transmute::<usize, DnsQueryRawFn>(fnval) })
}

/// Get DnsCancelQueryRaw as a typed function pointer.
///
/// # Safety
///
/// The returned function pointer must only be called with valid arguments.
pub unsafe fn get_dns_cancel_query_raw_fn() -> Result<DnsCancelQueryRawFn, WIN32_ERROR> {
    let fnval = get_dns_cancel_query_raw()?;
    // SAFETY: Function pointer has the correct signature for DnsCancelQueryRaw
    Ok(unsafe { std::mem::transmute::<usize, DnsCancelQueryRawFn>(fnval) })
}

/// Get DnsQueryRawResultFree as a typed function pointer.
///
/// # Safety
///
/// The returned function pointer must only be called with valid arguments.
pub unsafe fn get_dns_query_raw_result_free_fn() -> Result<DnsQueryRawResultFreeFn, WIN32_ERROR> {
    let fnval = get_dns_query_raw_result_free()?;
    // SAFETY: Function pointer has the correct signature for DnsQueryRawResultFree
    Ok(unsafe { std::mem::transmute::<usize, DnsQueryRawResultFreeFn>(fnval) })
}
