# hypestv

`hypestv` is an interactive command-line interface for Hyper-V VMs, designed for
making OpenHCL developers' lives easier.

Similar to [`ohcldiag-dev`](./ohcldiag_dev.md), it can interact with the
OpenHCL paravisor running inside a Hyper-V VM. But unlike `ohcldiag-dev`, it
sports an interactive terminal interface (with history and tab completion), and
it is specifically designed to interact with Hyper-VMs.

In many ways, it is similar to the OpenVMM interactive console. In time, it may
end up sharing code and capabilities with it and with `ohcldiag-dev`, but it
will always be a Hyper-V specific tool.

Currently, it can:

* Change VM state (starting/stopping/resetting)
* Enable serial port output
* Inspect paravisor state

In the future, it might be able to:

* Enable paravisor and Hyper-V log output
* Enable serial port input
* Capture serial port output to another terminal window or file
* Inspect host state
* Persistence workspaces (save/restore configured serial ports and logs)

## Example session

`hypestv` launches into a detached mode, unless you specify a VM name on the
command line. To select a VM to work on, the VM named `tdxvm` in this example,
use the `select` command. If successful, you will now see the name and VM state
in the prompt:

```
> select tdxvm
tdxvm [off]>
```

After this, all commands will implicitly operate on `tdxvm`. Use `select` again
to work on another VM.

To enable serial port output, use the `serial` command. This can be used at any
time, even while the VM is not running. E.g., to enable serial port output for
COM2:

```
tdxvm [off]> serial 2 output
```

Start a VM with `start`. This is an asynchronous command: you can continue to
type other commands at the prompt while the VM starts. You should see an output
message when the VM finishes starting, as well as output about any configured
serial ports connecting.

Note that, due to limitations of the `rustyline` crate, the displayed VM state
on the prompt may not be accurate until you type another command or press Enter.

```
tdxvm [off]> start
serial port 2 connected
VM started
tdxvm [off]>
tdxvm [running]>
```

At this point, the VM is running, including the paravisor (if one is
configured). As in the OpenVMM interactive console, you can inspect paravisor
state with the `inspect` or `x` command, passing `-p` to specify that you want
to inspect paravisor state:

```
tdxvm [running]> x -p
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

```
tdxvm [running]> kill
serial port 2 disconnected
VM killed
tdxvm [stopping]>
tdxvm [off]>
```
