# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""
Centralized branch configuration for OpenVMM repository.
This file provides a single source of truth for branch names used across
scripts and configuration files.
"""

# Main development branch
MAIN_BRANCH = "main"

# Release branch pattern for GitHub triggers
RELEASE_BRANCH_PATTERN = "release/*"

# Current active release branches
CURRENT_RELEASE_BRANCH_2411 = "release/2411"
CURRENT_RELEASE_BRANCH_2505 = "release/2505"

# List of all current release branches for easy iteration
CURRENT_RELEASE_BRANCHES = [
    CURRENT_RELEASE_BRANCH_2411,
    CURRENT_RELEASE_BRANCH_2505,
]