# Interactive Console

By default, OpenVMM will connect the guests's COM1 serial port to the current
terminal session, forwarding all keystrokes directly to the VM.

To enter OpenVMM's interactive command mode, launch OpenVMM, and type `ctrl-q`.

You can then type the following commands (followed by return):

```admonish danger title="Disclaimer"
The following list is not exhaustive and may be out of date.

The most up to date reference is always the code itself. For a full list of
commands, please invoke the `help` command.
```

* `q` / `quit`: quit the program.
* `I` / `input-mode`: switch to input mode (Ctrl-Q returns to command mode).
* `i <LINE>` / `input <LINE>`: write input to the VM console.
* `R` / `restart`: restart the VM worker (experimental).
* `n` / `nmi`: inject an NMI.
* `p` / `pause`: pause the VM.
* `r` / `resume`: resume the VM.
* `d` / `add-disk [-ro] [--dvd] [--path <INDEX>] [--target <INDEX>] [--lun <INDEX>] [--ram <SIZE>] <PATH>`: hot add a disk to the VTL0 guest.
* `D` / `rm-disk --target <INDEX> --path <INDEX> --lun <INDEX>`: hot remove a disk from the VTL0 guest.
* `x` / `inspect [-r] [-l <LIMIT>] [-v] [path] [-u <VALUE>]`: inspect runtime state using the `Inspect` trait infrastructure.
* `V` / `restart-vnc`: restart the VNC worker.
* `v` / `hvsock [--term <PATH>] <PORT>`: start an hvsocket terminal window.
* `snap` / `save-snapshot <DIR>`: save a snapshot to a directory.
* `psr` / `pulse-save-restore`: do a pulsed save-restore cycle.
* `reset`: reset the VM.
* `shutdown [-r] [-h] [-f]`: send a shutdown/reboot/hibernate request to the VM.
* `ch` / `clear-halt`: clear the current halt condition.
* `read-memory <GPA> <SIZE> [-f <FILE>]`: read guest memory.
* `write-memory <GPA> [HEX] [-f <FILE>]`: write guest memory.
* `panic`: inject an artificial panic into OpenVMM.
* `help`: show full command list.
