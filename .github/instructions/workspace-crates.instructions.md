---
applyTo: "**/Cargo.toml"
---

# Adding New Crates to the Workspace

`cargo xtask fmt --fix` enforces workspace consistency and **removes unused
entries**. This has important implications when creating new crates.

## Key rules

1. **Workspace dependencies are auto-cleaned.** The `[workspace.dependencies]`
   section in the root `Cargo.toml` only lists crates that are actually
   depended on by at least one workspace member. If you add an entry there
   that nothing uses, `cargo xtask fmt --fix` will remove it.

2. **Unused crate dependencies are auto-cleaned.** If you add a dependency
   to an existing crate's `Cargo.toml` but never `use` it in code,
   `cargo xtask fmt --fix` will remove that dependency too.

3. **To add a new standalone crate** (binary, fuzz target, tool, or a library
   still being built), you **must** add its path to the `[workspace] members`
   list in the root `Cargo.toml`. This is the only way to ensure the crate
   stays in the workspace before anything else depends on it.

4. **Library crates that are dependencies of other members** do not need to be
   in `members` — they are discovered transitively. Only add a library crate
   to `members` temporarily while bootstrapping it (before any other crate
   depends on it). Remove it from `members` once another member has a real
   dependency on it.

## Step-by-step: adding a new crate

1. Create the crate directory with `Cargo.toml` and `src/` as usual.
2. In the crate's `Cargo.toml`, use `dep_name.workspace = true` for any
   dependencies that already exist in `[workspace.dependencies]`.
3. **If the crate is a binary, tool, fuzz target, or a library with no
   consumer yet**, add its path to `[workspace] members` in the root
   `Cargo.toml`. This is the only way to keep it in the workspace.
4. **If the crate is a library that another member will depend on**, skip
   `members`. Instead, add the crate to `[workspace.dependencies]` in the
   root `Cargo.toml` with a `path = "..."` entry, then add
   `your_crate.workspace = true` to the consuming crate's `[dependencies]`.
   The new crate will be discovered transitively.
5. Run `cargo xtask fmt --fix` — it may reorder entries or remove anything
   that is genuinely unused.

## Common mistakes

- **Adding a workspace dependency entry without a real consumer.** The entry
  will be silently removed by `cargo xtask fmt --fix`.
- **Adding a crate dependency without using it in code.** Same result —
  silently removed.
- **Forgetting to add a standalone crate to `members`.** Without a `members`
  entry or a dependency from another member, the crate will not be compiled,
  tested, or checked by CI.
