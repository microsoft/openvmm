# Kernel Debugging (KDNET)

Kernel Debugging is available for Windows guests via KDNET over VMBus.

## Enabling and Starting the Debugger

Set up KDNET on the guest and start the debugger as described on
[Set up KDNET network kernel debugging manually | Microsoft Learn](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/setting-up-a-network-debugging-connection).
Setting `busparams` is not necessary.

## With OpenVMM and WHP as Host

Set up the VM for UEFI and VMBus depending on your use case and pass the
additional flag `--net consomme`:

- **Without OpenHCL:** Pass the `--uefi` flags when starting OpenVMM.
- **With OpenHCL:** Ensure "UEFI Boot" and "VTL2 VMBus Support" are active

### Known Issues with KDNET on WHP

- KDNET currently only works with the `consomme` networking option in OpenVMM,
  however `consomme` will create a new network adapter in the guest every time
  OpenVMM is restarted. This can be safely ignored.
  - KDNET will also connect with `--net vmnic:<ethernet switch id>`, but hangs
      immediately after due to a yet undiagnosed bug in vmbusproxy.
- Quitting OpenVMM without shutting down the VM first will prevent the same
  debugger instance from reconnecting to the guest on next boot. Relauch the
  debugger to reconnect.
- When launching an OpenHCL VM with KDNET, `virt_whp::synic` will report a
  constant stream of `failed to signal synic` errors for several seconds. These
  don't appear to affect the VM's functionality and can be ignored.
