# cargo xtask

`cargo xtask` is OpenVMM's "swiss army knife" Rust binary that houses various
bits of project specific tooling.

For more info on how `xtask` is different from `xflowey`, see [`xflowey` vs
`xtask`](./xflowey.md#xflowey-vs-xtask).

Some examples of tools that you can find under `xtask`:

- `cargo xtask fmt` implements various OpenVMM-specific style / linting rules
- `cargo xtask fuzz` implements various OpenVMM-specific `cargo fuzz` extensions
- `cargo xtask install-git-hooks` sets up git hooks for developers

This list is not exhaustive. Running `cargo xtask` will list what tools are
available, along with brief descriptions of what they do / how to use them.

For more information of the `xtask` pattern, see <https://github.com/matklad/cargo-xtask>

## Shell Completions

The xtask tool supports shell completions to help with command discovery and argument completion. Two types of completions are available:

### Static Completions (Recommended)

Static completions are pre-generated and provide fast completion for all available commands. These work well for most development workflows:

**Bash:**
```bash
# Generate and save completions to your user completion directory
mkdir -p ~/.local/share/bash-completion/completions
cargo xtask generate-completions bash > ~/.local/share/bash-completion/completions/xtask

# Or add to your bash profile for immediate loading
echo 'eval "$(cargo xtask generate-completions bash)"' >> ~/.bashrc
```

**PowerShell:**
```powershell
# Add to your PowerShell profile for persistent completions
cargo xtask generate-completions powershell >> $PROFILE

# Or create a temporary completion for the current session
cargo xtask generate-completions powershell | Invoke-Expression
```

### Dynamic Completions (Advanced)

Dynamic completions provide enhanced, context-aware completion with runtime intelligence. These are useful for advanced workflows that need custom completion logic:

**Bash:**
```bash
# Note: Dynamic completions for bash work through fish/zsh integration
# Most users should use static completions above
```

**PowerShell:**
```powershell
# Set up dynamic completions (requires additional setup)
cargo xtask completions powershell > xtask-dynamic.ps1
# Follow the instructions in the generated file
```

### Available Shells

**Static completions** (`generate-completions`): bash, elvish, fish, powershell, zsh
**Dynamic completions** (`completions`): fish, powershell, zsh

### Testing Completions

After setting up completions, test them by typing `cargo xtask ` and pressing Tab to see available commands and options.
