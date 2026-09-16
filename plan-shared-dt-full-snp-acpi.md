# Shared device-tree construction and full SNP Linux ACPI

Status: revised implementation plan; no implementation changes made.

## Goal

Make the IGVM device tree the source of truth for variable guest hardware.
Both `openhcl_boot` (through `host_fdt_parser`) and the enlightened SNP Linux
bootshim consume that same hardware description. The SNP shim generates the
complete supported ACPI set: DSDT, FADT, MADT, SRAT, optional PCIe MCFG/SSDT,
and XSDT/RSDP. `igvmfilegen` no longer embeds those tables for this boot path.

Fixed chipset devices remain hardcoded in the shim's ACPI construction, using
shared constants. DT describes CPU, memory, UART presence and PCIe resources;
it does not need to restate the fixed chipset.

There is no requirement to retain the experimental SNP static-ACPI or
PCIe-only paths, previously called v1/v2 or model A. Replace them together with
their producer. Do not introduce an `acpi` mode switch, a separate SNP DT view,
or a runtime-negotiated chipset profile.

This is not a universal hardware model, a binary-DT overlay engine, or a
conversion of every loader to native DT enumeration.

## Contracts and scope

- Preserve ARM64 native Linux DT hardware bindings and the minimal EFI/ACPI
  bootstrap DT used by ARM64 Linux ACPI boot.
- Keep the IGVM partition handoff: memory-type annotations, VTL-specific
  VMBus metadata, OpenHCL settings and entropy.
- Extend the single IGVM hardware description and update both consumers.
  Preserve OpenHCL's COM3 console selection; do not confuse console policy
  with the complete list of attached UARTs.
- Keep native Linux memory visibility and VMBus exposure separate from IGVM
  partition metadata. Linux must not claim another VTL's memory.
- Keep unrelated IGVM image types and their ACPI generation unchanged.
- Keep the tested integration and local hardcoded C experiment on their
  separate bookmarks. Do not import the experiment's layout into this work.

## Stage 1: shared hardware inputs and DT construction

### 1. Record UART identities, not duplicate resource descriptions

Add a small UART identity list to the manifest result and carry it through VM
configuration to the loader. Populate it where real UART handles are attached.
The proposed data shape is:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartId {
    Com1,
    Com2,
    Com3,
    Com4,
    Pl011_0,
    Pl011_1,
}

// Proposed field on the manifest result and VM configuration.
pub uarts: Vec<UartId>,
```

Use the repository's existing wire derives for configuration transport.
These are internal, trusted identities, not an enum decoded directly from
untrusted DT. Reuse an existing equivalent identity type if one exists.
Do not add configurable base addresses, lengths, IRQs, clocks or backend
handles to this list.

The list answers only "which UARTs exist":

- An x86 serial attachment records COM1 through COM4, even when only one
  backend is connected. Disconnected backends still have real UART devices.
- An ARM64 serial attachment records both PL011 devices.
- Mandatory UARTs are included. Missing-device PIO placeholders and debugcon
  are not.
- An empty list means no UARTs. All in-tree configuration constructors must
  populate the field explicitly; there is no old/new SNP inventory mode.
  Do not turn a missing required transport field into an empty inventory.
- Console selection remains separate and must select an attached device.
  Carry the existing console opt-in separately as `Option<UartId>`; do not
  derive it from this inventory.

For the OpenHCL x86 handoff, preserve the current explicit COM3 backend
selection. Emit its full node path in `/chosen/stdout-path` only when selected;
omit that property when the console is disabled. The updated OpenHCL consumer
uses that selection, not COM3's mere presence, to populate its console handoff.
A COM1-only backend configuration must not enable logging to disconnected
COM3. Keep native Linux's existing console-selection policy separate.

Device construction and DT emission use the same fixed UART constants.
Reuse the COM definitions behind `Serial16550DeviceHandle::com_ports`.
Consolidate duplicated PL011 constants into an existing lightweight definition
layer that both callers can use. Keep GIC SPI indices distinct from absolute
GSIVs; sharing constants must not change their encoding.

The emitted DT still includes each UART's resources. The bootshim reads and
validates those nodes; it must not fabricate all four COM devices merely
because their addresses are fixed.

### 2. Proposed shared-builder API

Add an internal `worker/vm_loaders/device_tree.rs` module with one high-level
`DeviceTreeBuilder` for both IGVM and native Linux DT construction. Only the
minimal ACPI stub stays separate. `new()` takes common hardware inputs;
boot-specific methods add IGVM or Linux-direct settings. `finish()` returns
the complete DT blob. Callers never manage open nodes, property placement,
string IDs or phandles.

The following is a proposed API sketch, not existing or compilable standalone
code. Reuse existing topology, memory, chipset and PCIe types. The builder
stores borrowed inputs and small option values, not a second tree
representation or a generic device graph.

```rust
pub(crate) struct DeviceTreeBuilder<'a, T: ArchTopology> {
    // Private input fields; no live FDT writer until finish().
}

