---
name: guide-maintenance
description: >
  Maintain the OpenVMM Guide. Load when: (1) adding, removing, or moving Guide
  pages, (2) adding new device crates or adding or changing CLI interfaces
  that need doc coverage, or (3) auditing Guide freshness against code changes.
---

# Guide Maintenance Skill

Procedures for keeping the OpenVMM Guide (`Guide/src/`) in sync with the codebase.

`.github/instructions/doc-code-sync.instructions.md` contains the documentation
relevance rules and heuristics for changes to `*.rs` and `Cargo.toml` files.

Apply that file's relevance and scope rules before editing the Guide. A code
change does not automatically require a Guide update. Prefer an existing page
over a new page when it adequately covers the reader's need.

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

---

## Removing or Moving a Guide Page

1. Update or remove the entry in `Guide/src/SUMMARY.md`
2. Check for cross-references from other Guide pages (grep for the old path)
3. If moving: ensure the SUMMARY.md link points to the new location

---

## Adding a New Device Crate

When a new crate is added under `vm/devices/`:

1. **Do readers need new documentation?** For a user-visible device or backend,
   document the capability and how to use it. Internal plumbing crates (e.g.,
   `*_resources`, `*_protocol`) do not need a page or a mention in the parent
   page unless they explain a reader-relevant constraint or architectural
   choice.

2. **Update an existing page** if it adequately covers the topic. Otherwise,
   follow the "Adding a New Guide Page" procedure above.

---

## Adding or Changing CLI Arguments

When the documented CLI interface changes (for example, flag syntax,
defaults, behavior, or interactions in
`openvmm/openvmm_entry/src/cli_args.rs`):

1. Update `Guide/src/reference/openvmm/management/cli.md`
2. Verify flag syntax, description, default value, and relevant interactions
   against the parser and generated help. Include what readers need to use
   the interface correctly.

Refactors that leave the documented interface unchanged need no CLI page edit.

---

## Auditing Guide Freshness

Use this procedure for an explicitly requested freshness audit. Limit scans
and placeholder checks to the requested scope; do not expand a routine code
or paragraph edit into an audit.

1. **Scan the relevant Guide pages** in `Guide/src/`
2. Check whether the Guide page content still matches the code:
   - Crate names still correct?
   - Struct/enum names still exist?
   - Behavioral descriptions still accurate?
   - Links to rustdoc still resolve?
3. Check `Guide/src/SUMMARY.md` for placeholder links `()` related to the audit.
   Report relevant gaps; fill them only when the requested scope includes
   writing the missing content.

### Quick audit commands

```bash
# Find Guide pages that reference a crate name
grep -r "crate_name" Guide/src/

# Find placeholder topics (empty links) in SUMMARY.md
grep '()\s*$' Guide/src/SUMMARY.md
```
