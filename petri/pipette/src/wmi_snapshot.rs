// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! One-shot, memory-free diagnostics for WMI provider-host activation.

// UNSAFETY: Windows wait-chain, process-snapshot, and symbol APIs require FFI.
#![expect(unsafe_code)]

use anyhow::Context;
use std::ffi::c_void;
use std::io::Write;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::ptr::null;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
use windows_sys::Win32::Foundation::ERROR_NO_MORE_ITEMS;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::Security::AdjustTokenPrivileges;
use windows_sys::Win32::Security::LUID_AND_ATTRIBUTES;
use windows_sys::Win32::Security::LookupPrivilegeValueW;
use windows_sys::Win32::Security::SE_DEBUG_NAME;
use windows_sys::Win32::Security::SE_PRIVILEGE_ENABLED;
use windows_sys::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::Diagnostics::Debug::AddrModeFlat;
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_FULL_AMD64;
use windows_sys::Win32::System::Diagnostics::Debug::CloseThreadWaitChainSession;
use windows_sys::Win32::System::Diagnostics::Debug::GetThreadWaitChain;
use windows_sys::Win32::System::Diagnostics::Debug::IMAGEHLP_MODULE64;
use windows_sys::Win32::System::Diagnostics::Debug::OpenThreadWaitChainSession;
use windows_sys::Win32::System::Diagnostics::Debug::STACKFRAME64;
use windows_sys::Win32::System::Diagnostics::Debug::SYMOPT_DEFERRED_LOADS;
use windows_sys::Win32::System::Diagnostics::Debug::SYMOPT_NO_PROMPTS;
use windows_sys::Win32::System::Diagnostics::Debug::StackWalk64;
use windows_sys::Win32::System::Diagnostics::Debug::SymCleanup;
use windows_sys::Win32::System::Diagnostics::Debug::SymFunctionTableAccess64;
use windows_sys::Win32::System::Diagnostics::Debug::SymGetModuleBase64;
use windows_sys::Win32::System::Diagnostics::Debug::SymGetModuleInfo64;
use windows_sys::Win32::System::Diagnostics::Debug::SymInitialize;
use windows_sys::Win32::System::Diagnostics::Debug::SymSetOptions;
use windows_sys::Win32::System::Diagnostics::Debug::WAITCHAIN_NODE_INFO;
use windows_sys::Win32::System::Diagnostics::Debug::WCT_MAX_NODE_COUNT;
use windows_sys::Win32::System::Diagnostics::Debug::WCT_OUT_OF_PROC_COM_FLAG;
use windows_sys::Win32::System::Diagnostics::Debug::WCT_OUT_OF_PROC_CS_FLAG;
use windows_sys::Win32::System::Diagnostics::Debug::WCT_OUT_OF_PROC_FLAG;
use windows_sys::Win32::System::Diagnostics::Debug::WctThreadType;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::HPSS;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::HPSSWALK;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_CAPTURE_THREAD_CONTEXT;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_CAPTURE_THREADS;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_CAPTURE_VA_CLONE;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_QUERY_VA_CLONE_INFORMATION;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_THREAD_ENTRY;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_VA_CLONE_INFORMATION;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PSS_WALK_THREADS;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssCaptureSnapshot;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssFreeSnapshot;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssQuerySnapshot;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssWalkMarkerCreate;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssWalkMarkerFree;
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::PssWalkSnapshot;
use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
use windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32NextW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD;
use windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32First;
use windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32Next;
use windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_AMD64;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::OpenThread;
use windows_sys::Win32::System::Threading::PROCESS_CREATE_PROCESS;
use windows_sys::Win32::System::Threading::PROCESS_DUP_HANDLE;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
use windows_sys::Win32::System::Threading::PROCESS_VM_READ;
use windows_sys::Win32::System::Threading::THREAD_QUERY_INFORMATION;

struct WaitSession(*mut c_void);

impl Drop for WaitSession {
    fn drop(&mut self) {
        // SAFETY: The handle came from OpenThreadWaitChainSession.
        unsafe { CloseThreadWaitChainSession(self.0) };
    }
}

struct Snapshot {
    process: HANDLE,
    handle: HPSS,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: This snapshot belongs to the still-open process handle.
        let status = unsafe { PssFreeSnapshot(self.process, self.handle) };
        if status != ERROR_SUCCESS {
            eprintln!("failed to free WMI process snapshot: {status}");
        }
    }
}

struct WalkMarker(HPSSWALK);

