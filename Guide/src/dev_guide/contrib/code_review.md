# Code Review Guidance

## The Code Review Process

Although the OpenVMM repository requires a maintainer to sign off before a pull request can be merged, this does not mean non-maintainer reviews are not valuable. It is extremely important for the health of the project for people to review areas of code they are familiar with. For example, for a change that touches multiple components, somebody familiar with one component could leave a review that says, "The changes to component A look good, but I'm not familiar with the rest of the code." These types of reviews are still extremely useful and help move changes forward. Maintainers will also feel more confident merging changes once subject-area experts have weighed in.

## Submitter's Responsibility

If your patch or change isn’t gaining traction, it is **your responsibility** as the submitter to follow up and ensure the right people review your changes. This means sending polite reminders to potential reviewers. Do not rely on GitHub notifications for this because people may have them filtered or disabled. The OpenVMM repository has a [CODEOWNERS](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners) file, which will automatically add teams to the review for some components. If you are unsure who the correct audience for the review is, please ask for guidance from maintainers or other contributors.