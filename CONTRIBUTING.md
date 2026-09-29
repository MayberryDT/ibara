# Contributing to ibara

Thank you for helping. ibara is a small project, and every bug report, fix and idea makes a real difference.

## Reporting a bug

[Open an issue](https://github.com/MayberryDT/ibara/issues/new/choose) and choose Bug report. Tell us:

- the ibara version (`pacman -Q ibara`) and your Omarchy version
- what you did, what you expected and what happened
- the relevant log lines ([troubleshooting](docs/troubleshooting.md) says where to find them)

Remove anything private from logs and screenshots first.

To report a security problem, do not open an issue. Follow [SECURITY.md](SECURITY.md) instead.

## Suggesting a change

Open an issue before you start on anything large, so we can agree on the approach first. For small fixes, a pull request on its own is fine.

The console plugin lives in its own repository, [omarchy-ibara](https://github.com/MayberryDT/omarchy-ibara). Changes to what you see in the console usually go there. Changes to what the console asks `ibarad` for go here.

## Making a change

[Development](docs/development.md) explains how to build, test and find your way around the code. In short:

1. Fork the repository and make a branch.
2. Make one logical change per commit. Write commit messages in plain English that say what changed for the person using ibara.
3. Add or change tests. We prefer end-to-end tests through the real `ibara` and `ibarad`. List the failure cases a test must catch before you write it.
4. Run `cargo test` as an ordinary user, not root.
5. Update the docs if behavior changed, and add a line to the [changelog](CHANGELOG.md) for anything a person would notice.
6. Open a pull request and fill in the template.

A few rules keep ibara what it is:

- No Node or Python at run time.
- `ibarad` stays small: at or below 40 MiB when idle.
- Messages a person reads are plain English, in American spelling. Buttons use Title Case.
- An outcome ibara cannot prove is reported as unknown, never as done.
- Tests and examples use made-up computers and people.

## Working with an AI agent

You are welcome to use an agent. Point it at this file and at [docs/development.md](docs/development.md) before it starts, and review its work as your own: you are responsible for what you submit.

## License

ibara's core is licensed under the [GNU General Public License version 3 only](LICENSE). By contributing, you agree that your contribution is licensed under the same terms.

## Code of conduct

Everyone taking part in ibara follows our [code of conduct](CODE_OF_CONDUCT.md).
