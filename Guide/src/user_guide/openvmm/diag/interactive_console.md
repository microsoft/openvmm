# Interactive Console

By default, OpenVMM will connect the guests's COM1 serial port to the current
terminal session, forwarding all keystrokes directly to the VM.

To enter OpenVMM's interactive command mode, type `ctrl-q`.

You can then type the following commands (followed by return):

> Note: The following list is not exhaustive and may be out of date. The most
> up to date reference is always the code itself. For a full list of commands,
> please invoke the `help` command.

* `q`: quit. Note--sometimes this does not work due to a bug in the virito serial teardown path. In this case, type Ctrl-C to exit after running `q`.
* `I`: re-enter interactive mode.
* `i<LINE>`: input `LINE` to the active serial console.
* `R`: restart worker (experimental)
* `n`: inject NMI
* `s`: print state
* `h`: print hv state
* `p`: pause
* `r`: resume
* `d [-ro] [-path <INDEX>] [-target <INDEX>] [-lun <INDEX>] [-ram <Size>] <PATH>`: hot add the disk at `<PATH>` to the VM. Requires `--hv`
* `x [-r] [path]`: inspect runtime state using the `Inspect` trait infrastructure
* `help`: help
