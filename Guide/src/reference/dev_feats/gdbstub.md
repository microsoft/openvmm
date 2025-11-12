# Hardware Debugging (gdbstub)

Think EXDI from Hyper-V, except instead of using the EXDI interface, we use the
[GDB Remote Serial Protocol](https://sourceware.org/gdb/onlinedocs/gdb/Remote-Protocol.html)
(via the [`gdbstub`](https://github.com/daniel5151/gdbstub/) Rust library).

Hardware debugging has several benefits over using an in-guest / in-kernel debugger:

- Debugging early-boot scenarios (before UEFI / Windows / Linux debuggers are set up)
- Debugging low-level ISRs
- Non-intrusive debugging = easier to repro certain bugs
- Debugging SNP/TDX/VBS Confidential VMs

## Enabling the Debugger

### OpenVMM
1. Pass the `--gdb <port>` flag at startup to enable the
debug worker. e.g., `--gdb 9001`

To pause the VM until the debugger has been attached, pass `--paused` at startup.

### OpenHCL
1. Pass the `OPENHCL_GDBSTUB=1` `OPENHCL_GDBSTUB_PORT=<gdbstub port>` parameters to enable gdbstub. e.g., `Set-VmFirmwareParameters -Name UhVM -CommandLine OPENHCL_GDBSTUB=1 OPENHCL_GDBSTUB_PORT=5900`.
2. To expose a TCP port, run `ohcldiag-dev.exe <name> vsock-tcp-relay --allow-remote --reconnect <gdbstub port> <tcp port>`.

To pause VTL0 boot until desired, pass `OPENHCL_VTL0_STARTS_PAUSED=1` as a parameter. Then once the debugger is attached, you can start VTL0 with `ohcldiag-dev.exe <name> resume`.

## Connecting via GDB

The quickest way to get connected to a OpenVMM VM is via `gdb` directly.

Note that GDB does _not_ support debugging PDBs, so if you're trying to debug
Windows, you'll be limited to plain disassembly. See the [`Connecting via
WinDbg`](#connecting-via-windbg) section below if this is your use-case.

On the flipside, if you're trying to debug ELF images with DWARF debug info
(e.g., a vmlinux binary), then you'll likely want to use `gdb` directly, as it
will support source-level debugging with symbols, whereas WinDbg will not.

You can install `gdb` via your distro's package manager. e.g., on Ubuntu:

```bash
sudo apt install gdb
```

Once `gdb` is installed, run it, and enter the following `gdb` command (swapping
`9001` for whatever port you specified at the CLI)

```text
target remote :9001
```

If all goes well, you should get output similar to this:

```text
(gdb) target remote :9001
Remote debugging using :9001
warning: No executable has been specified and target does not support
determining executable automatically.  Try using the "file" command.
0xfffff8015c054c1f in ?? ()
(gdb)
```

At this point, you can try some basic GDB commands to make sure things are working.

e.g., start / interrupt the VM's execution using `cont` and `ctrl-c`

```text
(gdb) cont
Continuing.
^C                                          # <-- hit ctrl-c in the terminal
Thread 1 received signal SIGINT, Interrupt.
0xfffff8015c054c1f in ?? ()
(gdb)
```

e.g., inspecting register state

```text
(gdb) info registers
rax            0x0                 0
rbx            0x0                 0
rcx            0x40086             262278
rdx            0x0                 0
rsi            0xffff960d4eea5010  -116491073990640
rdi            0x0                 0
rbp            0x0                 0x0
rsp            0xfffff8015b3f5ec8  0xfffff8015b3f5ec8
r8             0x0                 0
r9             0xffffffff          4294967295
r10            0xfffff8015bfff1f0  -8790254554640
r11            0x0                 0
r12            0xffffffff          4294967295
...etc...
```

e.g., setting data breakpoints

```text
(gdb) awatch *0xfffff804683190e0
Hardware access (read/write) watchpoint 1: *0xfffff804683190e0
```

e.g., single stepping

```text
0xfffff8047a309686 in ?? ()
(gdb) si
0xfffff8047a309689 in ?? ()
```

You may find [this blog post](https://blog.mattjustice.com/2018/08/24/gdb-for-windbg-users/)
useful, as it includes a table of common `gdb` commands along with their WinDbg
counterparts.

## Connecting via WinDbg

WinDbg doesn't understand the GDB Remote Serial Protocol directly, but
thankfully, some smart folks over on the WinDbg team have developed a GDB Remote
Serial Protocol <-> WinDbg translation layer!

For more information, see
[Setting Up QEMU Kernel-Mode Debugging using EXDI](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/setting-up-qemu-kernel-mode-debugging-using-exdi)

Getting this working with OpenVMM or OpenHCL is as easy as following the guide,
except you'll need to [enable our debugger](#enabling-the-debugger) instead of
running QEMU.

It's easiest to connect through the GUI. The steps are relatively simple: Open Windbgx -> File -> Attach to kernel -> EXDI. On the form, fill out:
- Target Type: `QEMU`
- Target Architecture: `X64`
- Target OS: `Windows`
- Image Screening heuristic size: `0xFFFE - NT`
- Gdb server and port: `<server>:<port>` e.g., `127.0.0.1:1337` (use whatever port you set above)

### Known WinDbg Bugs
- Hardware breakpoints are issued with [`ba`](https://learn.microsoft.com/en-us/windows-hardware/drivers/debuggercmds/ba--break-on-access-). The `Access Size` parameter is incorrectly multiplied by 8 when sent to the stub. Consequently, it _must_ be set to 1.
- Unlike GDB, WinDbg doesn't implicitly set software breakpoints via our offered write_addrs implementation.
---


## Supported Features

At the time of writing (8/16/24) the debugger supports the following operations:

- read/write guest memory
- read guest registers \*
- start/interrupt execution
- watchpoints
- hardware breakpoints
- single stepping

## TODO Features

If you're looking for work, and want to improve the debugging experience for
everyone, consider implementing one or more of the following features:

- \* reading _all_ guest registers, including fpu, xmm, and various key msrs
- software breakpoints:
    - Intercept guest breakpoint exceptions into VTL2
- writing guest registers
- exposing the OpenVMM interactive console via the
  [`MonitorCmd`](https://docs.rs/gdbstub/latest/gdbstub/target/ext/monitor_cmd/trait.MonitorCmd.html)
  interface
    - Custom commands sent using `monitor` (gdb) / `.exdicmd` (WinDbg)
    - e.g., being able to invoke `x device/to/inspect` directly from the debugger
- [any other features supported by the `gdbstub` library](https://github.com/daniel5151/gdbstub#debugging-features)
