---
applyTo: "**/*.rs,**/Cargo.toml"
---

# Documentation Sync

If a PR makes a structural change — new feature, renamed concept, changed CLI
flag, new device or backend type — check whether the
[OpenVMM Guide](https://openvmm.dev) (`Guide/src/`) covers that topic. If it
does, the PR should update the Guide or flag a follow-up.

## Quick heuristics

- CLI args changed in `openvmm_entry` → `Guide/src/reference/openvmm/management/cli.md`
- Device crate under `vm/devices/` changed → look for a matching page under `Guide/src/reference/`
- virtio-fs backends, queue/cache policy, save/restore, or microVM attachment
  semantics changed (`vm/devices/virtio/virtiofs/`,
  `vm/devices/virtio/virtio_resources/`, `vm/devices/support/fs/fuse/`) →
  `Guide/src/reference/devices/virtio/virtio-fs.md`
- Control-session framing, record types, payload rules, receive credits,
  authentication, or reconnect behavior changed
  (`vm/devices/virtio/virtio_console/`) →
  `Guide/src/reference/openvmm/management/control_session_protocol.md`
- OpenHCL internals changed (`openhcl/`) → `Guide/src/reference/architecture/openhcl/`
- Crate renamed or moved → grep `Guide/src/` for the old name
- New crate under `vm/devices/` → consider whether it needs a reference page

For the full maintenance procedures, load the **`guide-maintenance`** skill.
