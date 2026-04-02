# Moving Artifact-to-Build Mapping to Compile Time

## Problem Statement

The current `test_artifact_mapping_completeness` CI gate validates at **runtime**
that every petri artifact ID has a corresponding entry in
`flowey_lib_hvlite::artifact_to_build_mapping::resolve_artifact()`. This is
fragile for several reasons:

1. **Late feedback** — developers only discover missing mappings when 4 CI jobs
   run (one per platform: x64-linux, x64-windows, aarch64-linux,
   aarch64-windows), adding ~5+ minutes to the feedback loop.
2. **String-based matching** — the mapping matches on `module_path!()`-derived
   strings like `"petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"`.
   Renaming a module silently breaks the mapping with no compiler warning.
3. **Two disconnected sources of truth** — artifact declarations live in
   `petri_artifacts_vmm_test` while mappings live in
   `flowey_lib_hvlite::artifact_to_build_mapping`, with no compile-time link
   between them.
4. **Extra CI cost** — 4 dedicated PR gate jobs exist solely to catch this
   class of error.

## Current Architecture

### How artifacts are declared

Artifacts are declared via the `declare_artifacts!` macro in
`petri_artifacts_core` (used in `petri_artifacts_vmm_test` and
`petri_artifacts_common`):

```rust
// petri_artifacts_vmm_test/src/lib.rs
pub mod artifacts {
    declare_artifacts! {
        OPENVMM_WIN_X64,
        OPENVMM_LINUX_X64,
        // ...
    }

    pub mod openhcl_igvm {
        declare_artifacts! {
            LATEST_STANDARD_X64,
            // ...
        }
    }

    pub mod test_vhd {
        declare_artifacts! {
            GUEST_TEST_UEFI_X64,
            ALPINE_3_23_X64,
            // ...
        }
    }
    // ... more submodules
}
```

For each artifact name, the macro generates:
1. A **marker enum** (zero-sized type, e.g., `enum OPENVMM_WIN_X64 {}`)
2. A **const handle** (`pub const OPENVMM_WIN_X64: ArtifactHandle<OPENVMM_WIN_X64>`)
3. An **`ArtifactId` impl** with `GLOBAL_UNIQUE_ID` set to `module_path!()`

### How tests declare artifact requirements

Tests use a resolver closure that calls `resolver.require(handle)`:

```rust
petri::test!(my_test, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    Some(MyArtifacts { openvmm, kernel })
});
```

The resolver operates in two modes:
- **Collection mode** (`ArtifactResolver::collector`): records artifact IDs into
  `TestArtifactRequirements` without resolving paths.
- **Resolution mode** (`ArtifactResolver::resolver`): returns actual `PathBuf`s.

### How artifact IDs become strings

When `--list-required-artifacts` runs, collected `ErasedArtifactHandle`s are
formatted via their `Debug` impl, which outputs the `module_path!()`-based
string with `__ty` suffix stripped:

```rust
// petri_artifacts_core/src/lib.rs
impl Debug for ErasedArtifactHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.artifact_id_str.strip_suffix("__ty").unwrap_or(self.artifact_id_str))
    }
}
```

Result: `"petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"`

### How flowey maps strings to build selections

`artifact_to_build_mapping.rs` has a ~250-line `match` statement on these
strings:

```rust
fn resolve_artifact(&mut self, artifact_id: &str, ...) -> bool {
    match artifact_id {
        "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"
        | "petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_X64" => {
            self.build.openvmm = true;
            true
        }
        // ... ~60 more arms
        _ => false  // unknown → CI gate fails
    }
}
```

### The runtime completeness check

`test_artifact_mapping_completeness.rs` wires up:
1. `local_discover_vmm_tests_artifacts` — runs `cargo nextest list` + test
   binary `--list-required-artifacts` to discover all artifact ID strings
2. `ResolvedArtifactSelections::from_artifact_list_json()` — feeds them through
   the match
3. Fails if `resolved.unknown` is non-empty

This runs as **4 separate CI jobs** in `checkin_gates.rs` (lines 1365–1417).

### Dependency graph (relevant crates)

