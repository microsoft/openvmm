# Code Review Metrics

Automated workflow that tracks code review activity in the OpenVMM repository.

## What It Tracks

- **Active Reviewers**: Who is reviewing code and their review counts
- **Review Volume**: Number of reviews given and PRs reviewed per person
- **Review Gaps**: Contributors who submit PRs but don't participate in reviews
- **Participation Rate**: Percentage of contributors who also review code

## Schedule

- **Automatic**: Runs weekly on Mondays at midnight UTC
- **Manual**: Trigger via Actions → Code Review Metrics → Run workflow

## Configuration

Manual runs can specify a custom analysis period (default: 30 days).

## Output

Reports include:
- Reviewer activity table with review counts
- Contributors not participating in reviews
- Key insights and participation statistics

Artifacts are retained for 90 days in GitHub Actions.
