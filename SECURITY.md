# Security policy

ibara lets people and agents use real computers, so we take security reports seriously and answer them first.

## Reporting a vulnerability

Report it privately through GitHub: on this repository's Security tab, choose Report a vulnerability. Please do not open a public issue, pull request or discussion about it.

This covers the whole of ibara: this core, the console plugin in [omarchy-ibara](https://github.com/MayberryDT/omarchy-ibara), and the `ibara-stream` and `ibara-view` packages.

Tell us:

- what an attacker could do, and what they need first (for example, a computer on the same tailnet, a paired computer or a connected agent)
- the steps to reproduce it, and the ibara version (`pacman -Q ibara`)
- any fix you have in mind

We will reply to your report within 7 days, keep you told how the fix is going, and credit you in the release notes unless you would rather we did not.

## Supported versions

Only the newest release gets security fixes. `ibara update` installs it.

## What counts

We especially want to hear about:

- any way to reach a computer without being paired, or to get more access than your pairing or invite gives
- any way for an agent to act beyond its permissions, or to run a step that asks first without an approval
- any way to get a viewer admitted without its one-time ticket
- any way to make the installer or `ibara update` accept a release not signed with the release key
- anything that exposes ibara's ports beyond the Tailscale network

Some limits are known and documented in [security and access](docs/security-and-access.md#where-the-protection-ends). For example, an agent that may run commands or use the desktop is not in a sandbox. Reports that show those limits are worse than documented are still welcome.