```
petri_artifacts_core          (declares ArtifactId, ArtifactHandle, declare_artifacts!)
    ↑
petri_artifacts_common        (declares PIPETTE_*, TEST_LOG_DIRECTORY)
    ↑
petri_artifacts_vmm_test      (declares OPENVMM_*, OPENHCL_*, test VHDs, etc.)

flowey_lib_hvlite             (has artifact_to_build_mapping.rs)
    └── depends on: vmm_test_images (for KnownTestArtifacts enum)
    └── does NOT depend on: petri_artifacts_core or petri_artifacts_vmm_test
```

This dependency gap is the root cause: `flowey_lib_hvlite` cannot reference
artifact types, so it must match on strings.

---

## Root Cause Analysis

The fundamental issue is **type erasure across a crate boundary with no
shared vocabulary**:

1. Artifacts have rich type information (`ArtifactHandle<OPENVMM_WIN_X64>`)
2. When collected, types are erased to `ErasedArtifactHandle` (just a `&'static str`)
3. The erased handles cross into flowey, which has no dependency on the artifact
   crates
4. Flowey reconstructs meaning by string-matching — an inherently open-ended
   operation with no exhaustiveness guarantee

Any compile-time solution must either:
- **Preserve type information** long enough for flowey to consume it, or
- **Embed build metadata** into the artifact declaration so flowey doesn't need
  to independently maintain a mapping, or
- **Create a shared vocabulary** (enum/trait) that both sides reference

---

## Proposed Approaches

### Approach A: Trait bound on `require()` + build-category enum

**Core idea**: Add a `HasBuildMapping` trait with an associated const to
`petri_artifacts_core`. Require it as a bound on `ArtifactResolver::require()`.
Store the build category in `ErasedArtifactHandle`. Flowey matches on the
category enum (exhaustive) instead of strings.

**Changes**:

```rust
// petri_artifacts_core/src/lib.rs — NEW

/// What flowey needs to do with this artifact.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactBuildCategory {
    /// Must be cargo-built. The &'static str is a stable build-target key
    /// (e.g., "openvmm", "openhcl", "pipette_linux").
    Build(&'static str),
    /// Downloaded from an external source (VHDs, ISOs, VMGS, release IGVMs).
    Download,
    /// Always available from deps/environment (firmware, log directory).
    AlwaysAvailable,
}

/// Every artifact that can be used in a test must declare its build category.
pub trait HasBuildMapping: ArtifactId {
    const BUILD_CATEGORY: ArtifactBuildCategory;
}
```

Extend `ErasedArtifactHandle`:

```rust
pub struct ErasedArtifactHandle {
    artifact_id_str: &'static str,
    build_category: ArtifactBuildCategory,  // NEW
}

impl<A: ArtifactId + HasBuildMapping> AsArtifactHandle for ArtifactHandle<A> {
    fn erase(&self) -> ErasedArtifactHandle {
        ErasedArtifactHandle {
            artifact_id_str: A::GLOBAL_UNIQUE_ID,
            build_category: A::BUILD_CATEGORY,
        }
    }
}
```

Add bound to `require()`:

```rust
impl<'a> ArtifactResolver<'a> {
    pub fn require<A: ArtifactId + HasBuildMapping>(&self, handle: ArtifactHandle<A>)
        -> ResolvedArtifact<A> { ... }
}
```

At declaration sites, implement the trait:

```rust
// petri_artifacts_vmm_test/src/lib.rs
declare_artifacts! { OPENVMM_WIN_X64, OPENVMM_LINUX_X64 }

impl HasBuildMapping for OPENVMM_WIN_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory = ArtifactBuildCategory::Build("openvmm");
}
impl HasBuildMapping for OPENVMM_LINUX_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory = ArtifactBuildCategory::Build("openvmm");
}
```

Or extend `declare_artifacts!` to accept the category inline:

```rust
declare_artifacts! {
    OPENVMM_WIN_X64 => Build("openvmm"),
    OPENVMM_LINUX_X64 => Build("openvmm"),
    // ...
}
```

In flowey, replace the string-match with a category-match:

