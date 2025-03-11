# Release Management

 Occasionally, the OpenVMM project will declare upcoming release milestones. We stabilize the code base in a `release/YYMM` branch, typically named for the YYMM when the branch was forked. We expect a high quality bar for all code that goes in to the OpenVMM main branch, we ask developers to hold these `release/YYMM` to the highest quality standards. The OpenVMM maintainers will gradually slow the rate of churn into these branches as we get closer to a close date.

 This process should not impact your typical workflow; all new work should go into the `main` branch. But, to ease the cherry-picks, we may ask that you hold off from making breaking or large refactoring changes at points in this process.

## Marking, Approval Process, Code Flow

The OpenVMM maintainers will publish various dates for the upcoming releases. Currently, these dates are driven by a Microsoft-internal process and can, and do, often change. Microsoft does not mean to convey any new product launches by choices of these dates.

Releases naturally fall into several phases:

| Phase             | Meaning                                                                 |
|-------------------|-------------------------------------------------------------------------|
| Active Development| Regular development phase where new features and fixes are added.       |
| Stabilization     | Phase focused on stabilizing the release by fixing bugs.                |
| Ask Mode          | Changes are scrutinized and only critical fixes are allowed. No new features are accepted. This is the last phase before a release is closed. |
| Servicing         | Only essential fixes are made to support the release. a.k.a. maintenance mode      |

### release/2411 process
We track the state of candidates for a given release by tagging the PRs:

* `backport_YYMM`: This PR (to `main`) is a candidate to be included in the `YYMM` release.
    * N.B.: A maintainer will _remove_ this tag if the fix is not accepted into the release.
* `backported_YYMM`: This PR (to `main`) has been cherry-picked to the `YYMM` release.
* `backport_YYMM_approved`: This PR (to `main`) has been reviewed by the OpenVMM maintainers, who believe this meets the bar. This is a temporary tag until the PR is cherry-picked (e.g. a TODO).

#### Seeking Approval for Backport

To seek approval to include a change in a release branch, follow these steps:
* Tag your to-`main` PR with `backport_YYMM`.
* Cherry-pick the change to the appropriate `release/YYMM` branch in your fork and stage a PR to that same branch in the main repository.

Please reach out to the maintainers before staging that PR if you have any doubts.

## Existing Release Branches

| Release | Phase | Notes |
|--------|-------|-------|
| release/2411 | Ask Mode | |
 
## Depending on Releases
We welcome feedback, especially if you would like to depend on a reliable release process. Please reach out!