impl Drop for WalkMarker {
    fn drop(&mut self) {
        // SAFETY: This marker came from PssWalkMarkerCreate.
        let status = unsafe { PssWalkMarkerFree(self.0) };
        if status != ERROR_SUCCESS {
            eprintln!("failed to free WMI snapshot walk marker: {status}");
        }
    }
}

struct Symbols(HANDLE);

impl Drop for Symbols {
    fn drop(&mut self) {
        // SAFETY: This process handle was successfully passed to SymInitialize.
        if unsafe { SymCleanup(self.0) } == 0 {
            eprintln!(
                "failed to clean up WMI symbols: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn owned_handle(handle: HANDLE) -> std::io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: The successful Win32 call transferred ownership of this handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn enable_debug_privilege() -> anyhow::Result<()> {
    let mut token = null_mut();
    // SAFETY: token is a valid output pointer; the current process is live.
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };
    anyhow::ensure!(
        opened != 0,
        "opening process token: {}",
        std::io::Error::last_os_error()
    );
    let token = owned_handle(token)?;
    let mut luid = LUID::default();
    // SAFETY: SE_DEBUG_NAME and luid are valid input and output pointers.
    let found = unsafe { LookupPrivilegeValueW(null(), SE_DEBUG_NAME, &mut luid) };
    anyhow::ensure!(
        found != 0,
        "looking up debug privilege: {}",
        std::io::Error::last_os_error()
    );
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: The token is valid, and the privilege structure is initialized.
    let status = unsafe {
        windows_sys::Win32::Foundation::SetLastError(ERROR_SUCCESS);
        AdjustTokenPrivileges(
            token.as_raw_handle(),
            0,
            &privileges,
            0,
            null_mut(),
            null_mut(),
        )
    };
    // SAFETY: AdjustTokenPrivileges reports unavailable privileges through the last error.
    let error = unsafe { GetLastError() };
    anyhow::ensure!(
        status != 0 && error == ERROR_SUCCESS,
        "enabling debug privilege: {error}"
    );
    Ok(())
}

fn provider_hosts(snapshot: HANDLE) -> anyhow::Result<Vec<u32>> {
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: The snapshot and initialized entry are valid.
    let found = unsafe { Process32FirstW(snapshot, &mut entry) };
    anyhow::ensure!(
        found != 0,
        "enumerating guest processes: {}",
        std::io::Error::last_os_error()
    );
    let mut pids = Vec::new();
    loop {
        let name_end = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..name_end])
            .eq_ignore_ascii_case("WmiPrvSE.exe")
        {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: The snapshot and entry remain valid across iterations.
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            // SAFETY: Process32NextW just failed and set the thread's last error.
            let error = unsafe { GetLastError() };
            anyhow::ensure!(
                error == ERROR_NO_MORE_FILES,
                "enumerating processes: {error}"
            );
            break;
        }
    }
    anyhow::ensure!(!pids.is_empty(), "no guest WMI provider host was running");
    Ok(pids)
}

