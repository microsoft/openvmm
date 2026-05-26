---
name: guide-maintenance
description: >
  Maintain the OpenVMM Guide and its code-sync mapping. Load when: (1) adding,
  removing, or moving Guide pages, (2) adding new device crates or CLI args
  that need doc coverage, (3) updating the doc-code-sync mapping table, or
  (4) auditing Guide freshness against code changes.
---

# Guide Maintenance Skill

Procedures for keeping the OpenVMM Guide (`Guide/src/`) in sync with the
codebase. The doc-code-sync mapping table is maintained in this skill file;
`.github/instructions/doc-code-sync.instructions.md` contains the instructions
and heuristics that are automatically loaded during Copilot code review on
`*.rs` and `Cargo.toml` files.

---

## Adding a New Guide Page

### 1. Choose the right location

| Content type | Path pattern |
|-------------|-------------|
| Emulated device reference | `reference/emulated/<DeviceCategory>/<device>.md` |
| VMBus device reference | `reference/devices/vmbus/<device>.md` |
| Virtio device reference | `reference/devices/virtio/<device>.md` |
| Device backend reference | `reference/backends/<category>.md` |
| Architecture deep-dive | `reference/architecture/openvmm/` or `reference/architecture/openhcl/` |
| CLI / management | `reference/openvmm/management/` |
| Developer tool | `dev_guide/dev_tools/<tool>.md` |
| Test framework | `dev_guide/tests/` |

### 2. Create the page

- Follow the style in `.github/instructions/guide-docs.instructions.md`
- Include a brief overview, key concepts, and links to relevant rustdoc
- Reference crate names in backticks
- Use relative paths from repo root when pointing at code

### 3. Update SUMMARY.md

Add the page to `Guide/src/SUMMARY.md` in the correct section. Pages with
content get a path; placeholder topics for future work get an empty link `()`.

```markdown
- [Page Title](./reference/path/to/page.md)
```

### 4. Update the doc-code-sync mapping

**This is the critical step.** Add a row to the mapping table in this file:

```markdown
| `path/to/code/crate/` | `reference/path/to/page.md` |
```

Also add a bullet to the "What to Flag" section if the new page covers a
category of change (e.g., "New frobulator variant added → update frobulator.md").

---

## Removing or Moving a Guide Page

1. Update or remove the entry in `Guide/src/SUMMARY.md`
2. Remove or update the corresponding row in the doc-code-sync mapping table
3. Check for cross-references from other Guide pages (grep for the old path)
4. If moving: ensure the SUMMARY.md link and the mapping table both point to
   the new location

---

## Adding a New Device Crate

When a new crate is added under `vm/devices/`:

1. **Does it need a Guide page?** If it's a user-visible device or backend,
   yes. Internal plumbing crates (e.g., `*_resources`, `*_protocol`) usually
   don't need their own page but should be mentioned in the parent device page.

2. **Create the page** following the "Adding a New Guide Page" procedure above.

3. **Update the mapping table** with the crate path → Guide page.

4. **Update the "What to Flag" list** if the new crate introduces a new
   category of reviewable change.

---

## Adding or Changing CLI Arguments

When `openvmm/openvmm_entry/src/cli_args.rs` changes:

1. Update `Guide/src/reference/openvmm/management/cli.md`
2. The CLI page has a disclaimer that it may be out of date — keep it honest,
   but still update the page when making changes
3. New arguments should include: flag syntax, description, default value,
   and any interactions with other flags

---

## Auditing Guide Freshness

To check whether the Guide is in sync with the code:

1. **Scan the mapping table** in this file
2. For each row, check whether the Guide page content still matches the code:
   - Crate names still correct?
   - Struct/enum names still exist?
   - Behavioral descriptions still accurate?
   - Links to rustdoc still resolve?
3. Check `Guide/src/SUMMARY.md` for placeholder links `()` — these are
   topics that need content. Prioritize ones whose code areas have been
   actively developed.

### Quick audit commands

```bash
# Find Guide pages that reference a crate name
grep -r "crate_name" Guide/src/

# Find placeholder topics (empty links) in SUMMARY.md
grep '()\s*$' Guide/src/SUMMARY.md

# Find code crates with no Guide mapping
# Compare vm/devices/*/Cargo.toml crate names against the mapping table
```

---

## Mapping Table Format

Each row in the mapping table follows:

```
| `code/path/` | `guide/path.md` |
```

- Code paths use repo-root-relative paths with trailing `/` for directories
- Guide paths are relative to `Guide/src/`
- Multiple Guide pages for one code path: comma-separated
- Use glob-style `*` in code paths for crate families (e.g., `nvme*/`)
