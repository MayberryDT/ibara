# The fictional wall

The fictional wall lets you try console changes against made-up computers, in a copy of the plugin that runs beside the real one and cannot reach any real computer. Use it before you install changed QML into your own shell.

## Staging it

The stage script creates a separate **development-only** plugin ID (a unique `io.zet.ibara.wall-<random>` ID) from the production QML. It removes the bar-widget entry, rewrites only ID references, and points the service's `daemonSocketPath` at a fictional daemon (`scripts/dev-fixture-daemon.mjs`) that cannot call Tailscale, the installed client, or lifecycle tools. Only `pick-file` and `pick-folder` reach the real `ibarad` (`$XDG_RUNTIME_DIR/ibara/ibarad.sock`), so the desktop chooser runs; without it they fail with `MISSING_DEPENDENCY`. The existing `io.zet.ibara` installation is not its target.

```sh
node tests/stage-wall-acceptance.mjs 20
```

The command prints `/tmp/ibara-wall-acceptance-*` and, on its second line, the command that starts that package's fictional daemon on `/tmp/ibara-dev-wall-<plugin-id>/ibarad.sock` (also recorded in `DEVELOPMENT-ONLY`). Both folders are in `$TMPDIR` when it is set, instead of `/tmp`:

```sh
IBARA_DEV_WALL_COUNT=20 node /tmp/ibara-wall-acceptance-*/scripts/dev-fixture-daemon.mjs /tmp/ibara-dev-wall-<plugin-id>/ibarad.sock
```

Start it in its own terminal before enabling the staged plugin and stop it with Ctrl-C after removing the plugin; it refuses a socket that something still answers on. The stage script does not start it, so no fictional process outlives the session unnoticed. If the service connected first, it reconnects on its own. Read the staged folder’s manifest for its unique plugin ID. Every staging run gets a fresh ID and source path because Qt can retain child-component code after rescan/re-enable. Poll the registry until the new ID appears before enabling or summoning; a first enable racing rescan is not a product failure. Do not reuse an earlier staged ID after editing QML dependencies. Supported counts: 0, 1, 3, 10, 15, 20, 100. Count 15 is a fictional fleet with names from Iris to Pike, with holders, live tasks, file listings and grants; at any count, `incoming` in `control.json` adds a pending request to use this computer. Count 15 also answers Take Control and Hand Back without opening a viewer: the computer's holder becomes the fictional person and back. It needs 15 screen pictures, `screen-01.jpg` to `screen-15.jpg`, in a folder you name with `--assets DIR`; `magick` converts them. Run `tests/run` first. Check the staged manifest and its `DEVELOPMENT-ONLY` marker before you use it. Then copy the staged folder into `~/.config/omarchy/plugins/<staged-manifest-id>`, enable only that ID, summon it, and remove only that ID when you are done. If that folder already exists, keep a copy of it first. Never change or disable the real `io.zet.ibara` plugin, its credentials or your shell configuration for this.

Every fictional computer answers the Windows tab with the same six windows over workspaces 1, 2 and the scratchpad; Dune shows an empty workspace 3 on screen, and Nimbus, on an older ibara, answers "Unknown operator operation." Where count 15 has an agent at work, its `foot` terminal is In Use by an Agent, and its browser shows no agent but answers Close or Move To… with `BUSY` the first time, as a window an agent took after the list was read; Stop Task ends that task. btop (app id `org.omarchy.btop`, marked `terminal: true` as core marks any window whose process is a terminal) was left by an earlier task and asks before it closes, the text editor stays open after Close as one asking to save does, and the rest close or move. Changes last until the daemon stops.

Capture the hosted wall at 0/1/3/10/20 and 100 records. At 1920×1080 verify 20 identifiable entries; at 1440×900 verify 10; at 960×640 and enlarged text verify scrolling and controls. Test keyboard, selection, search, list/tiles, filters and theme variants. The 100-record case is rendering/search pressure only. A fictional frame does **not** prove real screen recognizability, capture freshness or preview performance. Verify those separately through the genuine installed ibara client and target.

## Load profile

`node tests/stage-wall-acceptance.mjs 20 --load PROFILE.json` stages the same fictional wall with changing desktop-like frames and independent endpoints. Like `ibarad`, the fixture daemon writes each frame as a PPM file scaled to the shown size the request names (`--width`, `--height`; 448×256 for a tile and 1152×648 for a selected frame when none is named). PROFILE.json holds measured real `[round_trip_ms, arrival_age_ms]` samples for `tile` and `selected`; each fictional request samples one pair. Record a profile from the genuine persistent preview route first.

The fixture daemon reads `control.json` and writes `requests.jsonl` beside its socket in `/tmp/ibara-dev-wall-<plugin-id>/`, outside the plugin folder, because the shell hot-reloads a plugin whenever its files change. Each `operator-observe` is answered by a `preview` event after one sampled round trip. `control.json` keys:

- `deny`, `offline`, `hang` and `revoked` (lists of fictional IDs). `revoked` returns the message the real routes give after the target closes a revoked console's socket.
- `epoch` (a new controller epoch, as after a target restart) and `session_refused` (fictional IDs whose `operator-session` bootstrap is refused).
- `late` (`{"computer": ID, "delay_ms": N}`). This sends that computer's next preview late with a solid red frame, sequence 999999, which must never appear.
- Login sharing: `logins` (`on`, the default, sharing from Brave; `undecided`, `off`, `elsewhere`, `target` or `old`), `login_requests` (agents asking for logins), `login_signed_out`, `login_browser_closed`, `login_unknown` and `login_rejected`, described beside `loginCommand` in `tests/dev-fixture-daemon.mjs`. Offscreen screenshots of every login surface against these come from `.research/login-sharing/console/harness/` in the ibara repository.

Sample the running shell with `tests/wall-load-sample.mjs --target PLUGIN_ID --seconds N --interval MS --out FILE.jsonl`, then summarize with `tests/wall-load-summary.mjs FILE.jsonl [--from MS] [--to MS] [--requests requests.jsonl]`. Ages come from the service's `previewStats` IPC (the frame it is showing), CPU from the Quickshell process tree. This measures the console side's load with modeled network and target time; actual target capture cost is measured separately against the real computer.

The host's plugin registry reads `HOME/.config/omarchy/plugins`, and `omarchy-shell` IPC resolves the running shell via `OMARCHY_PATH/shell`. Source: `/usr/share/omarchy/shell/services/PluginRegistry.qml`, `/usr/share/omarchy/shell/shell.qml`, `/usr/share/omarchy/shell/README.md` and `/usr/bin/omarchy-shell` (inspected 23 September 2026). Quickshell loads first-party idle/lock services with the full host; a second full shell on the same Wayland display is not a safe test shortcut.
