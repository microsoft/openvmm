# Release Management

The OpenVMM repository periodically creates releases from `main`, typically
about once a month. Releases are named for the year and month in which they
were forked, using `YYMM`. For example, a release forked in September 2026 is
release `2609`.

Each monthly release is represented by a branch named `release/<YYMM>`, such
as `release/2609`. The branch remains available as the release baseline and
receives backported fixes if that release needs stabilization or servicing.

The repository produces both the OpenVMM VMM and the OpenHCL paravisor.
OpenVMM and OpenHCL retain their own product versions; the `YYMM` release
identifies their repository baseline. OpenHCL currently ships about twice a
year and selects one of the repository releases as the baseline for each
product release.

Before publishing an OpenVMM source release from `main` or a release branch,
set the workspace version to a new, globally unique SemVer in a normal pull
request. A new feature release from `main` typically increments the minor
version, such as `1.1.0` to `1.2.0`. A servicing release from a release branch
increments the patch version, such as `1.1.0` to `1.1.1`. The same workspace
version may exist in multiple branches during development, but it can be
published as an `openvmm-v<VERSION>` release only once. Continuing development
or creating a release branch does not by itself require a version bump. See
[Cutting an OpenVMM source release](openvmm_source_release.md) for the
publication process.

We expect a high quality bar for all code that goes into the OpenVMM main
branch, and we ask developers to hold release branches to the highest quality
standards. The OpenVMM maintainers will gradually slow the rate of churn into
these branches as we get closer to a close date.

```admonish note title="See also"
[Security Releases](security_releases.md) describes private reporting and
coordinated disclosure for fixes that cannot be developed publicly.
```

```admonish note
Some existing release branches use the older
`release/<MAJOR>.<MINOR>.<YYMM>` format, such as `release/1.8.2607`.
```

This process should not impact your typical workflow; all new work should go
into the `main` branch. But, to ease the cherry-picks, we may ask that you hold
off from making breaking or large refactoring changes at points in this
process.

## Marking, Approval Process, Code Flow

The OpenVMM maintainers will publish various dates for the upcoming releases.
Currently, these dates are driven by a Microsoft-internal process and can, and
do, often change. Microsoft does not mean to convey any new product launches by
choices of these dates.

Releases naturally fall into several phases:

| Phase              | Meaning                                                                                                                        |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| Active Development | Regular development phase where new features and fixes are added.                                                              |
| Stabilization      | Phase focused on stabilizing the release by fixing bugs.                                                                       |
| Ask Mode           | Only critical fixes are allowed; changes are scrutinized. No new features. This is the last phase before a release is closed.  |
| Servicing          | Only essential fixes are made to support the release (a.k.a. maintenance mode).                                                |
| Out of service     | A previous release which is no longer receiving updates.                                                                       |

By default, a monthly release branch receives short-term servicing until the
next monthly release supersedes it. Branches selected for an OpenHCL product
release receive extended servicing.

For OpenHCL, the plan of record is to service the current product release and
the two preceding product releases. Because OpenHCL currently releases about
twice a year, this is approximately 18 months of servicing. The release-count
policy is authoritative if the release cadence changes. When a new OpenHCL
release ships, the release that is now three product releases behind moves out
of service.

### Release branch process

In the label and tooling examples below, `<RELEASE>` is the suffix of the
release branch. For `release/2609`, `<RELEASE>` is `2609`.

When creating a monthly release branch:

1. Create the `release_<RELEASE>`, `backport_<RELEASE>`, and
   `backported_<RELEASE>` labels in GitHub.
2. Add the `release_<RELEASE>` base-branch rule to `.github/labeler.yml` so pull
   requests targeting the branch are labeled consistently.

We track the state of candidates for a given release by tagging the PRs with the following labels:

* `backport_<RELEASE>`: This PR (to `main`) is a candidate to be included in the release.
  * N.B.: A maintainer will _remove_ this tag if the fix is not accepted into the release.
* `backported_<RELEASE>`: This PR (to `main`) has been cherry-picked to the release branch.

The [`repo_support/relabel_backported.py`](https://github.com/microsoft/openvmm/blob/main/repo_support/relabel_backported.py) script can be used to automatically transition PRs from `backport_<RELEASE>` to `backported_<RELEASE>` once they have been cherry-picked to the release branch.

#### Seeking Approval for Backport

To seek approval to include a change in a release branch, follow these steps:

1. Tag your PR to `main` with the `backport_<RELEASE>` label.
2. Wait for the PR to be merged to `main`.
3. Cherry-pick the change to the appropriate release branch in your fork and
   stage a PR to that same branch in the main repository.

Please reach out to the maintainers before staging that PR if you have any
doubts.

#### Backport PR Best Practices

When creating a backport PR to a release branch:

* **Clean cherry-picks are strongly preferred.** A clean cherry-pick minimizes
  reviewer effort and reduces the risk of introducing regressions.
* **If the backport is not a clean cherry-pick** (e.g., requires conflict
  resolution or additional modifications), clearly indicate this in the PR
  description. This signals to the reviewer that extra care is needed during
  the review process.
  
## Existing Release Branches

| Release          | Phase              | Notes                                                                                |
| ---------------- | ------------------ | ------------------------------------------------------------------------------------ |
| release/2411     | Out of service     |                                                                                      |
| release/2505     | Out of service     | Supports runtime servicing from release/2411.                                        |
| release/1.7.2511 | Servicing          | Supports runtime servicing from release/2411 and release/2505.                       |
| release/1.8.2607 | Ask Mode           | Supports runtime servicing from release/2411, release/2505, and release/1.7.2511.    |
| _tbd, in main_   | Active Development | Supports runtime servicing from release/2411, release/2505, and release/1.7.2511.    |

## Taking a Dependency on a Release

We welcome feedback, especially if you would like to depend on a reliable
release process. Please reach out!
