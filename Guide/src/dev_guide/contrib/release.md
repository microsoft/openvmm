# Release Management

Occasionally, the OpenVMM project will declare upcoming release milestones. We
stabilize the code base in a `release/YYMM` branch, typically named for the
YYMM when the branch was forked. We expect a high quality bar for all code that
goes in to the OpenVMM main branch, we ask developers to hold these
`release/YYMM` to the highest quality standards. The OpenVMM maintainers will
gradually slow the rate of churn into these branches as we get closer to a
close date.

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

| Phase             | Meaning                                                                 |
|-------------------|-------------------------------------------------------------------------|
| Active Development| Regular development phase where new features and fixes are added.       |
| Stabilization     | Phase focused on stabilizing the release by fixing bugs.                |
| Ask Mode          | Changes are scrutinized and only critical fixes are allowed. No new features are accepted. This is the last phase before a release is closed. |
| Servicing         | Only essential fixes are made to support the release. a.k.a. maintenance mode      |
| Out of service    | A previous release, which is no longer receiving updates. |

### Release branch process

We track the state of candidates for a given release by tagging the PRs with the following labels:

* `backport_YYMM`: This PR (to `main`) is a candidate to be included in the
  `YYMM` release.
  * N.B.: A maintainer will _remove_ this tag if the fix is not accepted into
    the release.
* `backported_YYMM`: This PR (to `main`) has been cherry-picked to the `YYMM`
  release.

#### Seeking Approval for Backport

To seek approval to include a change in a release branch, follow these steps:

* Tag your PR to `main` PR with the `backport_YYMM` label.
* Cherry-pick the change to the appropriate `release/YYMM` branch in your fork
  and stage a PR to that same branch in the main repository.

Please reach out to the maintainers before staging that PR if you have any
doubts.

#### Backport PR Best Practices

When creating a backport PR to a `release/YYMM` branch:

* **Clean cherry-picks are strongly preferred.** A clean cherry-pick minimizes
  reviewer effort and reduces the risk of introducing regressions.
* **If the backport is not a clean cherry-pick** (e.g., requires conflict
  resolution or additional modifications), clearly indicate this in the PR
  description. This signals to the reviewer that extra care is needed during
  the review process.

#### Updating Servicing Tests

After a release branch is created and produces at least one release candidate (i.e., a successful CI run from the release branch), the servicing upgrade and downgrade tests in the `main` branch should be updated to use the new OpenHCL binaries from that release. These tests verify that future changes in `main` maintain backward and forward compatibility with released versions. They ensure that the servicing functionality (upgrading from older releases or downgrading to them) continues to work correctly as the codebase evolves.
Once the release branch has a successful CI build that produces OpenHCL binaries. See [PR #2460](https://github.com/microsoft/openvmm/pull/2460) for a reference implementation of this update.

## Existing Release Branches

| Release | Phase | Notes |
|--------|-------|-------|
| release/2411 | Out of service | |
| release/2505 | Servicing | Supports runtime servicing from release/2411. |
| _tbd, in main_ | Active Development | Supports runtime servicing from release/2411 and release/2505. |

## Taking a Dependency on a Release

We welcome feedback, especially if you would like to depend on a reliable
release process. Please reach out!
