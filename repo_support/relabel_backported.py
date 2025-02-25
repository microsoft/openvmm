#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# This script is used to relabel PRs that have been backported to a release
# branch, from the backport_<release> label to the backported_<release> label.

import re
import json
import subprocess
import argparse
parser = argparse.ArgumentParser()
parser.add_argument('--update', action='store_true',
                    help='Relabel the PRs that have been backported')
parser.add_argument('--force-update-pr', action='append',
                    help='Force relabel specific PRs even if their backport PR title does not match')
# Get the release name as the first non-flag argument.
parser.add_argument('release', type=str,
                    help='The release to scan for backports')
args = parser.parse_args()
update = args.update
release = args.release
force_update_pr = args.force_update_pr or []

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
         f'origin/release/{release}',
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
                 '-b', f'Backported in #{backport_pr}']
            )
            subprocess.check_output(
                ['gh', 'pr', 'edit', pr,
                 '--add-label', f'backported_{release}',
                 '--remove-label', f'backport_{release}']
            )
            print("updated")

    print()
