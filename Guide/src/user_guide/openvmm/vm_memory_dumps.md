# VM Memory Dumps

OpenVMM can dump a running VM's processor state and memory to a `.vmrs`
file compatible with WinDbg. This is useful for offline debugging of
guest crashes without needing a live debugger attached.

## Overview

The `dump-state` command captures:

- **VP registers** — general-purpose, control, segment, table, debug,
  and FP/XSAVE registers for every virtual processor
- **Guest RAM** — the full contents of guest physical memory, streamed
  in 1 MiB blocks

The output is a `.vmrs` file that can be opened in WinDbg via the
`VmSavedStateDumpProvider.dll` from the Windows SDK.

## Usage

While the VM is running, open the interactive console and run:

```text
dump-state path/to/dump.vmrs
```

The shorter alias `dump` also works:

```text
dump path/to/dump.vmrs
```

OpenVMM pauses the VM, collects state, streams guest memory to disk,
and then resumes the VM. If the VM was already paused, it remains
paused after the dump completes.

The file is written atomically — a temporary file is created first
and renamed into place on success, so readers never see a
partially-written dump.

## Opening in WinDbg

1. Install the [Windows SDK](https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/)
   (the "Debugging Tools for Windows" component includes
   `VmSavedStateDumpProvider.dll`).

2. Open the `.vmrs` file in WinDbg:

   ```text
   windbg -z path/to/dump.vmrs
   ```

WinDbg will load the saved processor state and allow you to inspect
registers, stack traces, and memory contents as if attached to a live
VM.

```admonish note
Unlike snapshots, dump files do **not** capture device state and cannot
be used to restore a VM. They are strictly for offline debugging.
```
