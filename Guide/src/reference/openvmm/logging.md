# OpenVMM Logging

## Configuring OpenVMM logging messages to emit

To configure logging, use the `OPENVMM_LOG` environment variable. For example:

Enables debug events from all modules:

```cmd
set OPENVMM_LOG=debug
```

Enables trace events from the `mesh` crate and info events from everything else:

```cmd
set OPENVMM_LOG=info,mesh=trace
```

This is backed by the
[`EnvFilter`](https://docs.rs/tracing-subscriber/0.2.17/tracing_subscriber/struct.EnvFilter.html)
type; see the associated documentation for more details.

## Configuring OpenHCL Trace Logging

If OpenHCL is used, it also supports an [`EnvFilter`](https://docs.rs/tracing-subscriber/0.2.17/tracing_subscriber/struct.EnvFilter.html) style trace logging options that can be configured using the `OPENVMM_LOG=` command line variable passed during OpenHCL startup with `-c OPENVMM_LOG=`. The `-c` argument in OpenVMM passes a string of command line arguments to OpenHCL initialization.

The environment variable configuration and style of `OPENVMM_LOG` for OpenHCL is the same for configuring tracing for OpenVMM.

OpenHCL tracing can also be configured and dumped at runtime with `ohcldiag-dev`. See: [OpenHCL Diagnostics](../openhcl/diag/ohcldiag_dev.md)

To retrieve OpenHCL log output at runtime, an output console or file must attach to the OpenHCL logging COM port. By default, OpenHCL outputs to `COM3`.

To open a new terminal window with global OpenHCL debug level tracing enabled:

```cmd
openvmm.exe -c "OPENVMM_LOG=debug" --com3 "term,name=VTL2 OpenHCL" [...]
```

Configure log levels of only a given module name:

```cmd
openvmm.exe -c "OPENVMM_LOG=mesh=trace" --com3 "term,name=VTL2 OpenHCL" [...]
```

Multiple modules can be specified by separating them with a comma:

```cmd
openvmm.exe -c "OPENVMM_LOG=mesh=trace,nvme_driver=trace" --com3 "term,name=VTL2 OpenHCL" [...]
```

```admonish tip
For more configuration examples of serial ports and the OpenVMM CLI, see the [Running OpenHCL Guide](../../../user_guide/openhcl/run/openvmm.md) and CLI `--help` output.
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
