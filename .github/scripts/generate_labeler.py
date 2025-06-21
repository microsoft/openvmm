#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""
Generate GitHub labeler.yml from template using centralized branch configuration.
"""

import os
import sys

# Add repo_support to path to import branch_config
script_dir = os.path.dirname(os.path.abspath(__file__))
repo_root = os.path.dirname(os.path.dirname(script_dir))
sys.path.insert(0, os.path.join(repo_root, 'repo_support'))

import branch_config

def generate_labeler_yml():
    """Generate labeler.yml from template using branch configuration."""
    template_path = os.path.join(os.path.dirname(script_dir), 'labeler.yml.template')
    output_path = os.path.join(os.path.dirname(script_dir), 'labeler.yml')
    
    # Read template
    with open(template_path, 'r') as f:
        template_content = f.read()
    
    # Replace placeholders with actual branch names
    content = template_content.replace('{{CURRENT_RELEASE_BRANCH_2411}}', branch_config.CURRENT_RELEASE_BRANCH_2411)
    content = content.replace('{{CURRENT_RELEASE_BRANCH_2505}}', branch_config.CURRENT_RELEASE_BRANCH_2505)
    
    # Write output
    with open(output_path, 'w') as f:
        f.write(content)
    
    print(f"Generated {output_path} from {template_path}")

if __name__ == '__main__':
    generate_labeler_yml()