impl<'a, T: ArchTopology> DeviceTreeBuilder<'a, T> {
    pub(crate) fn new(
        topology: &'a ProcessorTopology<T>,
        ram: &'a [MemoryRangeWithNode],
        uarts: &'a [UartId],
        pcie: &'a [PcieHostBridge],
    ) -> Self;

    pub(crate) fn with_command_line(self, command_line: &'a str) -> Self;
    pub(crate) fn with_console(self, console: Option<UartId>) -> Self;
    pub(crate) fn with_capacity(self, bytes: usize) -> Self;
}

impl<'a> DeviceTreeBuilder<'a, X86Topology> {
    pub(crate) fn with_igvm(
        self,
        chipset_mmio: ChipsetMmioRanges,
        vtl2_base_address: Vtl2BaseAddressType,
    ) -> Self;
    pub(crate) fn with_igvm_protectable_ram(self, ram: &'a [MemoryRange]) -> Self;
    pub(crate) fn with_igvm_vmbus_redirect(self, enabled: bool) -> Self;
    pub(crate) fn with_igvm_entropy(self, entropy: Option<&'a [u8]>) -> Self;
    pub(crate) fn finish(self) -> Result<Vec<u8>, DeviceTreeError>;
}

impl<'a> DeviceTreeBuilder<'a, Aarch64Topology> {
    pub(crate) fn with_linux_direct(
        self,
        chipset_low_mmio: MemoryRange,
        chipset_high_mmio: MemoryRange,
    ) -> Self;
    pub(crate) fn with_linux_initrd(self, range: Option<(u64, u64)>) -> Self;
    pub(crate) fn with_linux_smmus(self, smmus: &'a [AcpiSmmuConfig]) -> Self;
    pub(crate) fn finish(self) -> Result<Vec<u8>, DeviceTreeError>;
}
```

`ArchTopology` is the existing bound on `ProcessorTopology`. Use one builder
type and common constructor, not separate IGVM/Linux builder types.
The specialized method blocks expose the currently supported paths: x86
IGVM and ARM64 native Linux. They share the same serialization pipeline and
private common encoders; only architecture-specific operations differ.
This does not add ARM64 IGVM or x86 native-DT support as part of the refactor.

Put required hardware in `new`; empty UART/PCIe inventories are explicit,
not omitted setters. IGVM supplies partition-wide RAM; native Linux supplies
only its guest-visible RAM through the same parameter. Keep partition settings
out of `new`. Require `with_igvm(...)` or `with_linux_direct(...)` for the
supported boot path, and require `with_capacity(...)` before `finish()`.
Missing required settings produce a typed error. Boot-specific setters may
precede the boot-path method; they only record inputs, not change node state.
Do not infer a boot path from UARTs, metadata or SNP isolation.

Optional settings default to an empty command line, no console, no
protectable RAM, no redirect, no entropy, no initrd and no SMMUs where
applicable. For initrd, `None` omits both address properties and clears an
earlier value; `Some((start, end))` emits both, allowing equal addresses but
rejecting reversed bounds. Preserve the native loader's current equal-address
representation when it has no initrd during extraction.

The last supplied SMMU list is authoritative: omitted or empty means no SMMU
nodes or IOMMU mappings. The native loader must pass its complete resolved
SMMU configuration rather than accidentally use the builder default.
`with_capacity(bytes)` sets a hard total-blob buffer limit, including FDT
overhead, not a reservation hint. There is no IGVM-specific default on the
shared builder. Preserve native Linux's existing 2-MiB buffer limit at its
call site. For an IGVM DeviceTree request, derive available capacity from
its parameter-area size minus its byte offset, using
checked subtraction and conversion. Preserve the final parameter-import
bounds check. Before interning names, `finish()` must reject capacity that
cannot hold the header, reservation map and reserved string-table region.
Do not grow beyond the selected capacity or expand image storage.

Each `with_*` method only records a setting. Calls for different settings can
appear in any order; a repeated setter replaces its previous value. None
serializes bytes or depends on the currently open node. `finish()` consumes
the builder, validates the complete input, serializes once in a fixed order,
and returns only the used bytes. It returns a typed `DeviceTreeError` for
invalid input or FDT/capacity failure, never a partial successful blob.

Inside `finish()`, use the existing `fdt::builder::Builder` and private shared
serializers for common resource properties. The implementation owns the
buffer, interns names once, assigns phandles centrally and closes every node.
Private helpers may use the consuming typed-node API; it is not exposed to
loader callers. Do not add callbacks that give callers an open node.

Share PCIe ECAM/bus/segment/range encoding and `pcie::identity_ranges`, UART
resource encoding from the common constants, and common memory and `/chosen`
properties. Keep architecture-specific CPU/interrupt bindings and differences
in NUMA/preserve-config properties inside the appropriate serializers.
Keep IGVM memory classification separate from native Linux memory selection.

### 3. Loader call sites and internal composition

Both loaders call the same generic constructor and supply resolved hardware,
then add their boot-specific settings. The builder owns the complete tree
structure. The IGVM/Linux methods express the existing partition-handoff
versus native-boot contracts, not separate hardware models or SNP versions.
There is no public output-view enum, arbitrary field bag or node emitter API.

For IGVM, both consumers receive output from the same builder:

```rust
let dt = DeviceTreeBuilder::new(
    processor_topology,
    all_ram,
    uarts,
    pcie_host_bridges,
)
.with_igvm(chipset_mmio, vtl2_base_address)
.with_igvm_protectable_ram(protectable_ram)
.with_igvm_vmbus_redirect(with_vmbus_redirect)
.with_command_line(command_line)
.with_console(console)
.with_igvm_entropy(entropy)
.with_capacity(dt_capacity)
.finish()?;
```

For native ARM Linux, replace the separate hardware-tree implementation
with the same builder:

```rust
let dt = DeviceTreeBuilder::new(
    processor_topology,
    cfg.mem_layout.ram(),
    uarts,
    pcie_host_bridges,
)
.with_linux_direct(chipset_low_mmio, chipset_high_mmio)
.with_linux_initrd(Some((initrd_start, initrd_end)))
.with_linux_smmus(smmu_configs)
.with_command_line(cfg.cmdline)
.with_console(console)
.with_capacity(0x20_0000)
.finish()?;
```

Here the topology type is inferred as x86 or ARM64 at the respective call
site. Do not retain an independent native Linux serializer behind `build_dt`.
If a thin loader wrapper remains, it only gathers inputs and makes the builder
call above; it does not manage nodes or compose its own tree.

`finish()` validates and orders all root, CPU, memory, PCIe, VMBus, UART and
`/chosen` output. It adds IGVM memory annotations, VTL VMBus and OpenHCL
metadata only for the IGVM contract; native Linux gets only its own memory
and VMBus visibility. ARM topology drives GIC, PSCI, timer and MSI bindings;
Linux SMMU settings provide its existing IOMMU mappings. The same UART
inventory input produces architecture-appropriate resources and parent nodes.
The builder derives the FDT BSP field using the existing path's convention,
not a blanket APIC-ID rule for ARM.

Empty device inventories produce no children. The caller cannot accidentally
insert a UART under a PCIe node or write root properties after a child.
Do not implement two complete serialization loops inside the shared type;
keep one orchestration with private architecture/boot-specific sections.

Keep GIC/clock/ITS/v2m/SMMU phandles internal. Preserve IOMMU mappings,
preserve-config behavior and the native ARM CPU `reg` convention; switching
VP indices to MPIDRs is separate work. Likewise, keep the complete-tree
ACPI-stub entry point minimal: it uses only relevant `/chosen`/EFI serializers,
never hardware serializers. Do not serialize, parse and patch a finished DT.

Refactor existing emission without behavior changes first. Then extend IGVM
UART output and update `host_fdt_parser`, its `openhcl_boot` use, and the SNP
consumer together. OpenHCL must recognize all valid UART nodes without warning
that non-console ports are unknown, while selecting COM3 only when requested
by the separate console metadata. Validate that a selected path resolves to
a present, supported UART; malformed selection is an error.

The native Linux console path needs one explicit, reviewed behavior correction
when adopting shared console serialization: it currently emits UARTs under
`/openvmm` but points `stdout-path` at `/hvlite`. Derive the path from the
emitted UART node. Preserve native selection of the first PL011 when enabled,
and omit the property when disabled. Cover this deliberate exception to
byte-preserving extraction in a separate regression test; it is not a change
to OpenHCL's COM3 policy.

Replace the existing `device_tree_pcie_bridges` isolation-dependent filter,
not just its emitter. Publish the actual bridge inventory for both SNP and
non-SNP IGVM requests. Separate common resource validation/encoding from the
SNP consumer's bridge-count and feature restrictions. Do not apply those
restrictions globally to OpenHCL or other IGVM consumers. Update OpenHCL to
recognize the common PCIe nodes without unknown-device warnings; it may leave
them unused where its boot path does not consume PCIe information. This does
not give OpenHCL the SNP shim's ACPI generation responsibilities.

Document one IGVM UART binding in `igvm_defs::dt` and the Guide. The existing
PIO parent declares one-cell addresses/sizes but writes 64-bit `reg` entries.
For this change, retain the existing property bytes and document this IGVM
convention explicitly; both consumers must read it identically. A conversion
to standard cell-sized resources is separate work, not a SNP-specific format.
Define exact resource lengths, IRQ encoding, duplicate handling and absence
semantics. No complete-inventory marker or chipset-profile property is needed:
the updated producer always emits the complete inventory.

Both consumers validate the shared hardware fields they use. OpenHCL retains
its partition-specific behavior; the SNP shim uses supported VTL0 hardware,
ignores documented handoff-only metadata, and rejects unsupported hardware or
memory layouts. The same wire description does not require identical consumer
policies or converting the whole IGVM tree into native Linux DT.

## Stage 2: complete ACPI in the SNP Linux bootshim

### 4. Hardcode the fixed chipset devices in the shim

The x86 `EnlightenedLinuxDirect` bootshim targets a known chipset. Its ACPI
construction directly includes:

- APIC/IOAPIC, PIC/PIT and the established interrupt overrides/NMI policy;
- CMOS RTC;
- Hyper-V PM block, SCI, timer/reset registers and reset value;
- supported ACPI flags and `_S0`/`_S5` objects.

There is no DT chipset device list, profile identifier, profile handshake or
per-register property. Changes to this fixed hardware contract require
corresponding host/shim changes, not runtime profile negotiation.

Keep reusable ACPI construction in lightweight `acpi`/`acpi_spec` code and the
fixed device selection in the shim. Reuse shared register constants; move
needed definitions to a lightweight layer if their current home would pull
hosted VMM/device code into the freestanding build.

Add hosted parity tests against the actual `EnlightenedLinuxDirect` manifest
and emulator definitions. Restrict this boot path to its supported fixed
chipset and reject optional additions that need unsupported ACPI, such as a
watchdog, at host configuration validation. This is a local validity check,
not a new wire protocol. Do not silently describe hardware that is absent.

### 5. Replace the SNP producer/consumer handoff

Make DT-derived ACPI the sole path for enlightened SNP Linux images. Update
`igvmfilegen` and the shim together; remove the static-base-table and PCIe-only
branches and their retained-table fields. Remove the old SNP `pcie` toggle
where it selects this handoff: PCIe tables now depend on the DT bridge list.
Update affected manifests, examples and callers. Do not add an `acpi` selector.

- Use the existing IGVM DeviceTree parameter. The host publishes the same
  IGVM hardware tree without inspecting the shim's VMSA RSI or measured pages
  to select a special DT format.
- Add the expected CPU count to the revised measured handoff; the current
  structures do not carry it. Populate it from the image-generation
  `processor_count` input and reject counts outside 1 through 255 during
  generation. Retain measured RAM bounds and workspace capacities for image
  layout and safe acceptance. These constrain DT input; they are not a second
  hardware discovery source or a chipset-profile identifier.
- Initially retain contiguous APIC IDs starting at 0, BSP APIC ID 0, the
  image's exact CPU count, one NUMA node 0 and the contiguous RAM contract.
  Relaxing these constraints is separate work.
- Reserve complete ACPI output storage and a pinned RSDP location, without
  prebuilt base-table addresses.
- Define imported versus accepted pages, zero initialization, E820
  reservation, heap/DT lifetime and publication order.
- Keep output reserved/reclaimable in Linux E820. No final table may point
  into temporary DT or heap storage.

Replace the old layout validator rather than passing empty base fields into
it. Use an unambiguous updated handoff revision/size check to reject mismatched
tools, not to retain old execution paths. Regenerate development images with
the updated producer and shim. Old experimental SNP images are unsupported;
there is no fallback to retained ACPI. Preserve unrelated OpenHCL/image
contracts rather than globally changing the IGVM format.

### 6. Extend the bounded SNP parser

Consume the common IGVM CPU, memory and UART nodes in addition to PCIe:

- Validate CPU count, unique APIC IDs, BSP identity, enabled status and NUMA
  affinity against the measured limits. Use APIC ID + 1 consistently for
  DSDT/MADT UIDs under the initial contiguous-ID contract.
- Validate memory types and normalize ranges. Require agreement with measured
  RAM; never expand RAM or change acceptance policy from DT. Reject
  unsupported VTL-protectable or multi-node layouts.
- Validate UART identities/resources, lengths, IRQs, duplicates and overlaps
  against the supported fixed UART definitions. Emit only present devices.
- Allow shared UART IRQ numbers: COM1/COM3 share IRQ4; COM2/COM4 share IRQ3.
  Check identities and address ranges separately from interrupt lines.
- Preserve existing PCIe validation and unsupported-feature errors.
- Specify required properties, exact lengths, duplicate-property rules and
  which metadata may be ignored. Unknown hardware must not silently disappear
  from the generated ACPI.
- Bound depth, nodes, properties, devices, table sizes and cumulative
  allocation. Keep preflight allocation-free and before RAM acceptance.

Preserve the unmeasured-input and deferred-attestation comments. Input
validation and fail-before-handoff behavior are not deferred. Missing required
DT input is an error, not a reason to generate static tables.

### 7. Generate and publish one complete ACPI set

Use the `no_std + alloc` builders for:

- DSDT: DT CPUs and UARTs, plus fixed chipset devices and sleep objects;
- FADT: fixed PM/SCI/timer/reset data and the generated DSDT pointer;
- MADT: DT CPUs plus fixed controllers, NMI and interrupt overrides;
- SRAT: validated CPU and RAM affinities;
- MCFG/PCIe SSDT: host-described bridges, omitted when no bridges are present;
- XSDT/RSDP: references to the new tables and valid checksums.

Do not call the exclusive-IRQ `Dsdt::add_uart` helper four times unchanged.
Define shared-IRQ resources for these UARTs and test simultaneous guest use.
Leave other helper callers unchanged unless separately reviewed.

Do not add SLIT/PPTT or arbitrary platform tables. Preserve the current PCIe
MSI/MSI-X subset and its documented IOMMU/CXL/INTx limitations.

Initial limits are 255 CPUs (APIC IDs 0 through 254), 32 RAM records normalized
into the fixed interval, four NS16550 UARTs and eight PCIe bridges, on one NUMA
node. These are SNP consumer limits, not limits on the shared IGVM producer,
OpenHCL or native ARM Linux.

Use 64 KiB DT capacity, 64 KiB ACPI output, a 1-MiB heap and 32-KiB stack as
initial budget targets. Test the combined maximum, including temporary
buffers and cumulative allocations. If insufficient, revise the measured
layout before merging; never truncate input or fail after publication.
Publish the pinned RSDP and Linux pointer only after validating every table.
Never enter Linux with a partial ACPI set.

## Validation

### Publisher and shared consumers

- Test no serial, one connected x86 backend with four UARTs, all backends,
  mandatory/disconnected UARTs, missing-device placeholders and ARM's two UARTs.
- Test identity transport through every configuration constructor; reject
  duplicates, wrong-architecture identities and missing required fields.
- Check shared UART constants against device construction and DT output,
  including PL011 SPI versus GSIV encoding.
- Compare decoded DT properties before/after extraction, including phandles,
  memory ordering, NUMA, PCIe bindings and VTL metadata. Preserve existing
  bytes where they form a compatibility contract.
- Exercise both IGVM and native ARM through `DeviceTreeBuilder::new` and
  `finish()`. Check that constructor inputs remain boot-neutral, IGVM metadata
  does not leak into native trees, and native memory visibility is preserved.
  Test missing boot settings and capacity as errors.
  Cover absent, empty, populated, reversed and cleared initrd settings.
  Verify the native selected-console path resolves to a UART, and disabled
  selection omits it. Check configured SMMUs retain all mappings through the
  loader, and a builder-level nonempty-to-empty reset removes those mappings.
- Verify that permutations of distinct builder setters produce identical
  bytes, repeated setters use the last value, and `finish()` returns a
  complete parseable tree with correct parent nodes and BSP identity.
  Test invalid inputs and insufficient capacity without partial output.
  Compare omitted optional setters with explicit defaults; test clearing
  earlier values with `None`, empty slices/strings and `false`. Cover zero
  and undersized capacities before string interning, exact-fit and
  one-byte-short buffers, and nonzero parameter-area offsets. Capacity
  failures must return errors without panicking or publishing output.
- Feed the same generated IGVM fixtures to the real `host_fdt_parser` and SNP
  parser. Verify all UARTs are recognized and the SNP shim gets the complete
  inventory. Test COM1-only backends with OpenHCL console disabled, explicitly
  selected COM3, no UARTs, and invalid console paths.
- Verify identical bridge inventories survive SNP and non-SNP IGVM publication.
  Test that SNP-only count/feature limits do not constrain unrelated consumers,
  and that OpenHCL recognizes the published PCIe nodes.
- Exercise the `openhcl_boot` handoff using the updated parser. Test preserved
  VTL memory/VMBus behavior and rejection of layouts unsupported by SNP.
- Keep native ARM DT behavior and the minimal ACPI stub unchanged.

### Shim and tables

- Invalid CPU IDs/counts, memory types/ranges, UART resources, unsupported
  hardware and exhausted capacities must fail before handoff.
- Match DSDT/MADT UIDs; check SRAT affinities, FADT/DSDT pointers, interrupt
  overrides, register values, checksums and memory reservations.
- Compare fixed ACPI data with manifest/emulator definitions.
- Confirm newly generated SNP images contain no prebuilt ACPI tables and no
  static/PCIe-only selection branch. Test rejection of mismatched handoffs;
  do not add a legacy-image compatibility matrix.
- Test zero and maximum UART/PCIe counts, combined maximum allocations and
  one-over-limit cases. Cover 255 CPUs and reject 256/out-of-contract APIC IDs.
  Test producer count bounds, measured handoff serialization and DT/count
  mismatch rejection before acceptance.
- Verify permanent output placement, pinned RSDP bounds and exclusion from
  later acceptance operations.
- Confirm unrelated image generation still follows its existing paths.

### Runtime

- Boot 1, 2, 4 and 8 VPs on `chris-mshv`; check CPU/node assignments, timer
  wakeups and topology warnings. Use an image measured for each count and
  test rejection of count mismatches.
- Exercise poweroff/reset and PM timer behavior, not only PCIe enumeration.
- Match DT/DSDT UART inventory to attachments; cover no serial and working
  connected backends.
- Exercise simultaneous interrupt-driven COM1+COM3 and COM2+COM4 traffic.
- Repeat dynamic ECAM/BAR and PCIe virtio-blk/net I/O tests using the same
  image. Keep the current guest's below-4-GiB ECAM constraint.
- Reuse images across supported UART/PCIe variations. Reject host-expanded
  RAM and unsupported memory layouts before RAM acceptance.

## Delivery and review

Keep reviewable changes for UART identities/shared constants, the shared
hardware-DT builder and migration of both loaders, common IGVM UART publication
and consumers, fixed ACPI helpers, and replacement of the SNP producer/consumer
path. Land dependent activation
together so an intermediate release cannot produce unusable images.

Update the directly related Guide pages and doc-code-sync mapping as needed
during implementation. Review new code before committing. Run per-package
checks, tests, Clippy and docs, the freestanding shim build, then repository
formatting last. Preserve an integration bookmark for runtime validation.
Keep local experiments out of production changes. Do not push.

## Evidence for the API and scope

- `vmm_core/vm_manifest_builder/src/lib.rs:215-275,855-936`: real UART
  construction, disconnected backends, fixed PL011 resources and attachment.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:472-720`: current inputs,
  CPU/memory/PCIe emission, PIO UART encoding and OpenHCL metadata.
- `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs:249-330,540-620`:
  architecture-specific bindings and common PCIe/UART emission candidates.
- `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs:481-510,570-600,648-657,948-974`:
  supplied SMMU inventory, native UART/console path mismatch, and the
  equal-address initrd representation to preserve.
- `support/fdt/src/builder.rs:20-26,232-285`: consuming builder API and typed
  nesting to keep private inside the shared complete-tree builder.
- `vm/vmcore/vm_topology/src/processor.rs:35-44`: existing generic
  `ProcessorTopology<T: ArchTopology>` used by the common constructor.
- `openhcl/host_fdt_parser/src/lib.rs:1008-1063`: current 64-bit PIO resource
  reads, COM3 selection and warnings for other ports.
- `vmm_core/vm_manifest_builder/src/lib.rs:541-576`: current fixed
  `EnlightenedLinuxDirect` chipset attachments.
- `openvmm/openvmm_entry/src/lib.rs:1225-1230,1354-1357` and
  `openhcl/openhcl_boot/src/boot_logger.rs:89-110`: configured COM3 backend
  selection and the resulting logger enablement.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:135-181,1228-1242` and
  `openhcl/host_fdt_parser/src/lib.rs:779-814`: current SNP-only bridge
  publication gate and OpenHCL unknown-compatible handling.
- `vm/loader/loader_defs/src/linux.rs:341-384` and
  `vm/loader/igvmfilegen/src/snp_linux_direct.rs:87-100,205-235`: current handoff
  structures lack CPU count; generation currently uses it for topology/ACPI.

## Review

The prior review is superseded where it required legacy SNP modes, a separate
SNP DT view, inventory markers or profile negotiation.

The first five user comments are incorporated: one authoritative IGVM DT, a
concrete builder API, UART identities with shared constants, no output-view
enum, and fixed chipset construction in the shim. The follow-up comment
replaces public emitter calls with an input-collecting builder and `finish()`;
node placement and ordering are now implementation details.
The latest comments further require one builder for IGVM and native Linux:
`DeviceTreeBuilder<'a, T>` now has a common constructor, common setters and
boot-specific IGVM/Linux methods. Only the minimal ACPI stub stays separate.

Previous verdict: **Minor revisions**. The review confirmed that private typed
FDT helpers fit the existing builder. It identified three corrections, retained
in this revision:

1. Preserve OpenHCL console opt-in independently of complete UART inventory,
   with selected-console metadata and COM1-only/COM3/no-UART tests.
2. Remove the existing SNP-only PCIe publication gate, retain consumer-specific
   validation, and define OpenHCL handling of the common nodes.
3. Add the expected CPU count to the revised measured handoff rather than
   assume it already exists, with producer bounds and mismatch tests.

The review found native ARM preservation, bounded preflight, memory acceptance
constraints and publication ordering adequately covered. It did not require
legacy modes, inventory markers, separate DT views or chipset negotiation.
This revision changes only the plan, not implementation code.

High-level builder review: **Minor revisions**. The review confirmed that
sections 2 and 3 resolve the follow-up comment: callers supply inputs and
receive a complete tree from consuming `finish()`, with all FDT node state
private. Prior decisions remain intact. Its capacity corrections are
incorporated: offset-aware parameter limits, checks before string interning,
and tests for defaults, resets and capacity boundaries. The latest user
comments supersede that revision's separate IGVM/native entry-point design;
the capacity checks still apply, with capacity supplied by each caller.

Unified builder review: **Minor revisions**. The review confirmed that the
common generic constructor, boot-specific methods and single private
serialization orchestration satisfy both latest comments. Architecture
specialization matches current support. Its corrections are incorporated:
explicit optional-initrd/reset behavior, a documented native console-path
correction that retains selection policy, and authoritative SMMU setter values
with complete inventory supplied by the loader. Capacity and prior safety
contracts remain intact. No separate IGVM/Linux builders or duplicated
complete-tree serializers are needed.
