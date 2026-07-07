# mesh

mesh is a channel-based communication framework for Rust. It provides typed
channels (`Sender<T>` / `Receiver<T>`) that work the same way whether the
two ends are in the same process or in different processes. Components that
communicate through mesh channels can be moved between processes without
changing their source code.

OpenVMM and OpenHCL use mesh as their primary communication mechanism. In
OpenHCL, the [paravisor, VM worker, diagnostics server, and device
workers](../../architecture/openhcl/processes.md) are all separate
processes connected by mesh channels.

**Source code:**
[support/mesh](https://github.com/microsoft/openvmm/tree/main/support/mesh) |
**Docs:**
[mesh rustdoc](https://openvmm.dev/rustdoc/linux/mesh/index.html)

## Contents

- **[Using mesh](mesh/usage.md)** — channels, serialization, workers, and
  other things you need to know to write code that uses mesh.
- **[How mesh works](mesh/internals.md)** — crate structure, ports and
  nodes, encoding, cross-process transport. For people working on mesh
  itself or reviewing mesh infrastructure changes.
