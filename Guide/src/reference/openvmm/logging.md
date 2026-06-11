# OpenVMM Logging

## Configuring OpenVMM logging messages to emit

To configure logging, set the `OPENVMM_LOG` environment variable. The default
level is `info`. For example:

- `OPENVMM_LOG=debug` — enable debug events from all modules
- `OPENVMM_LOG=info,mesh=trace` — enable trace events from the `mesh` crate
  and info events from everything else

This is backed by the
[`EnvFilter`](https://docs.rs/tracing-subscriber/0.2.17/tracing_subscriber/struct.EnvFilter.html)
type; see the associated documentation for more details.

### Span events

By default, OpenVMM does not log span enter/exit events. To enable them, set
`OPENVMM_LOG_SPANS=1`.

### Rate limiting

Trace events that can be triggered repeatedly by guest interactions are
rate-limited by default. To disable rate limiting (useful for debugging), set
`OPENVMM_DISABLE_TRACING_RATELIMITS=1`.

## Configuring OpenHCL Trace Logging

OpenHCL also supports `EnvFilter`-style trace logging, configured via the
`-c OPENVMM_LOG=` command line argument. The `-c` flag passes arguments to
OpenHCL initialization. The filter syntax is the same as for OpenVMM.

OpenHCL tracing can also be configured and dumped at runtime with
`ohcldiag-dev`. See: [OpenHCL Diagnostics](../openhcl/diag/ohcldiag_dev.md)

To retrieve OpenHCL log output at runtime, an output console or file must
attach to the OpenHCL logging COM port. By default, OpenHCL outputs to `COM3`.

To open a new terminal window with global OpenHCL debug level tracing enabled:

```shell
openvmm -c "OPENVMM_LOG=debug" --com3 "term,name=VTL2 OpenHCL" [...]
```

Configure log levels of only a given module name:

```shell
openvmm -c "OPENVMM_LOG=mesh=trace" --com3 "term,name=VTL2 OpenHCL" [...]
```

Multiple modules can be specified by separating them with a comma:

```shell
openvmm -c "OPENVMM_LOG=mesh=trace,nvme_driver=trace" \
    --com3 "term,name=VTL2 OpenHCL" [...]
```

```admonish tip
For more configuration examples of serial ports and the OpenVMM CLI, see the
[Running OpenHCL Guide](../../../user_guide/openhcl/run/openvmm.md) and CLI
`--help` output.
```

## Capturing the ETW traces on the host

On Windows, OpenVMM also logs to ETW, via the Microsoft.HvLite provider.

To capture the trace, start a session:

```powershell
logman start trace <SESSION_NAME> -ow -o trace.etl `
    -p "{22bc55fe-2116-5adc-12fb-3fadfd7e360c}" 0xffffffffffffffff 0xff `
    -nb 16 16 -bs 16 -mode 0x2 -ets
```

```admonish note
For OpenHCL traces, use `{AA5DE534-D149-487A-9053-05972BA20A7C}` as the
provider GUID.
```

To flush:

```powershell
logman update <SESSION_NAME> -ets -fd
```

To stop:

```powershell
logman stop <SESSION_NAME> -ets
```

To decode as CSV:

```powershell
tracerpt trace.etl -y -of csv -o trace.csv -summary trace-summary.txt
```
