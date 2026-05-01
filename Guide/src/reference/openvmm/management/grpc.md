# gRPC / ttrpc

To enable gRPC or ttrpc management interfaces, use `openvmm serve` with
`--transport grpc` or `--transport ttrpc` and a Unix socket path. This will
spawn an OpenVMM process acting as a gRPC or ttrpc server.

```bash
openvmm serve --transport grpc /tmp/openvmm-rpc.sock
openvmm serve --transport ttrpc /tmp/openvmm-rpc.sock
```

Here is a list of supported RPCs:

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date. The most up to
date reference is the [`vmservice.proto`] file.

Moreover, many APIs defined in the `.proto` file may not be fully wired up yet.

In other words: This API is _very_ WIP, and user discretion is advised.
```

* CreateVM
* TeardownVM
* PauseVM
* ResumeVM
* WaitVM
* CapabilitiesVM
* PropertiesVM
* ModifyResource
* Quit

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
