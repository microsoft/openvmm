# Kernel Debugging (KDNET)

Kernel Debugging is available for Windows guests via KDNET over VMBus.

## Enabling and Starting the Debugger

Set up KDNET on the guest and start the debugger as described on
[Set up KDNET network kernel debugging manually | Microsoft Learn](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/setting-up-a-network-debugging-connection).
Setting `busparams` is not necessary.

```admonish warning
If you're using a `memdiff:` disk (the default in most examples), guest state
is not persisted between runs. You'll need to configure KDNET in the guest
each time you launch — or configure it once on the base disk image before
wrapping it with `memdiff:`.
```

## With OpenVMM and WHP as Host

Launch the VM with UEFI and networking enabled. KDNET requires `--net consomme`
for the network transport:

```bash
# Without OpenHCL
cargo run -- --uefi --hv --net consomme --disk memdiff:file:path/to/windows.vhdx

# With OpenHCL
cargo run -- --uefi --hv --vtl2 --net consomme --igvm path/to/openhcl.igvm --disk memdiff:file:path/to/windows.vhdx
```

### Known Issues

- **Networking backend:** KDNET currently requires `--net consomme`. The
  `consomme` backend creates a new network adapter in the guest on each
  OpenVMM restart — this is harmless and can be ignored. Using
  `--net vmnic:<switch id>` connects but hangs immediately due to an
  undiagnosed vmbusproxy issue.
- **Debugger reconnection:** If you quit OpenVMM without shutting down the
  guest first, the same WinDbg instance cannot reconnect on next boot.
  Close and relaunch WinDbg to reconnect.
- **Synic warnings with OpenHCL:** When launching an OpenHCL VM with KDNET,
  you may see a stream of `failed to signal synic` errors from
  `virt_whp::synic` for several seconds. These do not affect VM functionality
  and can be ignored.

## Debugging workflow

Once KDNET is connected:

1. In WinDbg, go to **File → Kernel Debug → Net** and enter the key and port from the guest setup.
2. Start the VM. WinDbg should connect during Windows boot.
3. To break into the debugger: **Debug → Break** (or Ctrl+Break).

```admonish note
KDNET debugs the VTL0 (guest) Windows kernel. For debugging the VTL2
(OpenHCL) side, see [Debugging OpenHCL](../openhcl/debugging.md) which covers
serial logs, crash dumps, and `ohcldiag-dev`.
```
