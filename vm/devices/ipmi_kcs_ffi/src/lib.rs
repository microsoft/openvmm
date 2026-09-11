// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! C ABI staticlib over [`ipmi_kcs_core`], for linking the shared IPMI KCS BMC
//! into non-Rust hosts (the Legacy HCL C++ in the OS Repo).
//!
//! The host drives the device through [`ipmi_kcs_io_read`] / [`ipmi_kcs_io_write`]
//! and supplies two callbacks: one invoked when a SEL entry is committed (to
//! forward it, e.g. to ETW), and one returning wall-clock time. SEL/KCS logic is
//! shared with OpenHCL via the common [`ipmi_kcs_core`] crate — no duplicated
//! implementation.
//!
//! The shared core is `no_std` + `alloc`; this thin shim is `std` (matching the
//! `vmgs_lib` C-ABI precedent) so it links and unit-tests cleanly. The consuming
//! OS Repo build controls the final link (and may compile with `panic = "abort"`
//! to drop unwinding for minimal footprint).

// UNSAFETY: Exporting `no_mangle extern "C"` functions over raw pointers.
#![expect(unsafe_code)]

use ipmi_kcs_core::KcsDevice;
use ipmi_kcs_core::KcsError;
use ipmi_kcs_core::sink::BmcClock;
use ipmi_kcs_core::sink::SelDeps;
use ipmi_kcs_core::sink::SelSink;
use std::ffi::c_void;
use std::sync::Arc;

/// Callback invoked after a SEL entry is committed.
///
/// `ctx` is the opaque pointer supplied at construction; `record` points to
/// `record_len` bytes (always [`ipmi_kcs_core::SEL_RECORD_SIZE`]) valid only for
/// the duration of the call.
pub type SelCallback =
    Option<unsafe extern "C" fn(ctx: *mut c_void, record_id: u16, record: *const u8, record_len: usize)>;

/// Callback returning the current wall-clock time in seconds since the Unix epoch.
pub type ClockCallback = Option<unsafe extern "C" fn(ctx: *mut c_void) -> i64>;

/// Return code: success.
pub const IPMI_KCS_OK: i32 = 0;
/// Return code: the device handle (or output pointer) was null.
pub const IPMI_KCS_NULL: i32 = -1;
/// Return code: the accessed port is not a KCS register.
pub const IPMI_KCS_INVALID_REGISTER: i32 = -2;

/// SEL sink that forwards to a C callback.
struct CSelSink {
    cb: SelCallback,
    ctx: *mut c_void,
}

// SAFETY: The device is driven single-threaded by the host VP intercept; the
// host is responsible for not sharing the context across threads concurrently.
unsafe impl Send for CSelSink {}
// SAFETY: See above.
unsafe impl Sync for CSelSink {}

impl SelSink for CSelSink {
    fn log_sel_entry(&self, record_id: u16, record: &[u8]) {
        if let Some(cb) = self.cb {
            // SAFETY: `record` is valid for its length for the call; `ctx` is the
            // host-provided opaque pointer.
            unsafe { cb(self.ctx, record_id, record.as_ptr(), record.len()) }
        }
    }
}

/// Clock backed by a C callback.
struct CClock {
    cb: ClockCallback,
    ctx: *mut c_void,
}

// SAFETY: See `CSelSink`.
unsafe impl Send for CClock {}
// SAFETY: See `CSelSink`.
unsafe impl Sync for CClock {}

impl BmcClock for CClock {
    fn now_unix_secs(&self) -> i64 {
        match self.cb {
            // SAFETY: `ctx` is the host-provided opaque pointer.
            Some(cb) => unsafe { cb(self.ctx) },
            None => 0,
        }
    }
}

