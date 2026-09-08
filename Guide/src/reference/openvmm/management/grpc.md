# gRPC / ttrpc

To enable a gRPC or ttrpc management interface, pass `--rpc`. This spawns an
OpenVMM process acting as an RPC server on the given Unix socket, or a Windows
named pipe:

```bash
--rpc path=<PATH>[,transport=<TRANSPORT>][,allow-sid=<SID>]
```

`transport` selects which wire protocol the server accepts:

* `auto` (default) — auto-detect ttrpc vs. gRPC per connection
* `ttrpc` — accept ttrpc clients only
* `grpc` — accept gRPC clients only

For example, to accept ttrpc clients only:

```bash
--rpc path=/path/to/openvmm.sock,transport=ttrpc
```

On Windows, a path beginning with `\\.\pipe\` selects a named-pipe endpoint:

```bash
--rpc path=\\.\pipe\openvmm
```

Use `allow-sid=<SID>` to restrict a Windows named pipe to the specified user or
group and OpenVMM's own process user. If omitted, Windows default pipe security
applies. This option is supported only for Windows named pipes.

Here is a list of supported RPCs:

```admonish note title="API reference"
The API continues to evolve, and compatibility between releases is not
guaranteed. The [`vmservice.proto`] file is the authoritative API definition.
The list below summarizes the available RPCs; some definitions may be added
before their implementation is connected end to end.
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
