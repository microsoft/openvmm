# Agent Result: Fix microsoft/openvmm#3463

## Root Cause

`VmManifestBuilder::build()` returns a `VmChipsetResult` struct, but `BaseChipsetBuilder`
had no method to accept it directly. Every call site was forced to destructure the result
and pass four fields via separate builder calls (`.with_expected_manifest()`,
`.with_device_handles()`, `.with_pci_device_handles()`, `.with_isa_dma_handle()`).

The root complication was a **circular dependency**: `VmChipsetResult` was defined in
`vm_manifest_builder` (which depends on `vmotherboard`), so it could not be used directly
in `BaseChipsetBuilder` (which lives in `vmotherboard`) without creating a cycle.

## Change Made

**`vmm_core/vmotherboard/src/base_chipset.rs`**
- Added `VmChipsetResult` struct to the `options` module. All its field types
  (`BaseChipsetManifest`, `ChipsetDeviceHandle`, `LegacyPciChipsetDeviceHandle`,
  `VmChipsetCapabilities`, `Resource<IsaDmaControllerHandleKind>`) already resided in
  `vmotherboard`, so no new dependencies were needed.
- Added `BaseChipsetBuilder::with_vm_chipset_result(result: VmChipsetResult) -> Self`
  convenience method that fans out to the four underlying builder calls. The `capabilities`
  field is not consumed by the builder (it is for the caller's use); callers copy it out
  before the call since `VmChipsetCapabilities: Copy`.

**`vmm_core/vm_manifest_builder/src/lib.rs`**
- Removed the `VmChipsetResult` struct definition (moved to `vmotherboard`).
- Added `pub use vmotherboard::options::VmChipsetResult;` for source compatibility.
- Converted the `impl VmChipsetResult { ... }` inherent impl block to a private extension
  trait `VmChipsetResultExt` (inherent impls must be in the defining crate; since the type
  moved, a private extension trait is the idiomatic solution).

**`openhcl/underhill_core/src/worker.rs`**
- Replaced the four `.with_*()` builder calls with a single `.with_vm_chipset_result(...)`.

**`openvmm/openvmm_core/src/worker/dispatch.rs`**
- Added `use vmotherboard::options::VmChipsetResult;` import.
- Replaced the four `.with_*()` builder calls with a single `.with_vm_chipset_result(...)`.

The optional `VmConfig` consolidation (section 2 of the issue) was not implemented as it
is a larger refactor beyond the minimal fix requested.

## Testing

Cargo is not available in this environment. The changes were verified by:
- Manual code review of all modified files for syntactic and semantic correctness.
- Confirming all field types in `VmChipsetResult` are already in scope in `vmotherboard::options` (via `use super::*`).
- Confirming `vm_manifest_builder` re-exports `VmChipsetResult` for callers using the old import path.
- Confirming call sites properly include all fields when constructing `VmChipsetResult` before passing to `with_vm_chipset_result`.
- Confirming `capabilities` is captured from the destructured result before being included in the struct literal at each call site.

## Lint

Cargo is not available in this environment; clippy, doc, fmt, and nextest could not be run.
The code follows existing style patterns in the repository.