```rust
fn resolve_from_category(
    &mut self,
    category: ArtifactBuildCategory,
    artifact_id: &str,  // still available for download-specific logic
) {
    match category {
        ArtifactBuildCategory::Build(target) => {
            match target {
                "openvmm" => self.build.openvmm = true,
                "openhcl" => self.build.openhcl = true,
                "pipette_linux" => self.build.pipette_linux = true,
                // ... exhaustive if you use an enum instead of &str
                _ => panic!("unknown build target: {target}"),
            }
        }
        ArtifactBuildCategory::Download => { /* handled by KnownTestArtifacts */ }
        ArtifactBuildCategory::AlwaysAvailable => { /* nothing to do */ }
    }
}
```

**Compile-time enforcement**: If a developer adds a new artifact via
`declare_artifacts!` and uses it in a test (`resolver.require(NEW_ARTIFACT)`)
without implementing `HasBuildMapping`, they get a compiler error:

```
error[E0277]: the trait bound `NEW_ARTIFACT: HasBuildMapping` is not satisfied
```

**Pros**:
- True compile-time enforcement via trait bounds
- Category enum is small and stable — adding a new artifact to an existing
  category (e.g., a new OpenHCL IGVM variant) requires zero flowey changes
- Eliminates the 4 CI gate jobs
- Build category info co-located with artifact declaration

**Cons**:
- Introduces build-system concepts (`ArtifactBuildCategory`) into
  `petri_artifacts_core`, which is otherwise build-system-agnostic
- Requires implementing `HasBuildMapping` for every existing artifact (~60 impls)
- `Build(&'static str)` keys are still stringly-typed unless replaced with an
  enum (which would further couple petri to flowey)
