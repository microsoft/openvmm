# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import click
from git import Repo
from github import Github
from github import Auth

@click.command()
@click.argument('repo_path', required=True)
@click.argument('target_branch', required=True)
@click.option('--token', default=None)
@click.option('--pull-request', default=None)
@click.option('--team', default='@microsoft/openvmm-unsafe-approvers')
def main(repo_path: str, target_branch: str, token: str, pull_request: str, team: str):
    def contains_unsafe(change) -> bool:
        if change.change_type not in ['A', 'M'] or not change.a_path.endswith('.rs'):
            return False

        with open(f'{repo_path}/{change.a_path}') as fd:
            content = fd.read()
            return 'unsafe ' in content or 'unsafe(' in content

    # Look for modified / added files
    repo = Repo(repo_path)
    changed_file_unsafe = [e.a_path for e in repo.commit(target_branch).diff(None) if contains_unsafe(e)]

    api = Github(auth=Auth.Token(token))
    pull_request = api.get_repo('microsoft/openvmm').get_pull(int(pull_request))
    if changed_file_unsafe:
        print(f'Unsafe review triggered by changes in: {",".join(changed_file_unsafe)}')

        # N.B. If a member of the required reviewer team has reviewed the PR, this team member will replace
        #      the required reviewer in the list of review requests. This means that the required reviewer
        #      team will be re-added when a new iteration is pushed.
        #      This behavior is discussed in detail here: https://github.com/orgs/community/discussions/5289
        if any(review_team.slug.lower() == team.lower() for review_team in pull_request.get_review_requests()[1]):
            print(f'{team} is already present on the pull request')
        else:
            pull_request.create_review_request(team_reviewers=[team])
    else:
        print(f'No unsafe file modified in this change')
        pull_request.delete_review_request(reviewers=[], team_reviewers=[team])

if __name__ == '__main__':
    main()
