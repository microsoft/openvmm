# hypestv

`hypestv` is an interactive command-line interface for Hyper-V VMs, designed for
making OpenHCL developers' lives easier.

Similar to [`ohcldiag-dev`][], it can interact with the OpenHCL paravisor
running inside a Hyper-V VM. But unlike `ohcldiag-dev`, it sports an interactive
terminal interface (with history and tab completion), and it is specifically
designed to interact with Hyper-VMs.

[`ohcldiag-dev`]: ../../reference/openhcl/diag/ohcldiag_dev.md

In many ways, it is similar to the OpenVMM interactive console. In time, it may
end up sharing code and capabilities with it and with `ohcldiag-dev`, but it
will always be a Hyper-V specific tool.

Currently, it can:

* Change VM state (starting/stopping/resetting)
* Enable serial port output to standard output, or input/output to another
  terminal window
* Enable paravisor log output to standard output or another terminal window
* Inspect paravisor state
* Modify the paravisor (update the paravisor command line, reload the paravisor)

In the future, it might be able to:

* Enable Hyper-V log output
* Capture serial port output to a file
* Inspect host state
* Persistence workspaces (save/restore configured serial ports and logs)

## Example session

`hypestv` launches into a detached mode, unless you specify a VM name on the
command line. To select a VM to work on, the VM named `tdxvm` in this example,
use the `select` command. If successful, you will now see the name and VM state
in the prompt:

```text
> select tdxvm
tdxvm [off]>
```

After this, all commands will implicitly operate on `tdxvm`. Use `select` again
to work on another VM.

To enable serial port output, use the `serial` command. This can be used at any
time, even while the VM is not running. E.g., to open a separate window for
interactive use of COM1 and enable logging serial port output for COM2:

```text
tdxvm [off]> serial 1 term
tdxvm [off]> serial 2 log
```

You can also enable paravisor log output at any time:

```text
tdxvm [off]> paravisor kmsg log
```

Start a VM with `start`. This is an asynchronous command: you can continue to
type other commands at the prompt while the VM starts. You should see an output
message when the VM finishes starting, as well as output about any configured
serial ports connecting.

Note that, due to limitations of the `rustyline` crate, the displayed VM state
on the prompt may not be accurate until you type another command or press Enter.

```text
tdxvm [off]> start
com1 connected
com2 connected
VM started
tdxvm [off]>
tdxvm [running]>
```

At this point, the VM is running, including the paravisor (if one is
configured). As in the OpenVMM interactive console, you can inspect paravisor
state with the `inspect` or `x` command, but under the `paravisor`/`pv` command:

```text
tdxvm [running]> pv x
{
    build_info: _,
    control_state: "started",
    mesh: _,
    proc: _,
    trace: _,
    uhdiag: _,
    vm: _,
}
```

You can terminate the VM with `kill`. This will disconnect any connected serial
ports as well, but they will reconnect next time the VM starts. Killing a VM
does not detach/deselect it; subsequent commands will continue to operate on the
VM.

```text
tdxvm [running]> kill
com1 disconnected
com2 disconnected
VM killed
tdxvm [stopping]>
tdxvm [off]>
```