- Platform-specific logic (e.g., "this VHD also needs pipette_windows on
  Windows") can't be expressed purely in the category — some flowey-side logic
  remains

**Mitigation for the coupling concern**: The `ArtifactBuildCategory` enum can be
kept generic — `Build`, `Download`, `AlwaysAvailable` are universal concepts,
not flowey-specific. The build target keys (`"openvmm"`, `"openhcl"`) are
stable identifiers that any build system could use.

---

### Approach B: Dedicated build-target enum as the category key

**Core idea**: Instead of `Build(&'static str)`, use an enum for build targets.
This makes the flowey-side match exhaustive.

```rust
// In petri_artifacts_core (or a new shared crate)
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BuildTarget {
    Openvmm,
    OpenvmmVhost,
    Openhcl,
    GuestTestUefi,
    Tmk,
    TmkVmmWindows,
    TmkVmmLinux,
    Vmgstool,
    PipetteWindows,
    PipetteLinux,
    TpmGuestTestsWindows,
    TpmGuestTestsLinux,
    TestIgvmAgentRpcServer,
    PrepSteps,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactBuildCategory {
    Build(BuildTarget),
    Download,
    ReleaseDownload,
    AlwaysAvailable,
}
```

**Additional compile-time guarantee**: Adding a new `BuildTarget` variant forces
updating every `match` on `BuildTarget` in flowey (unless `#[non_exhaustive]` is
used, in which case there's a `_` arm — but we can choose not to use it in
flowey's internal match).

**Pros**: Two layers of compile-time checking — trait bound AND exhaustive match
**Cons**: More coupling; the enum must live somewhere both petri and flowey can
see. ~15 variants today, grows with new build targets.

---

### Approach C: `linkme` distributed-slice registration (co-located mappings)

**Core idea**: Instead of a centralized match statement, each artifact
declaration site also registers its build mapping via a `linkme::distributed_slice`.
At "resolve time" the slice is iterated to build the `BuildSelections`.

```rust
// petri_artifacts_core or a new crate
pub struct ArtifactBuildRegistration {
    pub artifact_id: &'static str,
    pub apply: fn(&mut BuildSelectionsAccumulator),
}

#[linkme::distributed_slice]
pub static ARTIFACT_BUILD_REGISTRY: [ArtifactBuildRegistration];
```

At declaration sites:

```rust
// petri_artifacts_vmm_test/src/lib.rs
declare_artifacts! { OPENVMM_WIN_X64 }

#[linkme::distributed_slice(ARTIFACT_BUILD_REGISTRY)]
static REG_OPENVMM_WIN_X64: ArtifactBuildRegistration = ArtifactBuildRegistration {
    artifact_id: "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64",
    apply: |acc| acc.set_openvmm(true),
};
```

**Pros**:
- Co-locates mapping with declaration (harder to forget)
- No centralized match statement to maintain
- Uses existing `linkme` pattern (already used for test registration)

**Cons**:
- **Not strictly compile-time** — forgetting the registration is still possible
  (just less likely due to co-location)
- Requires a `BuildSelectionsAccumulator` trait/interface visible to both sides
- Registration boilerplate per artifact (could be wrapped in the macro)
- The registry is populated at link time, not compile time — a forgotten
  registration is still a runtime error

---

### Approach D: Extend `declare_artifacts!` to generate everything

**Core idea**: A single macro invocation declares the artifact AND its build
mapping, making it impossible to have one without the other.

```rust
declare_artifacts! {
    OPENVMM_WIN_X64 => build(openvmm),
    OPENVMM_LINUX_X64 => build(openvmm),
    GUEST_TEST_UEFI_X64 => build(guest_test_uefi),
    ALPINE_3_23_X64 => download,
    LINUX_DIRECT_TEST_KERNEL_X64 => always_available,
}
```

The macro expands to:
1. The existing marker enum + const handle + `ArtifactId` impl
2. A `HasBuildMapping` impl (as in Approach A)
3. Optionally, a `linkme` registration (as in Approach C)

**Pros**:
- Single source of truth — impossible to declare without mapping
- Clean API
- Compile-time enforcement (via trait bound on `require()`)

**Cons**:
- Larger macro, more complex to maintain
- Every `declare_artifacts!` call site must be updated
- Build concepts baked into the declaration macro

---

### Approach E: Separate mapping crate with exhaustive const array

**Core idea**: Create a new crate (e.g., `petri_artifact_mappings`) that depends
on both `petri_artifacts_vmm_test` and provides the mapping. Use a const array
of `(ErasedArtifactHandle, BuildCategory)` pairs and a compile-time or test-time
length assertion.

```rust
// petri_artifact_mappings/src/lib.rs
use petri_artifacts_vmm_test::artifacts::*;

pub const ARTIFACT_MAPPINGS: &[(ErasedArtifactHandle, BuildCategory)] = &[
    (OPENVMM_WIN_X64.erase(), BuildCategory::Build("openvmm")),
    (OPENVMM_LINUX_X64.erase(), BuildCategory::Build("openvmm")),
    // ...
];
```

Then add a unit test that compares `ARTIFACT_MAPPINGS` against the full list
of artifacts (obtained by running the test binary). This is still a test-time
check, but it runs as a fast `cargo test` in the mapping crate rather than a
full CI gate.

Alternatively, if `ErasedArtifactHandle::erase()` can be `const fn`, the array
construction is fully compile-time, and a `const_assert!(ARTIFACT_MAPPINGS.len()
== EXPECTED_COUNT)` provides a compile-time length check.

**Pros**:
- Centralizes mapping in one crate with proper dependencies
- Uses actual types (not strings) for the artifact handles
- Fast unit test replaces slow CI gate

**Cons**:
- Still requires manual maintenance of the array
- Length assertion catches additions but not removals or mismatches
- `erase()` may not be `const fn` today (depends on `module_path!()` usage)

---

## Comparison Matrix

| Criterion                          | A (trait bound) | B (build enum) | C (linkme) | D (unified macro) | E (mapping crate) |
|------------------------------------|:---:|:---:|:---:|:---:|:---:|
| True compile-time error            | ✅  | ✅  | ❌  | ✅  | ⚠️  |
| No string matching                 | ✅  | ✅  | ❌  | ✅  | ✅  |
| Co-located with declaration        | ✅  | ✅  | ✅  | ✅  | ❌  |
| Low coupling to build system       | ⚠️  | ❌  | ❌  | ⚠️  | ✅  |
| Small diff / incremental adoption  | ⚠️  | ❌  | ⚠️  | ❌  | ✅  |
| Eliminates CI gate jobs            | ✅  | ✅  | ⚠️  | ✅  | ⚠️  |
| Handles platform-specific logic    | ⚠️  | ✅  | ✅  | ⚠️  | ⚠️  |

✅ = fully addressed, ⚠️ = partially addressed, ❌ = not addressed

---

## Recommendation

**Approach A (trait bound + build-category enum)** or **Approach D (unified
macro)** provide the strongest compile-time guarantees. Of these, **Approach A
is more incremental** — it can be adopted without rewriting the
`declare_artifacts!` macro, by adding `HasBuildMapping` impls alongside existing
declarations. Once all impls are in place, the 4 CI gate jobs can be removed.

### Suggested implementation order

1. Add `ArtifactBuildCategory` enum and `HasBuildMapping` trait to
   `petri_artifacts_core`
2. Add `build_category` field to `ErasedArtifactHandle`
3. Add `HasBuildMapping` bound to `ArtifactResolver::require()` and
   `ArtifactResolver::try_require()`
4. Implement `HasBuildMapping` for all existing artifacts in
   `petri_artifacts_vmm_test` and `petri_artifacts_common`
5. Update flowey's `artifact_to_build_mapping.rs` to match on
   `build_category` instead of strings
6. Remove `test_artifact_mapping_completeness` job and its 4 CI gates
7. Optionally evolve toward Approach D (unified macro) later

### Handling platform-specific mapping logic

Some artifacts need platform-dependent build logic (e.g., Windows VHDs also
require `pipette_windows` to be built). This can be handled by:

- Keeping a small amount of category-to-selections logic in flowey that
  considers the target platform
- Using additional trait associated consts (e.g.,
  `const ALSO_NEEDS_PIPETTE: bool`) for artifacts that have side-dependencies
- Or accepting that the `Download` category's secondary effects (like needing
  pipette) are inherently platform-logic and stay in flowey — the category just
  needs to be recognized, not contain every detail

### Open questions

1. **Should `ArtifactBuildCategory` live in `petri_artifacts_core` or a new
   intermediate crate?** Putting it in `petri_artifacts_core` is simplest but
   introduces build-system concepts. A separate `petri_artifact_build_info`
   crate keeps concerns separated but adds a crate.

2. **Should `Build` carry a `&'static str` key or a dedicated enum?** A string
   key is simpler and more extensible; an enum provides exhaustive matching.
   Could start with strings and upgrade to an enum if desired.

3. **Should `declare_artifacts!` be extended to accept the category, or should
   `HasBuildMapping` impls be written separately?** Extending the macro is
   cleaner long-term (Approach D) but is a larger change. Separate impls allow
   incremental adoption.

---

## Files involved in the current approach

| File | Role |
|------|------|
| `petri/petri_artifacts_core/src/lib.rs` | `ArtifactId`, `ArtifactHandle`, `ErasedArtifactHandle`, `declare_artifacts!` |
| `vmm_tests/petri_artifacts_vmm_test/src/lib.rs` | All VMM test artifact declarations |
| `petri/petri_artifacts_common/src/lib.rs` | Common artifacts (pipette, log dir) |
| `petri/src/test.rs` | `--list-required-artifacts`, `ArtifactResolver`, test collection |
| `flowey/flowey_lib_hvlite/src/artifact_to_build_mapping.rs` | String-based mapping (the thing to replace) |
| `flowey/flowey_lib_hvlite/src/_jobs/test_artifact_mapping_completeness.rs` | Runtime completeness check (the thing to remove) |
| `flowey/flowey_lib_hvlite/src/_jobs/local_discover_vmm_tests_artifacts.rs` | Artifact discovery via nextest + test binary |
| `flowey/flowey_lib_hvlite/src/_jobs/local_build_and_run_nextest_vmm_tests.rs` | `BuildSelections` struct |
| `flowey/flowey_hvlite/src/pipelines/checkin_gates.rs` | CI gate wiring (lines 1365–1417) |