fn wait_chain(
    out: &mut impl Write,
    session: &WaitSession,
    pid: u32,
    tid: u32,
) -> anyhow::Result<()> {
    let mut count = WCT_MAX_NODE_COUNT;
    let mut nodes = [WAITCHAIN_NODE_INFO::default(); WCT_MAX_NODE_COUNT as usize];
    let mut is_cycle = 0;
    // SAFETY: The session, node array, and output pointers remain valid for this synchronous call.
    if unsafe {
        GetThreadWaitChain(
            session.0,
            0,
            WCT_OUT_OF_PROC_FLAG | WCT_OUT_OF_PROC_COM_FLAG | WCT_OUT_OF_PROC_CS_FLAG,
            tid,
            &mut count,
            nodes.as_mut_ptr(),
            &mut is_cycle,
        )
    } == 0
    {
        writeln!(
            out,
            "process={pid} thread={tid} wct_error={}",
            std::io::Error::last_os_error()
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "process={pid} thread={tid} nodes={count} deadlock={is_cycle}"
    )?;
    for (index, node) in nodes[..count as usize].iter().enumerate() {
        if node.ObjectType == WctThreadType {
            // SAFETY: The WctThreadType discriminator selects the ThreadObject union member.
            let thread = unsafe { node.Anonymous.ThreadObject };
            writeln!(
                out,
                "  node={index} type={} status={} owner_pid={} owner_tid={} wait_ms={}",
                node.ObjectType,
                node.ObjectStatus,
                thread.ProcessId,
                thread.ThreadId,
                thread.WaitTime
            )?;
        } else {
            writeln!(
                out,
                "  node={index} type={} status={}",
                node.ObjectType, node.ObjectStatus
            )?;
        }
    }
    Ok(())
}

fn module_name(module: &IMAGEHLP_MODULE64) -> String {
    let end = module
        .ModuleName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(module.ModuleName.len());
    String::from_utf8_lossy(
        &module.ModuleName[..end]
            .iter()
            .map(|&c| c as u8)
            .collect::<Vec<_>>(),
    )
    .into_owned()
}

fn stack(
    out: &mut impl Write,
    clone: HANDLE,
    pid: u32,
    entry: &PSS_THREAD_ENTRY,
) -> anyhow::Result<()> {
    if entry.ContextRecord.is_null()
        || usize::from(entry.SizeOfContextRecord) < size_of::<CONTEXT>()
    {
        writeln!(
            out,
            "process={pid} thread={} context_missing size={}",
            entry.ThreadId, entry.SizeOfContextRecord
        )?;
        return Ok(());
    }
    // SAFETY: PssWalkSnapshot guarantees this context pointer is valid until the marker is freed.
    let mut context = unsafe { *entry.ContextRecord };
    // SAFETY: The original thread is only queried; stack bytes are read from the VA clone.
    let thread =
        match owned_handle(unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, entry.ThreadId) }) {
            Ok(thread) => thread,
            Err(error) => {
                writeln!(
                    out,
                    "process={pid} thread={} stack_open_error={error}",
                    entry.ThreadId
                )?;
                return Ok(());
            }
        };
    let mut frame = STACKFRAME64::default();
    frame.AddrPC.Offset = context.Rip;
    frame.AddrPC.Mode = AddrModeFlat;
    frame.AddrFrame.Offset = context.Rsp;
    frame.AddrFrame.Mode = AddrModeFlat;
    frame.AddrStack.Offset = context.Rsp;
    frame.AddrStack.Mode = AddrModeFlat;
    writeln!(
        out,
        "process={pid} thread={} snapshot_stack",
        entry.ThreadId
    )?;
    let mut unchanged = 0;
    for depth in 0..20 {
        let pc = frame.AddrPC.Offset;
        if pc == 0 {
            break;
        }
        let mut module = IMAGEHLP_MODULE64 {
            SizeOfStruct: size_of::<IMAGEHLP_MODULE64>() as u32,
            ..Default::default()
        };
        // SAFETY: DbgHelp has been initialized for the VA clone and module is a valid output.
        if unsafe { SymGetModuleInfo64(clone, pc, &mut module) } != 0 {
            if let Some(rva) = pc.checked_sub(module.BaseOfImage) {
                writeln!(
                    out,
                    "  stack={depth} module={} rva=0x{rva:x}",
                    module_name(&module)
                )?;
            } else {
                writeln!(out, "  stack={depth} module_base_invalid")?;
            }
        } else {
            writeln!(out, "  stack={depth} module_unknown")?;
        }
        // SAFETY: The captured CONTEXT and STACKFRAME64 describe the clone, not a live thread.
        if unsafe {
            StackWalk64(
                u32::from(IMAGE_FILE_MACHINE_AMD64),
                clone,
                thread.as_raw_handle(),
                &mut frame,
                std::ptr::from_mut(&mut context).cast(),
                None,
                Some(SymFunctionTableAccess64),
                Some(SymGetModuleBase64),
                None,
            )
        } == 0
        {
            // SAFETY: StackWalk64 just returned false and set the thread's last error.
            let error = unsafe { GetLastError() };
            if error != ERROR_SUCCESS {
                writeln!(out, "  stack_walk_error={error}")?;
            }
            break;
        }
        if frame.AddrPC.Offset == pc {
            unchanged += 1;
            if unchanged == 2 {
                break;
            }
        } else {
            unchanged = 0;
        }
    }
    Ok(())
}

