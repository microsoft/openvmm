# Style Guide

This page defines the writing conventions for the OpenVMM developer guide.
Follow these standards when creating or editing any page.

## Voice and tone

Write as if explaining to a smart colleague who is new to the project.
Use second person ("you") for instructions: "To build OpenHCL, run..." not
"One can build OpenHCL by running..." State facts. Avoid hedging ("I believe",
"some reason", "maybe we can"). If something is uncertain, say "As of
[date], verify that..."

## Page structure

Open every page with a one-sentence summary of what it covers. Use heading
levels consistently: `# Page Title`, `## Major Section`, `### Subsection`.
Keep pages under 400 lines. If a page grows beyond that, consider splitting
it into sub-pages.

## Callouts

This guide uses
[mdbook-admonish](https://github.com/tommilligan/mdbook-admonish) for
callouts. Use fenced code block syntax with the `admonish` keyword:

~~~markdown
```admonish note
Supplementary information the reader should be aware of.
```

```admonish tip
Helpful suggestions or shortcuts.
```

```admonish warning
Potential pitfalls or easy mistakes.
```

```admonish danger
Actions that could cause data loss or security issues.
```
~~~

You can add a custom title with the `title` attribute:

~~~markdown
```admonish warning title="Breaking change"
This API was removed in v1.8.
```
~~~

For cross-links to related pages, use a **See also** callout:

~~~markdown
```admonish note title="See also"
[Debugging OpenHCL](../../reference/openhcl/debugging.md) for serial logs,
crash dumps, and diagnostic tools.
```
~~~

## Code blocks

### Shell labeling

Always specify the language. Never leave a code fence unlabeled.

| Label | When |
|-------|------|
| `bash` | Linux / WSL / macOS shell commands |
| `powershell` | PowerShell commands and scripts |
| `rust` | Rust source code |
| `toml` | TOML configuration |
| `json` | JSON data |
| `text` | Plain text output, logs, or paths |

### Placeholders

Two conventions are used for placeholder values. Use whichever is clearest
in context:

- **`path/to/...` style** — for file paths where the structure is obvious.
  This is the predominant style in the existing guide.
  ```bash
  cargo run -- --uefi --disk memdiff:file:path/to/disk.vhdx
  ```

- **`<SCREAMING_SNAKE_CASE>` style** — for values that need extra emphasis or
  where the expected format isn't obvious (build numbers, VM names, keys).
  ```powershell
  Set-VMComPort -VMName <VM_NAME> -Number 3 -Path \\.\pipe\<PIPE_NAME>
  ```

Bad (hardcoded path that won't work for the reader):

~~~bash
cargo run -- --uefi --disk memdiff:file:/home/alice/disks/myvm.vhdx
~~~

### File paths in code examples

Use **forward slashes** (Unix-style) for file paths in `bash` code blocks,
even when the path refers to a Windows filesystem location accessed via WSL:

```bash
# Good — forward slashes in bash
cargo run -- --disk memdiff:file:path/to/disk.vhdx

# Good — wslpath output uses backslashes, but that's a Windows path
cargo run -- --disk "memdiff:file:$(wslpath -w /mnt/c/vhds/disk.vhdx)"
```

Use **backslashes** for paths in `powershell` code blocks:

```powershell
# Good — backslashes in PowerShell
cargo run -- --disk memdiff:file:C:\vhds\disk.vhdx
```

### Length

Keep code blocks under 30 lines. If longer, split with explanatory text
between blocks. Comments inside code blocks should explain *why*, not *what*.

## Links

Use relative links to other guide pages. When linking to external
documentation, prefer stable URLs (Microsoft Learn, GitHub, official docs)
over internal wikis or SharePoint.
