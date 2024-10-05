# Troubleshooting

## Vtl2/Vtl0 failed to start

  - VTL2/VTL0 fails to boot is when either VTL2 or VTL0 has crashed. When the crash happens, they will emit an event to the Hyper-V worker channel. 
  - So first thing is to check 
    `Hyper-V worker  events` at `Applications and Services Logs -> Microsoft -> Windows -> Hyper-V-Worker-Admin`
  - OR Some queries you can use to get Hyper-V-Worker logs 
    - Display the `{n}` most recent events -  `wevtutil qe Microsoft-Windows-Hyper-V-Worker-Admin /c:{n} /rd:true /f:text` 
    - Export events to file - `wevtutil epl Microsoft-Windows-Hyper-V-Worker-Admin C:\vtl2_0_crash.evtx`
  - You can also get OpenHCL traces in Windows event log by following instructions [here](./diag.md).
  - Some of the event ids we can look for in the logs are - 3131, 3132, 3133, 3134, 3135


If running into issues with building/running OpenHCL or connecting to OpenHCL
via `ohcldiag-dev.exe`, please try these steps to resolve your issue.

  - Try rebasing your branch off of `main`. OpenHCL is very dynamic, and your
    branch might simply be out of date with some dependencies.

  - If you're still having issues, try building and booting off of `main` to
    ensure `main` is in a working state.
