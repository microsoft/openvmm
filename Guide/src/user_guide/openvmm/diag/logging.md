# Logging

## Configuring the logging messages to emit

To configure logging, use the `HVLITE_LOG` environment variable. For example:

Enables debug events from all modules:

```
set HVLITE_LOG=debug
```

Enables trace events from the `mesh` crate and info events from everything else:

```
set HVLITE_LOG=info,mesh=trace
```

This is backed by the
[`EnvFilter`](https://docs.rs/tracing-subscriber/0.2.17/tracing_subscriber/struct.EnvFilter.html)
type; see the associated documentation for more details.

## Capturing the ETW traces on the host

On Windows, OpenVMM also logs to ETW, via the Microsoft.HvLite provider.

To capture the trace first need to start the session:
```cmd
logman.exe start trace <SessionName> -ow -o FileName0.etl -p "{22bc55fe-2116-5adc-12fb-3fadfd7e360c}" 0xffffffffffffffff 0xff -nb 16 16 -bs 16 -mode 0x2 -ets
```
 > For OpenHCL traces, use `{AA5DE534-D149-487A-9053-05972BA20A7C}` as the provider GUID.

To flush:
```cmd
logman.exe update <SessionName> -ets -fd
```
To stop:
```cmd
logman.exe stop <SessionName> -ets
```
To decode as CSV:
```cmd
tracerpt.exe <FileName0>.etl -y -of csv -o <FileName1>.csv -summary <FileName2>.summary
```
