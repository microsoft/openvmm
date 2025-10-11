# Code Review Metrics

This workflow tracks code reviewer activity to help identify who is reviewing code and encourage broader participation.

## What It Does

- Tracks who is reviewing code and how many reviews each person does
- Identifies contributors who submit code but don't participate in reviews
- Runs automatically every Monday at midnight UTC
- Excludes bot accounts from analysis

## Usage

**Automatic:** Reports generated weekly and saved as GitHub Actions artifacts

**Manual:** Go to Actions → Code Review Metrics → Run workflow (default 30 days, configurable)

## Reports Include

- Active reviewers with review counts and PRs reviewed
- Contributors not participating in reviews
- Review participation statistics

Reports are available in the GitHub Actions artifacts section with 90-day retention.
