# Branch Configuration

This directory contains centralized branch name configuration for the OpenVMM repository.

## Files

- `branch_config.py` - Python module containing branch name constants
- `generate_labeler.py` - Script to generate GitHub labeler.yml from template

## Branch Name Constants

Branch names are centrally configured in two places:

1. **Rust code**: `flowey/flowey_lib_hvlite/src/_jobs/cfg_versions.rs`
   - Used by flowey pipeline configurations
   - Contains constants like `MAIN_BRANCH`, `RELEASE_BRANCH_PATTERN`, etc.

2. **Python scripts**: `repo_support/branch_config.py`
   - Used by Python scripts and GitHub configuration generation
   - Contains the same branch name constants

## Updating Branch Names

When creating a new release branch:

1. Update constants in `flowey/flowey_lib_hvlite/src/_jobs/cfg_versions.rs`
2. Update constants in `repo_support/branch_config.py`
3. Update the release branch table in `Guide/src/dev_guide/contrib/release.md`
4. Regenerate GitHub labeler configuration: `python .github/scripts/generate_labeler.py`

## Files That Use These Constants

- Flowey pipeline configurations (`flowey/flowey_hvlite/src/pipelines/`)
- OpenHCL kernel package downloads
- GitHub refresh mirror script
- GitHub labeler configuration (via template generation)
- Documentation