/// Create a new IPMI KCS device.
///
/// `sel_cb`/`sel_ctx` receive each committed SEL entry; either callback may be
/// null. Returns an owned handle that must be released with [`ipmi_kcs_free`].
///
/// # Safety
///
/// The callbacks, if non-null, must be valid C function pointers; the contexts
/// must remain valid for the lifetime of the returned device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ipmi_kcs_new(
    sel_cb: SelCallback,
    sel_ctx: *mut c_void,
    clock_cb: ClockCallback,
    clock_ctx: *mut c_void,
) -> *mut KcsDevice {
    let sink = Arc::new(CSelSink {
        cb: sel_cb,
        ctx: sel_ctx,
    });
    let clock = Arc::new(CClock {
        cb: clock_cb,
        ctx: clock_ctx,
    });
    let dev = Box::new(KcsDevice::with_deps(SelDeps::new(sink, clock)));
    Box::into_raw(dev)
}

/// Free a device created by [`ipmi_kcs_new`]. Passing null is a no-op.
///
/// # Safety
///
/// `dev` must be a handle returned by [`ipmi_kcs_new`] and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ipmi_kcs_free(dev: *mut KcsDevice) {
    if !dev.is_null() {
        // SAFETY: `dev` is a valid handle per the contract.
        drop(unsafe { Box::from_raw(dev) });
    }
}

/// Read a KCS register into `*out_byte`.
///
/// Returns [`IPMI_KCS_OK`], [`IPMI_KCS_NULL`], or [`IPMI_KCS_INVALID_REGISTER`].
///
/// # Safety
///
/// `dev` must be a valid handle; `out_byte` must be a valid writable `u8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ipmi_kcs_io_read(
    dev: *mut KcsDevice,
    port: u16,
    out_byte: *mut u8,
) -> i32 {
    if dev.is_null() || out_byte.is_null() {
        return IPMI_KCS_NULL;
    }
    // SAFETY: `dev` is a valid handle per the contract.
    let dev = unsafe { &mut *dev };
    match dev.io_read(port) {
        Ok(byte) => {
            // SAFETY: `out_byte` is a valid writable pointer per the contract.
            unsafe { *out_byte = byte };
            IPMI_KCS_OK
        }
        Err(KcsError::InvalidRegister) => IPMI_KCS_INVALID_REGISTER,
    }
}

/// Write a byte to a KCS register.
///
/// Returns [`IPMI_KCS_OK`], [`IPMI_KCS_NULL`], or [`IPMI_KCS_INVALID_REGISTER`].
///
/// # Safety
///
/// `dev` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ipmi_kcs_io_write(dev: *mut KcsDevice, port: u16, byte: u8) -> i32 {
    if dev.is_null() {
        return IPMI_KCS_NULL;
    }
    // SAFETY: `dev` is a valid handle per the contract.
    let dev = unsafe { &mut *dev };
    match dev.io_write(port, byte) {
        Ok(()) => IPMI_KCS_OK,
        Err(KcsError::InvalidRegister) => IPMI_KCS_INVALID_REGISTER,
    }
}

/// Reset the device to IDLE and clear the SEL.
///
/// # Safety
///
/// `dev` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ipmi_kcs_reset(dev: *mut KcsDevice) {
    if !dev.is_null() {
        // SAFETY: `dev` is a valid handle per the contract.
        unsafe { &mut *dev }.reset();
    }
}

/// The KCS data register I/O port (0xCA2).
#[unsafe(no_mangle)]
pub extern "C" fn ipmi_kcs_data_port() -> u16 {
    ipmi_kcs_core::protocol::KCS_DATA_REG
}

