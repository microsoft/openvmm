# gRPC / ttrpc
To enable gRPC or ttrpc management interfaces, specify the respective cli flags,
`--grpc <SOCKETPATH>` and `--trpc <SOCKETPATH>`. This runs OpenVMM as a gRPC or
ttrpc server.

Here is a list of supported RPCs:

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date. The most up to
date reference is always [the code].
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

[the code]: https://openvmm.dev/rustdoc/linux/hvlite_ttrpc_vmservice/index.html