fn snapshot_stacks(out: &mut impl Write, pid: u32) -> anyhow::Result<usize> {
    // SAFETY: The process ID came from a ToolHelp snapshot and OpenProcess returns an owned handle.
    let process = owned_handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION
                | PROCESS_VM_READ
                | PROCESS_CREATE_PROCESS
                | PROCESS_DUP_HANDLE,
            0,
            pid,
        )
    })
    .with_context(|| format!("opening guest WMI provider host {pid}"))?;
    let mut handle = null_mut();
    // SAFETY: The process is open with the rights required to clone its VA and contexts.
    let status = unsafe {
        PssCaptureSnapshot(
            process.as_raw_handle(),
            PSS_CAPTURE_VA_CLONE | PSS_CAPTURE_THREADS | PSS_CAPTURE_THREAD_CONTEXT,
            CONTEXT_FULL_AMD64,
            &mut handle,
        )
    };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "capturing WMI provider host {pid}: {status}"
    );
    let snapshot = Snapshot {
        process: process.as_raw_handle(),
        handle,
    };
    let mut clone = PSS_VA_CLONE_INFORMATION::default();
    // SAFETY: The snapshot is live and clone points to a properly sized output structure.
    let status = unsafe {
        PssQuerySnapshot(
            snapshot.handle,
            PSS_QUERY_VA_CLONE_INFORMATION,
            std::ptr::from_mut(&mut clone).cast(),
            size_of::<PSS_VA_CLONE_INFORMATION>() as u32,
        )
    };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "opening WMI VA clone {pid}: {status}"
    );
    // SAFETY: This configures DbgHelp in this dedicated diagnostic process.
    unsafe { SymSetOptions(SYMOPT_DEFERRED_LOADS | SYMOPT_NO_PROMPTS) };
    // SAFETY: The VA clone is live, and the symbol path is empty (no symbol-server traffic).
    let initialized = unsafe { SymInitialize(clone.VaCloneHandle, c"".as_ptr().cast(), 1) };
    anyhow::ensure!(
        initialized != 0,
        "initializing WMI clone symbols: {}",
        std::io::Error::last_os_error()
    );
    let _symbols = Symbols(clone.VaCloneHandle);
    let mut marker = null_mut();
    // SAFETY: The walk marker receives a valid handle from PssWalkMarkerCreate.
    let status = unsafe { PssWalkMarkerCreate(null(), &mut marker) };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "creating WMI thread walk marker: {status}"
    );
    let marker = WalkMarker(marker);
    let mut captured = 0;
    loop {
        let mut entry = PSS_THREAD_ENTRY::default();
        // SAFETY: The marker, snapshot, and thread entry remain valid until the walk completes.
        let status = unsafe {
            PssWalkSnapshot(
                snapshot.handle,
                PSS_WALK_THREADS,
                marker.0,
                std::ptr::from_mut(&mut entry).cast(),
                size_of::<PSS_THREAD_ENTRY>() as u32,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        anyhow::ensure!(
            status == ERROR_SUCCESS,
            "walking WMI provider threads: {status}"
        );
        stack(out, clone.VaCloneHandle, pid, &entry)?;
        captured += 1;
    }
    Ok(captured)
}

pub fn capture(mut out: impl Write) -> anyhow::Result<()> {
    enable_debug_privilege()?;
    // SAFETY: The synchronous WCT session needs no callback.
    let session = WaitSession(unsafe { OpenThreadWaitChainSession(0, None) });
    anyhow::ensure!(
        !session.0.is_null(),
        "opening wait-chain session: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: ToolHelp returns an owned process/thread snapshot handle.
    let processes = owned_handle(unsafe {
        CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS | TH32CS_SNAPTHREAD, 0)
    })?;
    let pids = provider_hosts(processes.as_raw_handle())?;
    writeln!(out, "matched={}", pids.len())?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: The process/thread snapshot and output entry remain valid.
    let found = unsafe { Thread32First(processes.as_raw_handle(), &mut entry) };
    anyhow::ensure!(
        found != 0,
        "enumerating guest threads: {}",
        std::io::Error::last_os_error()
    );
    loop {
        if pids.contains(&entry.th32OwnerProcessID) {
            wait_chain(
                &mut out,
                &session,
                entry.th32OwnerProcessID,
                entry.th32ThreadID,
            )?;
        }
        // SAFETY: The snapshot and entry remain valid across iterations.
        if unsafe { Thread32Next(processes.as_raw_handle(), &mut entry) } == 0 {
            // SAFETY: Thread32Next just failed and set the thread's last error.
            let error = unsafe { GetLastError() };
            anyhow::ensure!(
                error == ERROR_NO_MORE_FILES,
                "enumerating guest threads: {error}"
            );
            break;
        }
    }
    let mut captured = 0;
    for pid in pids {
        match snapshot_stacks(&mut out, pid) {
            Ok(count) => captured += count,
            Err(error) => writeln!(out, "process={pid} snapshot_error={error:#}")?,
        }
    }
    anyhow::ensure!(
        captured != 0,
        "no WMI provider-host thread context could be captured"
    );
    Ok(())
}
