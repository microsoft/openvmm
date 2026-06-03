# Copilot Agent Instructions

You are fixing GitHub issue #3463 in microsoft/openvmm.

## Task

Read AGENT_TASK.md for the full issue description and instructions.

## Constraints

- Make the SMALLEST possible fix.
- Do NOT push, create PRs, or modify git config.
- Do NOT refactor unrelated code.
- Follow existing code style.
- Run the project's linter on changed files and fix lint errors you introduced.
- Run relevant tests and ensure they pass.
- Write results to AGENT_RESULT.md when done with sections:
  - ## Root Cause
  - ## Change Made
  - ## Testing
  - ## Lint


## Repo-Specific Requirements

- Language: rust
- Commit style: conventional commits (prefixes: crypto, mesh_process, net_consomme, openhcl snp, openvmm_entry, pcie, virt_kvm, vmm_tests)
