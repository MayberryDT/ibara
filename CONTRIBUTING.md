# Report bugs and suggest changes

Bug reports and suggestions are welcome. Tyler maintains ibara and does not merge outside contributions directly.

You can submit a pull request to demonstrate a proposed fix. Tyler uses Claude or Codex to review submissions through GitHub. He decides whether and how to make a change independently.

This keeps maintenance and responsibility with one person. Please read this policy before spending time on a patch.

## Report a bug

[Open a bug report](https://github.com/MayberryDT/ibara/issues/new/choose) and include:

- your ibara version, from `pacman -Q ibara`, and your Omarchy version
- what you did, what you expected and what happened
- the relevant log lines or screenshots

Remove private information before posting. The [troubleshooting guide](docs/troubleshooting.md) explains where to find logs.

Report security problems privately through the [security reporting process](SECURITY.md).

## Suggest a change

Open an issue describing the problem you want to solve. Include an example of how the change would help.

For console interface problems, use the [console plugin repository](https://github.com/MayberryDT/omarchy-ibara). Report problems with agent tools and the ibara service here.

## Demonstrate a fix

Keep example patches focused on one problem. Explain how to reproduce the failure and how you checked the proposed fix.

The [development guide](docs/development.md) explains how to build and check the source. Run checks as an ordinary user, not root.

We prefer end-to-end checks through the real `ibara` and `ibarad`. List the failure cases before writing an isolated test.

If you use an agent, review its work before submitting it. Explain what you verified and what remains untested.

Follow these conventions in example patches:

- write user messages in plain English, using American spelling
- preserve the exact names of interface controls
- report uncertain outcomes as unknown
- use example computers and people in tests
- update affected documentation when behavior changes

## Licenses and conduct

The [MIT license](LICENSE) covers ibara’s core. The separate stream and viewer forks retain GPL-3.0. Example patches must follow the license of the component they change.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
