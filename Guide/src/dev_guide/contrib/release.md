# OpenHCL Release Management

This page describes the OpenHCL release branch, stabilization, and servicing
process.

```admonish note title="See also"
[OpenVMM Release Model and Support](./openvmm_release.md) describes standalone
OpenVMM releases. OpenVMM tags and versions are independent from the OpenHCL
release branches described below.
```

```admonish warning
The `release/*` branches on this page service OpenHCL. They are not standalone
OpenVMM release branches. Pre-1.0 OpenVMM fixes ship from `main` in the next
rolling OpenVMM release instead of being backported here.
```
Occasionally, the OpenHCL project will declare upcoming release milestones. We
Occasionally, the OpenHCL project will declare upcoming release milestones. We
stabilize the code base in a `release/<MAJOR>.<MINOR>.<YYMM>` branch, typically
named for the YYMM when the branch was forked. Future references to the release
number will be shortened to `<RELEASE>` in this doc. We expect a high quality
bar for all code that goes into the repository's main branch, and we ask
developers to hold these release branches to the highest quality standards. The
OpenHCL maintainers will gradually slow the rate of churn into these branches as
we get closer to a close date.

```admonish note
Some older release branches use the format `release/<YYMM>` without the major
and minor version numbers, such as `release/2411` and `release/2505`.
```

This process should not impact your typical workflow; all new work should go
into the `main` branch. But, to ease the cherry-picks, we may ask that you hold
off from making breaking or large refactoring changes at points in this
process.

## Marking, Approval Process, Code Flow

The OpenHCL maintainers will publish various dates for upcoming releases.
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

### Release branch process

We track the state of candidates for a given release by tagging the PRs with the following labels:

* `backport_<RELEASE>`: This PR (to `main`) is a candidate to be included in the release.
  * N.B.: A maintainer will _remove_ this tag if the fix is not accepted into the release.
* `backported_<RELEASE>`: This PR (to `main`) has been cherry-picked to the release branch.

The [`repo_support/relabel_backported.py`][relabel-backported] script can
automatically transition PRs from `backport_<RELEASE>` to
`backported_<RELEASE>` once they have been cherry-picked to the release branch.

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
| release/1.8.2607 | Stabilization      | Supports runtime servicing from release/2411, release/2505, and release/1.7.2511.    |
| _tbd, in main_   | Active Development | Supports runtime servicing from release/2411, release/2505, and release/1.7.2511.    |

## Taking a Dependency on a Release

We welcome feedback, especially if you would like to depend on a reliable
release process. Please reach out!

[relabel-backported]: https://github.com/microsoft/openvmm/blob/main/repo_support/relabel_backported.py
