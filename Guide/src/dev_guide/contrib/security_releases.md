# Security Releases

This page describes how OpenVMM coordinates fixes that cannot be developed in
public before disclosure.

## Report vulnerabilities privately

Report potential vulnerabilities through the
[Microsoft Security Response Center](https://aka.ms/SECURITY.md). Do not open a
public issue, pull request, or discussion for a suspected vulnerability.

MSRC coordinates severity assessment, CVE handling, disclosure timing, and
communication with affected parties. Not every security fix requires an
embargo. When public development would not expose users to additional risk, the
maintainers may use the normal public contribution process.

## Embargoed fix development

When an embargo is warranted, a small need-to-know incident team develops and
reviews the fix in a restricted workspace containing the public OpenVMM source
and the embargoed changes. The workspace must not introduce proprietary source
or dependencies into the eventual public change.

The team prepares the public patch, tests, release notes, advisory, and CVE
material before the coordinated disclosure time. Embargoes should be no longer
than necessary to produce and validate fixes and give affected downstreams a
reasonable opportunity to prepare.

## Coordinating with downstreams

MSRC may authorize advance sharing with qualified downstream security teams
when they need time to validate or integrate a fix. Advance sharing is
case-by-case, not a general early-access program.

An advance package should identify the exact public base commit and affected
versions, include a reviewable patch series and its hashes, and provide build,
test, disclosure, and handling instructions. Recipients must keep the material
need-to-know, use approved secure channels, and must not redistribute the
patches or patched artifacts before disclosure unless MSRC explicitly
authorizes it.

An operator may be authorized to deploy early to a hosted service when users
cannot retrieve the patched software or infer the vulnerability from the
deployment. This also requires explicit incident-specific approval.

```admonish warning
Receiving an embargoed patch does not grant permission to publish it, ship a
patched product, or disclose the vulnerability before the coordinated time.
The instructions supplied with the incident are authoritative.
```

## Coordinated disclosure

At the agreed disclosure time, the incident team coordinates:

1. Publishing the fix to each affected public branch.
2. Publishing a new patch-version OpenVMM release.
3. Publishing the security advisory and CVE information.
4. Notifying coordinated downstreams that the embargo has lifted.

The release, advisory, and fix should become public in the same release
window. Detailed exploit material may be delayed when MSRC determines that
doing so reduces risk without preventing users from understanding and applying
the fix.

Support for older OpenVMM releases is decided per incident. A fix normally
targets supported releases; an affected unsupported release does not
automatically become supported.

## After disclosure

After the public fix is available, development and review continue through the
normal public process. Embargo recipients should replace private patches with
the public fix when practical and securely delete pre-disclosure material when
their incident instructions require it.