/// The KCS status/command register I/O port (0xCA3).
#[unsafe(no_mangle)]
pub extern "C" fn ipmi_kcs_status_port() -> u16 {
    ipmi_kcs_core::protocol::KCS_STATUS_CMD_REG
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    static SEL_COUNT: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn sel_cb(_ctx: *mut c_void, record_id: u16, record: *const u8, len: usize) {
        assert_eq!(len, ipmi_kcs_core::SEL_RECORD_SIZE);
        // SAFETY: record points to len valid bytes for the call.
        let slice = unsafe { std::slice::from_raw_parts(record, len) };
        assert_eq!(u16::from_le_bytes([slice[0], slice[1]]), record_id);
        SEL_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn clock_cb(_ctx: *mut c_void) -> i64 {
        1_700_000_000
    }

    /// Drive a Get Device ID transaction over the C ABI.
    #[test]
    fn ffi_get_device_id() {
        // SAFETY: valid callbacks, null contexts.
        let dev = unsafe {
            ipmi_kcs_new(Some(sel_cb), std::ptr::null_mut(), Some(clock_cb), std::ptr::null_mut())
        };
        assert!(!dev.is_null());

        assert_eq!(ipmi_kcs_data_port(), 0xCA2);
        assert_eq!(ipmi_kcs_status_port(), 0xCA3);

        let mut byte = 0u8;
        let wr = |port, b| unsafe { ipmi_kcs_io_write(dev, port, b) };
        let rd = |port, out: &mut u8| unsafe { ipmi_kcs_io_read(dev, port, out) };

        assert_eq!(wr(0xCA3, 0x61), IPMI_KCS_OK); // WRITE_START
        rd(0xCA2, &mut byte);
        assert_eq!(wr(0xCA2, 0x18), IPMI_KCS_OK); // NetFn/LUN
        assert_eq!(wr(0xCA3, 0x62), IPMI_KCS_OK); // WRITE_END
        rd(0xCA2, &mut byte);
        assert_eq!(wr(0xCA2, 0x01), IPMI_KCS_OK); // cmd Get Device ID

        let mut resp = Vec::new();
        loop {
            let mut s = 0u8;
            rd(0xCA3, &mut s);
            let mut d = 0u8;
            rd(0xCA2, &mut d);
            if s & 0xC0 != 0x40 {
                break;
            }
            resp.push(d);
            wr(0xCA2, 0x68); // READ ack
        }
        assert_eq!(resp[0], 0x1C);
        assert_eq!(resp[2], 0x00); // success
        assert_eq!(resp[3], 0x20); // device id

        // SAFETY: valid handle.
        unsafe { ipmi_kcs_free(dev) };
    }

    /// Invalid register is reported; SEL callback fires on add.
    #[test]
    fn ffi_invalid_register_and_sel_callback() {
        SEL_COUNT.store(0, Ordering::SeqCst);
        // SAFETY: valid callbacks.
        let dev = unsafe {
            ipmi_kcs_new(Some(sel_cb), std::ptr::null_mut(), Some(clock_cb), std::ptr::null_mut())
        };
        let mut byte = 0u8;
        // SAFETY: valid handle.
        assert_eq!(unsafe { ipmi_kcs_io_read(dev, 0xCA4, &mut byte) }, IPMI_KCS_INVALID_REGISTER);

        // Add a SEL entry: NetFn Storage(0x28), cmd ADD(0x44) + 16 bytes.
        let mut req = vec![0x28u8, 0x44];
        req.extend_from_slice(&[0u8; 16]);
        let wr = |port, b| unsafe { ipmi_kcs_io_write(dev, port, b) };
        let rd = |port, out: &mut u8| unsafe { ipmi_kcs_io_read(dev, port, out) };
        wr(0xCA3, 0x61);
        for &b in &req[..req.len() - 1] {
            rd(0xCA2, &mut byte);
            wr(0xCA2, b);
        }
        wr(0xCA3, 0x62);
        rd(0xCA2, &mut byte);
        wr(0xCA2, *req.last().unwrap());
        // Drain response.
        loop {
            let mut s = 0u8;
            rd(0xCA3, &mut s);
            rd(0xCA2, &mut byte);
            if s & 0xC0 != 0x40 {
                break;
            }
            wr(0xCA2, 0x68);
        }
        assert_eq!(SEL_COUNT.load(Ordering::SeqCst), 1);

        // SAFETY: valid handle.
        unsafe { ipmi_kcs_free(dev) };
    }
}
