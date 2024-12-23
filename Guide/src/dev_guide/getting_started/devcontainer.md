# Getting started via Dev Container

This page provides instructions for setting up a development environment using
the repo provided Dev Container configuration for local development or
development using GitHub Codespaces.

The repo provides an Ubuntu `devcontainer.json` that installs Rust and the
supported targets for the project.

## Developing using a GitHub Codespace

Either create a GitHub Codespace via your fork by clicking on the `Code` box, or
visit the link [here](https://github.com/codespaces/new) and select your fork
and branch.

If you plan on using rust-analyzer or doing any sort of dev work, it's
recommended to use an 8 core SKU or beefier.

More documentation can be found at the official GitHub
[docs](https://docs.github.com/en/codespaces).

## Developing using a local dev container

This will use Docker + the dev container vscode extension to launch the repo
provided `devcontainer.json` on your local machine.

Follow the install instructions outlined
[here](https://code.visualstudio.com/docs/devcontainers/containers#_installation).

From there, use the dev container extension in vscode to create a new dev
container for the repository.

It's recommended to clone the repo _inside_ the dev container using the `Dev
Containers: Clone Repository Inside Container Volume...` command, as the
filesystem otherwise will be very slow over the bind mount, which will make your
builds & rust-analyzer very slow.

More documentation can be found at the official vscode
[docs](https://code.visualstudio.com/docs/devcontainers/containers).

## Customizing your dev container

Both GitHub codespaces and local dev containers support dotfile repos which can
be used to run personalized install scripts like installing your favorite tools
and shells, and copying over your dotfiles and configuration.

For codespaces, see the documentation
[here](https://docs.github.com/en/codespaces/setting-your-user-preferences/personalizing-github-codespaces-for-your-account#dotfiles).
For dev containers, see the documentation [here](https://code.visualstudio.com/docs/devcontainers/containers#_personalizing-with-dotfile-repositories).

You can use the same dotfiles repo for both, but note that codespaces has a few
more limitations outlined in their documentation.
