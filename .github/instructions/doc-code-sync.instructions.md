---
applyTo: "**/*.rs,**/Cargo.toml"
---

# Documentation Sync

Updates to the [OpenVMM Guide](https://openvmm.dev) (`Guide/src/`) are
conditional, not a checklist requirement.

Update the Guide when a change makes an existing statement or example
incorrect, or introduces a capability, limitation, or workflow that readers
need to understand. Topic overlap alone does not require an edit.

Internal refactors, implementation-only changes, and bug fixes that restore
documented behavior normally need no Guide update. No Guide change is a valid
outcome.

## Scope and detail

- Make the smallest sufficient edit. Document what readers need to do or
  understand, not how the patch implements it.
- Keep local invariants, helper types, control flow, and test mechanics in
  code comments or rustdoc.
- Architecture pages may explain stable design choices and important
  constraints. Do not turn them into implementation inventories or patch
  histories.
- Do not expand nearby documentation or fill unrelated gaps unless requested.

## Quick heuristics

These identify pages to check, not mandatory edits:

- CLI syntax, defaults, or behavior changed in `openvmm_entry` →
  `Guide/src/reference/openvmm/management/cli.md`
- Device capability or limitation changed under `vm/devices/` → look for a
  matching page under `Guide/src/reference/`
- Documented OpenHCL architecture changed →
  `Guide/src/reference/architecture/openhcl/`
- Crate renamed or moved → grep `Guide/src/` for references that need updating
- New device or backend → consider whether readers need reference coverage;
  prefer an existing page when it adequately covers the topic

For the full maintenance procedures, load the **`guide-maintenance`** skill.
