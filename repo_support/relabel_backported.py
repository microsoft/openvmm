#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# This script is used to relabel PRs that have been backported to a release
# branch, from the backport_<release> label to the backported_<release> label.

import re
import json
import subprocess
import argparse
import sys


def get_upstream_repo():
    """Get the upstream repo name (owner/repo) using gh CLI.
    
    If the current repo is a fork, returns the parent. Otherwise returns
    the current repo (assuming it's the upstream itself).
    """
    try:
        result = subprocess.check_output(
            ['gh', 'repo', 'view', '--json', 'parent,nameWithOwner'],
            stderr=subprocess.DEVNULL
        ).decode('utf-8')
        data = json.loads(result)
        # If this repo has a parent, use that as upstream
        if data.get('parent'):
            return data['parent']['owner']['login'] + '/' + data['parent']['name']
        # Otherwise, this repo is the upstream
        return data.get('nameWithOwner')
    except (subprocess.CalledProcessError, json.JSONDecodeError, KeyError):
        return None


def detect_upstream_remote(upstream_repo):
    """Find which git remote points to the given upstream repo."""
    if not upstream_repo:
        return None
    
    # Patterns to match the upstream repo in remote URLs
    # Handles both HTTPS (github.com/owner/repo) and SSH (github.com:owner/repo)
    patterns = [
        f'github.com/{upstream_repo}',
        f'github.com:{upstream_repo}',
    ]
    
    try:
        remotes = subprocess.check_output(
            ['git', 'remote', '-v'], stderr=subprocess.DEVNULL
        ).decode('utf-8').splitlines()
    except subprocess.CalledProcessError:
        return None

    for line in remotes:
        parts = line.split()
        if len(parts) >= 2:
            remote_name = parts[0]
            remote_url = parts[1]
            for pattern in patterns:
                if pattern in remote_url:
                    return remote_name
    return None


parser = argparse.ArgumentParser()
parser.add_argument('--update', action='store_true',
                    help='Relabel the PRs that have been backported')
parser.add_argument('--force-update-pr', action='append',
                    help='Force relabel specific PRs even if their backport PR title does not match')
parser.add_argument('--remote', type=str, default=None,
                    help='Git remote to use for release branches (auto-detected if not specified)')
parser.add_argument('--no-fetch', action='store_true',
                    help='Skip fetching from the remote before scanning')
# Get the release name as the first non-flag argument.
parser.add_argument('release', type=str,
                    help='The release to scan for backports')
args = parser.parse_args()
update = args.update
release = args.release
force_update_pr = args.force_update_pr or []

# Detect or use specified remote
if args.remote:
    remote = args.remote
else:
    upstream_repo = get_upstream_repo()
    if upstream_repo:
        print(f"Detected upstream repo: {upstream_repo}")
    remote = detect_upstream_remote(upstream_repo)
    if not remote:
        print("Error: Could not detect upstream remote.", file=sys.stderr)
        print("Please specify --remote explicitly.", file=sys.stderr)
        sys.exit(1)
    print(f"Using remote: {remote}")

# Fetch from remote unless --no-fetch is specified
if not args.no_fetch:
    print(f"Fetching from {remote}...")
    try:
        subprocess.check_call(['git', 'fetch', remote])
    except subprocess.CalledProcessError as e:
        print(f"Error: Failed to fetch from {remote}: {e}", file=sys.stderr)
        sys.exit(1)

# Get the list of PRs to backport by the backport_<release> label.
prs = subprocess.check_output(
    ['gh', 'pr', 'list',
     '--limit', '10000',
     '--base', 'main',
     '--state', 'merged',
     '--label', f'backport_{release}',
     '--json', 'title,url,number']
)
prs = json.loads(prs)
prs = {str(pr['number']): (pr['title'], pr['url']) for pr in prs}

# Look for commits in the release branch that mention the PRs by number, URL, or
# title.
backported_prs = {}
for pr, (title, url) in prs.items():
    title_for_regex = re.escape(re.sub(r' (\(#\d+\))+$', '', title))
    commits = subprocess.check_output(
        ['git', 'log',
         f'{remote}/release/{release}',
         '--oneline',
         '-E',
         f'--grep=(#{pr}\\b)|(github.com/microsoft/openvmm/pull/{pr}\\b)|({title_for_regex})']
    ).decode('utf-8').split('\n')
    commits = [commit for commit in commits if commit]
    if commits:
        backported_prs[pr] = commits

for pr, backports in backported_prs.items():
    (title, url) = prs[pr]
    print(f'{title}')
    print(f'{url}')
    # Print the backport commit if the commit message does not contain the original PR title, otherwise add
    # a comment to the PR, add the label, and remove the backport label.
    backport_pr = None
    bad = False
    print(f'Backports:')
    for backport in backports:
        print(f'  {backport}')
        if not title in backport:
            print(f"    WARN: maybe mismatched, won't update by default")
            bad = True

        # Find the last PR number in the commit title, since that is
        # conventionally the backport PR.
        maybe_backport_pr = re.findall(r'#([0-9]+)', backport)[-1]
        if maybe_backport_pr != pr:
            print(
                f'    https://github.com/microsoft/openvmm/pull/{maybe_backport_pr}')
            backport_pr = maybe_backport_pr

    if backport_pr and (not bad or pr in force_update_pr):
        if update:
            subprocess.check_output(
                ['gh', 'pr', 'comment', pr,
                 '-b', f'Backported to [release/{release}](https://github.com/microsoft/openvmm/tree/release/{release}) in #{backport_pr}']
            )
            subprocess.check_output(
                ['gh', 'pr', 'edit', pr,
                 '--add-label', f'backported_{release}',
                 '--remove-label', f'backport_{release}']
            )
            print("updated")

    print()
