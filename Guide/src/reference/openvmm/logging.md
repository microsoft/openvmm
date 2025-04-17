# OpenVMM Logging

## Configuring the logging messages to emit

To configure logging, use the `OPENVMM_LOG` environment variable. For example:

Enables debug events from all modules:

```
set OPENVMM_LOG=debug
```

Enables trace events from the `mesh` crate and info events from everything else:

```
set OPENVMM_LOG=info,mesh=trace
```

This is backed by the
[`EnvFilter`](https://docs.rs/tracing-subscriber/0.2.17/tracing_subscriber/struct.EnvFilter.html)
type; see the associated documentation for more details.

The environment variable configuration and style is the same for configuring tracing for OpenVMM as OpenHCL. The `OPENVMM_LOG`
environment variable can be supplied to OpenHCL as command line argument or can be configured at runtime with `ohcldiag-dev`. See: [OpenHCL Diagnostics](../openhcl/diag/ohcldiag_dev.md).

## Enabling trace logging for OpenHCL running under OpenVMM

OpenVMM can specify command line arguments to pass to OpenHCL during startup. These are specified through the openvmm binary command line argument `-c`.
To configure tracing verbosity for OpenHCL, the `OPENVMM_LOG` variable must be passed to OpenHCL through the `-c` argument.

To open a new terminal window with global OpenHCL debug level tracing enabled:
```
openvmm.exe -c "OPENVMM_LOG=debug" --com3 "term,name=VTL2 OpenHCL" [...]
```

Configure log levels of only a given module name:
```
openvmm.exe -c "OPENVMM_LOG=mesh=trace" --com3 "term,name=VTL2 OpenHCL" [...]
```

Multiple modules can be specified by separating them with a comma:
```
openvmm.exe -c "OPENVMM_LOG=mesh=trace,nvme_driver=trace" --com3 "term,name=VTL2 OpenHCL" [...]
```

```admonish tip
To retrieve OpenHCL log output over serial at runtime, an output console or file can be specified. By default, this output port is `COM3`. 
For more configuration examples, see the [Running OpenHCL Guide](../../user_guide/openhcl/run/openvmm.md).
```

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
