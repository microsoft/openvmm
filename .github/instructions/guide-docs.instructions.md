---
applyTo: "Guide/**"
---

# Guide Documentation Style

Follow the style guide at `Guide/src/dev_guide/contrib/style_guide.md`.

## Authoring Rules

- Use **mdbook-admonish** syntax for callouts:
  `` ```admonish note ``, `` ```admonish warning ``, `` ```admonish tip ``
  — do **NOT** use docfx `> [!NOTE]` syntax
- Label all code fences with the language (`bash`, `powershell`, `rust`, etc.)
- Keep code blocks under 30 lines; split with explanatory text
- Wrap lines at 80 characters
- Use `path/to/...` placeholders for file paths, or
  `<SCREAMING_SNAKE_CASE>` for non-obvious values (VM names, build numbers)

## Content Policy

- Apply `.github/instructions/doc-code-sync.instructions.md` to decide whether
  a Guide edit is needed and keep it limited to the reader's needs.
- Do not duplicate rustdoc or source-level detail. Include implementation
  detail only when it explains a reader-relevant constraint or stable
  architectural choice.
- This is an **OSS repo** — do not reference internal Microsoft tools, wikis,
  build paths, or infrastructure
- Link to crate rustdoc where possible (https://openvmm.dev/rustdoc/)
- When referencing code, use backtick crate names or relative paths from
  the repo root

## Structure

- Every new page must be added to `Guide/src/SUMMARY.md`
- Use the existing hierarchy: user_guide → dev_guide → reference
- Reference pages live under `reference/` organized by component type
  (emulated, vmbus, backends, architecture)
