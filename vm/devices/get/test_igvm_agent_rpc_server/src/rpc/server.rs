// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ffi::c_void;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::Foundation::TRUE;
use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
use windows_sys::Win32::System::Console::CTRL_C_EVENT;
use windows_sys::Win32::System::Console::CTRL_CLOSE_EVENT;
use windows_sys::Win32::System::Console::CTRL_SHUTDOWN_EVENT;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Rpc::RPC_C_LISTEN_MAX_CALLS_DEFAULT;
use windows_sys::Win32::System::Rpc::RPC_C_PROTSEQ_MAX_REQS_DEFAULT;
use windows_sys::Win32::System::Rpc::RPC_S_NOT_LISTENING;
use windows_sys::Win32::System::Rpc::RPC_S_OK;
use windows_sys::Win32::System::Rpc::RPC_STATUS;
use windows_sys::Win32::System::Rpc::RpcMgmtStopServerListening;
use windows_sys::Win32::System::Rpc::RpcServerListen;
use windows_sys::Win32::System::Rpc::RpcServerRegisterIf3;
use windows_sys::Win32::System::Rpc::RpcServerUnregisterIf;
use windows_sys::Win32::System::Rpc::RpcServerUseProtseqEpW;
use windows_sys::core::BOOL;

pub const PROTOCOL_SEQUENCE: &str = "ncalrpc";
pub const ENDPOINT: &str = "IGVM_AGENT_RPC_SERVER";

pub type RpcInterfaceHandle = *mut c_void;

// SAFETY: FFI handle
unsafe extern "C" {
    pub static IGVmAgentRpcApi_v1_0_s_ifspec: RpcInterfaceHandle;
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain([0]).collect()
}

/// # SAFETY
/// The callback for the passing through FFI
unsafe extern "system" fn rpc_bind_callback(
    _context: *const c_void,
    _uuid: *const c_void,
) -> RPC_STATUS {
    tracing::debug!("rpc_bind_callback invoked");
    RPC_S_OK
}

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// # SAFETY
/// Used by RPC handler registration.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_SHUTDOWN_EVENT => {
            if !STOP_REQUESTED.swap(true, Ordering::SeqCst) {
                tracing::info!("console control signal {ctrl_type} received; requesting shutdown");
                // SAFETY: Make an FFI call.
                let status = unsafe { RpcMgmtStopServerListening(ptr::null_mut()) };
                if status != RPC_S_OK && status != RPC_S_NOT_LISTENING {
                    tracing::error!("RpcMgmtStopServerListening failed: {status}");
                }
            }
            TRUE
        }
        _ => FALSE,
    }
}

struct ConsoleHandlerGuard;

impl ConsoleHandlerGuard {
    fn register() -> Result<Self, String> {
        // SAFETY: Make an FFI call.
        unsafe {
            if SetConsoleCtrlHandler(Some(console_ctrl_handler), TRUE) == 0 {
                return Err("SetConsoleCtrlHandler failed".to_owned());
            }
        }
        tracing::info!("console control handler registered");

        Ok(Self)
    }
}

impl Drop for ConsoleHandlerGuard {
    fn drop(&mut self) {
        // SAFETY: Make an FFI call.
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), FALSE);
        }
    }
}

fn register_protocol_and_interface() -> Result<(), String> {
    tracing::info!(protocol = %PROTOCOL_SEQUENCE, endpoint = %ENDPOINT, "registering RPC protocol");
    let mut protocol_seq = to_wide(PROTOCOL_SEQUENCE);
    let mut endpoint = to_wide(ENDPOINT);

    // SAFETY: Make an FFI call.
    let status = unsafe {
        RpcServerUseProtseqEpW(
            protocol_seq.as_mut_ptr(),
            RPC_C_PROTSEQ_MAX_REQS_DEFAULT,
            endpoint.as_mut_ptr(),
            ptr::null_mut(),
        )
    };

    if status != RPC_S_OK {
        tracing::error!(status = status, "RpcServerUseProtseqEpW failed");
        return Err(format!("RpcServerUseProtseqEpW failed: {status}"));
    }

    tracing::info!("RPC protocol bound successfully");

    // SAFETY: Make an FFI call.
    let status = unsafe {
        RpcServerRegisterIf3(
            IGVmAgentRpcApi_v1_0_s_ifspec,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            RPC_C_LISTEN_MAX_CALLS_DEFAULT,
            0,
            Some(rpc_bind_callback),
            ptr::null_mut(),
        )
    };
    if status != RPC_S_OK {
        tracing::error!(status = status, "RpcServerRegisterIf3 failed");
        return Err(format!("RpcServerRegisterIf3 failed: {status}"));
    }

    tracing::info!("RPC interface registered");
    Ok(())
}

fn unregister_interface() {
    // SAFETY: Make an FFI call.
    unsafe {
        RpcServerUnregisterIf(IGVmAgentRpcApi_v1_0_s_ifspec, ptr::null_mut(), 0);
    }
    tracing::info!("RPC interface unregistered");
}

/// Run the IGVM agent RPC server until it is interrupted.
pub fn run_server() -> Result<(), String> {
    tracing::info!("starting IGVM agent RPC server");
    register_protocol_and_interface()?;
    let _handler = ConsoleHandlerGuard::register()?;

    // Close stdout to signal that the server is ready to accept connections.
    // The test harness waits for stdout EOF before proceeding.
    // We must close the actual file descriptor, not just drop the Rust handle.
    // SAFETY: Make FFI calls.
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Console::GetStdHandle;
        use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
        let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if !stdout_handle.is_null() {
            CloseHandle(stdout_handle);
            tracing::info!("closed stdout to signal readiness");
        }
    }

    // SAFETY: Make an FFI call.
    let listen_status = unsafe { RpcServerListen(1, RPC_C_LISTEN_MAX_CALLS_DEFAULT, 0) };

    unregister_interface();

    if listen_status != RPC_S_OK {
        tracing::error!(status = listen_status, "RpcServerListen failed");
        return Err(format!("RpcServerListen failed: {listen_status}"));
    }

    tracing::info!("IGVM agent RPC server stopped cleanly");

    Ok(())
}
