# Agent Task: Fix microsoft/openvmm#3463

## Issue

**Repository:** microsoft/openvmm
**Issue:** #3463
**Title:** BaseChipsetBuilder should accept VmChipsetResult directly instead of piecemeal fields
**URL:** https://github.com/microsoft/openvmm/issues/3463
**Labels:** good first issue

## Description

## Summary

`VmManifestBuilder::build()` returns a `VmChipsetResult` containing `chipset` (the manifest), `chipset_devices`, `pci_chipset_devices`, and `capabilities`. At every call site, that struct is immediately destructured and its fields are passed individually into `BaseChipsetBuilder` via separate builder calls:

```rust
// underhill_core/src/worker.rs (and openvmm_core/src/worker/dispatch.rs)
let VmChipsetResult {
    chipset,
    mut chipset_devices,
    pci_chipset_devices,
    capabilities,
} = chipset.build()?;

// ... later ...

BaseChipsetBuilder::new(foundation, devices)
    .with_expected_manifest(chipset)
    .with_device_handles(chipset_devices)
    .with_pci_device_handles(pci_chipset_devices)
    // ...
    .build(...)
    .await?;
```

This pattern appears in at least two places:
- `openhcl/underhill_core/src/worker.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`

## Proposed Change

### 1. `BaseChipsetBuilder`: accept `VmChipsetResult` directly

Add a `with_vm_chipset_result(result: VmChipsetResult)` method (or integrate it into `BaseChipsetBuilder::new`) so callers can pass the whole `VmChipsetResult` directly, eliminating the repetitive destructuring at each call site.

### 2. `VmConfig` in `openvmm_core`: consolidate chipset fields

`VmConfig` currently stores the `VmChipsetResult` fields split across separate fields:
- `chipset: BaseChipsetManifest`
- `chipset_devices: Vec<ChipsetDeviceHandle>`
- `pci_chipset_devices: Vec<LegacyPciChipsetDeviceHandle>`
- `chipset_capabilities: VmChipsetCapabilities`

These are effectively the same as `VmChipsetResult`. Replacing them with a single `VmChipsetResult` field would keep the two types in sync and reduce divergence risk.

## Motivation

- `VmChipsetResult`'s fields were designed together and are produced together — splitting them apart at every consumer is accidental complexity.
- If `VmChipsetResult` gains or loses fields in the future, each call site must be updated manually; a single `with_vm_chipset_result` call handles this automatically.
- Reduces boilerplate at call sites.

## Affected Files

- `vmm_core/vmotherboard/src/base_chipset.rs` — add the new method
- `openhcl/underhill_core/src/worker.rs` — update call site
- `openvmm/openvmm_core/src/worker/dispatch.rs` — update call site and `VmConfig`
- `openvmm/openvmm_entry/src/lib.rs` — update call site
- `petri/src/vm/openvmm/construct.rs` — may need updating depending on how `VmChipsetResult` flows through


## Instructions

1. Read and understand this issue thoroughly.
2. Explore the codebase to find relevant files.
3. Identify the root cause or the correct approach.
4. Implement the SMALLEST safe fix.
5. Add or update tests if applicable.
6. Run relevant tests to verify.
7. Do NOT submit a PR or push.
8. Write a summary of your changes to AGENT_RESULT.md.

## Rules

- Keep changes minimal and focused.
- Do not refactor unrelated code.
- Do not add features beyond the issue scope.
- Follow the repo's existing code style.
- If unsure, write your analysis in AGENT_RESULT.md even without a fix.
