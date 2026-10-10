# Contributing to the ibara plugin

Thank you for helping. This repository is ibara's console for Omarchy: the bar widget, the quick panel and the console, written in QML. It talks only to the local `ibarad` service over one socket. Everything else, including that service, is in ibara's core, [MayberryDT/ibara](https://github.com/MayberryDT/ibara).

## Reporting a bug

[Open an issue](https://github.com/MayberryDT/omarchy-ibara/issues/new/choose) for anything you see in the bar, the quick panel or the console. Include the ibara version (`pacman -Q ibara`), what you did, what you expected and what happened, and a screenshot if it helps. Remove anything private first.

Problems with installing, agents, pairing or Take Control usually belong in [ibara's core](https://github.com/MayberryDT/ibara/issues/new/choose). To report a security problem, follow ibara's [security policy](https://github.com/MayberryDT/ibara/blob/main/SECURITY.md) instead of opening an issue.

## Making a change

1. Read [design](docs/design.md) and follow it exactly: square shapes, theme colors only, Title Case button text and plain English messages.
2. Try console changes in [the fictional wall](docs/fictional-wall.md) first. It runs under its own plugin ID and cannot reach real computers.
3. Run `tests/run` before every commit. It checks the manifest, the QML syntax when `qmllint` (from `qt6-declarative`) is installed, the model tests and, when Omarchy is installed, `omarchy plugin validate`. It needs Python 3 and Node.
4. Use made-up computers and people in tests, fixtures and screenshots.
5. Update [the console reference](docs/reference.md) when behavior changes, and add a line to the [changelog](CHANGELOG.md) for anything a person would notice.
6. Open a pull request and fill in the template.

If a change needs something new from `ibarad`, open an issue in ibara's core first, so the two sides can be designed together.

## Working with an AI agent

You are welcome to use an agent. Point it at this file and at [docs/design.md](docs/design.md) before it starts, and review its work as your own.

## License

This plugin is MIT-licensed. By contributing, you agree that your contribution is licensed under the same terms.

Everyone taking part follows our [code of conduct](CODE_OF_CONDUCT.md).
