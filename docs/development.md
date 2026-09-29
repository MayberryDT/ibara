# Development

This page explains how to build ibara's core, run its tests, find your way around the code and make a release. [Internals](internals.md) has the detailed reference for each module.

## What you need

- Arch Linux or Omarchy on x86_64
- Rust (the `rust` package) with the 2024 edition
- `git`, to clone and to package, and `openssh`, `openssl` and `acl`, which the tests use
- `base-devel`, to build the package with `makepkg`

To try a change on a real desktop you also need Omarchy with Hyprland, Tailscale and `cua-driver-bin`.

## Build and test

```bash
cargo build --release
cargo test
```

`cargo test` runs the unit tests and the end-to-end tests in `tests/`. The end-to-end tests start real `ibarad` daemons and consoles in temporary folders, with small stand-ins for `hyprctl`, `grim`, `cua-driver`, `ssh` and `tailscale`, so they need no desktop, no network and no root. Run them as an ordinary user: several tests check that files belong to the user running them, which root would pass by accident.

The tests expect Arch's system files: one relies on `/etc/shadow` having mode 0600, so on other distributions it fails. cargo stops at the first failing test program, so use `cargo test --no-fail-fast` to see every failure at once.

CI runs the same `cargo test --locked` as an ordinary user in an `archlinux:base-devel` container on every pull request and every push to `main` ([.github/workflows/ci.yml](../.github/workflows/ci.yml)).

## Where things are

| Folder | What it holds |
|---|---|
| `src/bin/` | The 2 programs, `ibarad` and `ibara`, each a few lines that call into the library |
| `src/contract/` | The agent tools: their input types, from which the JSON Schemas and help are generated |
| `src/controller/` | The engine behind `ibarad`: tasks, steps, checks, control, approvals, Take Control |
| `src/desktop/` | Everything that observes or changes the local Hyprland desktop, through `hyprctl`, `grim` and Cua |
| `src/store/` | The journal: tasks, operations, access, the timeline |
| `src/storage/` | Files, artifacts, jobs and procedures |
| `src/server/` | `ibarad`'s sockets, pairing, invites and the paired consoles' accounts |
| `src/entry/` | The agent entry and the short commands that run on a computer others use |
| `src/operator/` | What a console uses to reach other computers: its directory, routes and `ibara mcp` |
| `src/console/` | The console's service, which the Omarchy plugin talks to |
| `src/install/` | `ibara setup`, `update`, `rollback`, `uninstall` and `unattended-boot` |
| `src/harness/` | `ibara harness`, which runs real agent tasks end to end |
| `tests/` | End-to-end tests of the real binaries |
| `chrome-extension/` | The browser page reader, plain files with no build step |
| `packaging/` | The Arch package, systemd units, helper programs in `ops/`, the installer, and the release and publish scripts |
| `packaging/e2e/` | An installation test in a throwaway Arch container |
| `vendor/cua-hyprland-plugin/` | Cua's Hyprland plugin with ibara's changes, built on each computer |
| `skills/ibara/` | The ibara skill agents read, installed to `/usr/share/ibara/skills/ibara` |
| `docs/` | These pages |

## Rules we follow

- No Node or Python at run time. New logic is Rust in this crate.
- Memory is a requirement, not a nice-to-have: `ibarad` stays at or below 40 MiB when idle. Use the single-threaded runtime, keep no unbounded caches, stream large bodies and drop image buffers once they are encoded.
- There is one contract. Tool definitions, schemas, errors and help all come from the types in `src/contract/`. Never write a schema by hand anywhere else.
- Write intent before effect. Every effect is saved as running before it is dispatched, and becomes unknown after a restart.
- Outcomes are honest. If ibara cannot prove a step worked, it says unknown, never done.
- Messages a person reads are plain English, in American spelling, and buttons use Title Case.

## Tests we write

- End to end first: through `ibara` against a real `ibarad`, as in `tests/`.
- An isolated test only for logic with clear failure cases, such as fingerprints, migrations and schema generation.
- Before writing a test, list the failure cases it must catch, in a comment at its top. Then write the test and see it fail, then write the code.
- No tests that repeat the implementation back to itself.
- Tests use made-up computers and people only.

## Changing an agent tool

1. Change the input type in `src/contract/tools.rs`. Use `#[serde(deny_unknown_fields)]` and give it a `Validate` implementation, even an empty one.
2. Change its entry in `TOOLS` in `src/contract/schema.rs`: a short summary of when to use it, the long description and one example. A test parses every example.
3. Add any new result type to `src/contract/envelope.rs`, and a short rendering of it to `src/contract/render.rs`.
4. Update [agent tools](agent-tools.md).
5. Run `cargo test`. It checks that every schema is valid and that the whole tool list stays within 10 KiB.

[Internals](internals.md#contract-and-mcp) explains the parsing and schema rules.

## Trying it on a real computer

Build the package the way a release does, with a checkout of the console plugin beside this one:

```bash
cd packaging
IBARA_PLUGIN_DIR=$PWD/../../omarchy-ibara IBARA_PKGREL=1 makepkg --nodeps
```

`ibara` depends on `ibara-stream` and `ibara-view` at exactly the same version and release number. So try a change on a computer that already runs a release: set `IBARA_PKGREL` to that release's number (the part after the dash in `pacman -Q ibara`), install the new package over it with `sudo pacman -U`, then run `ibara setup` again. `ibara update` later replaces it with the next real release.

The end-to-end installation test in `packaging/e2e/` builds a clean Arch container, installs 2 releases from a local copy of GitHub's release layout and checks install, update, rollback and uninstall. It needs an Arch host with `systemd-nspawn` and `sudo`; the header of `packaging/e2e/container.sh` describes how to run it.

## Making a release

Releases are GitHub releases of this repository. Only maintainers with the release key can make one.

1. Make sure core, the console plugin and both forks (`ibara-stream` and `ibara-view`, which have no public repositories; their source is in each release's source tarball) are committed and clean.
2. Write the release notes: one plain sentence per line. The console shows them once after the update, under What's New.
3. Run `packaging/publish.sh` from a checkout of this repository whose commit is already pushed:

   ```bash
   packaging/publish.sh --plugin ../omarchy-ibara --stream ../ibara-stream --view ../ibara-view \
     --notes NOTES_FILE 0.1.0-1
   ```

`publish.sh` runs `packaging/release.sh`, which builds the 3 packages, the source tarball and the installer, and signs the manifest. It then checks every file again and uploads a draft release. `--publish` makes it public and marks it Latest, so every install and update gets it from then on.

`release.sh` refuses to build if any of the 4 trees, or any built package, contains a string from the maintainer's private-markers file. That file lists what must never be published, and lives outside every repository.

## Getting help

Open an issue or a draft pull request early. Say what you want to change and why, and we will help you find the right place in the code.
