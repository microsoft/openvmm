# Code Review Guidance

## The Code Review Process

Although the OpenVMM repository requires a maintainer to sign off before a pull request can be merged, this does not mean non-maintainer reviews are not valuable. It is extremely important for the health of the project for people to review areas of code they are familiar with. For example, for a change that touches multiple components, somebody familiar with one component could leave a review that says, "The changes to component A look good, but I'm not familiar with the rest of the code." These types of reviews are still extremely useful and help move changes forward. Maintainers will also feel more confident merging changes once subject-area experts have weighed in.

## Submitter's Responsibility

If your patch or change isn’t gaining traction, it is your responsibility as the submitter to follow up and ensure the right people review your changes. This means sending polite reminders to potential reviewers. Do not rely on GitHub notifications for this because people may have them filtered or disabled. The OpenVMM repository has a [CODEOWNERS](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners) file, which will automatically add teams to the review for some components. If you are unsure who the correct audience for the review is, please ask for guidance from maintainers or other contributors.
## Release Mode Testing

The OpenVMM project runs two sets of checks on pull requests:

1. **Standard PR gates** - Run in debug mode for faster feedback on every PR
2. **Release PR gates** - Run in release mode for thorough testing, triggered manually

### When to Use Release Gates

Release gates should be triggered when:
- Your change might behave differently between debug and release builds
- You're making performance-critical changes
- A maintainer requests release mode testing
- You want extra confidence before merging a complex change

### How to Trigger Release Gates

Maintainers can trigger release mode testing in two ways:

1. **Bot command**: Comment `/queue-release-gates` in the PR
2. **Manual trigger**: Use the GitHub Actions "workflow_dispatch" trigger for the "OpenVMM Release PR Gates" workflow

The bot will automatically check that you have maintainer permissions and queue the release gates for the current commit in the PR.
