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
- OpenHCL internals changed (`openhcl/`) → `Guide/src/reference/architecture/openhcl/`
- Crate renamed or moved → grep `Guide/src/` for the old name
- New crate under `vm/devices/` → consider whether it needs a reference page

For the full code-to-Guide mapping and maintenance procedures, load the
**`guide-maintenance`** skill.
