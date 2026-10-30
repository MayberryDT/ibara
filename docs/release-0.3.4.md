# 0.3.4 notification privacy correction

30 October 2026. Runtime core `f883cb8704b84f2615cd71face310df15b39c7b2`; bundled plugin `9e53ccf` on `release/privacy-034`. Marketplace main already carries the reviewed notification fix `ca1c7f6`; its setup instructions were corrected in `0368c1a`.

Core setup preserves existing Marketplace directories and independent symlinks. Fresh setup links the corrected package plugin. Uninstall removes only the package-owned link. Desktop notifications contain fixed generic text and Open; private request details and decisions stay in the console.

Update core with `ibara update`; update an existing Marketplace copy through Marketplace too. Update core before rerunning setup: older core versions can replace a corrected plugin with their older bundle.

## Evidence

The checked package prints `ibara 0.3.4-1`. Its bundled `Service.qml` SHA256 is `84fcc273a8432c22ae94382790af58288f0ad1ec8649dded31901cca93b274cd`, matching the release plugin source. Streaming/viewing payloads match 0.3.3 byte-for-byte; package version metadata and mtree were regenerated. Package dependencies pin both siblings to 0.3.4-1. Manifest signature, package digests and private-marker scans passed.

[Repeatable fixture sources and result receipts](verification/notification-privacy/) cover:

- The real packaged CLI: two setup calls, then uninstall, for fresh, Marketplace-directory and independent-symlink homes. Fresh setup creates the package link; independently installed contents/links survive setup and uninstall.
- The real release QML service: pop-ups disabled, and pop-ups plus desktop notifications enabled. Approval, question, sign-in and ordinary notifications spawn with no synthetic private canaries in argv. All three request Open actions route to the original request card.

These are process-level integration checks on Omarchy. Setup runs in bubblewrap with an isolated home; sudo, system service control and shell management are stubs. QML uses the real Wayland backend and notification process spawning, a synthetic attention queue and a recording notify-send stand-in. They do not claim a fresh whole-machine installation, a real notification-server click, or a security audit. The live installation was preserved.

Independent source review caught uninstall's treatment of independent symlinks; the repair is included and the packaged CLI check covers it. Review also identified the separate packaging input, which was verified against the final package.

## Repeat the checks

Use an Omarchy workstation with ibara already installed, Python 3, bubblewrap, bsdtar and Quickshell. Work in a **new scratch directory for each run**. Copy the files from `docs/verification/notification-privacy/` there, download and verify the 0.3.4 assets using [release verification](release-verification.md), then extract the core package into `unpacked/`.

```bash
python3 setup-e2e.py
ln -s unpacked/usr/share/ibara/omarchy-plugin final-plugin
ln -s /usr/share/omarchy/shell/Commons Commons
ln -s /usr/share/omarchy/shell/Ui Ui
mkdir -p stubs qml-home qml-state
cp notify-send stubs/notify-send
chmod +x stubs/notify-send
runtime=$(mktemp -d /tmp/ibara-privacy.XXXXXX)
ln -s "/run/user/$UID/$WAYLAND_DISPLAY" "$runtime/$WAYLAND_DISPLAY"
for mode in false true; do
  PATH="$PWD/stubs:$PATH" HOME="$PWD/qml-home" XDG_STATE_HOME="$PWD/qml-state" \
    PRIVACY_POPUPS="$mode" PRIVACY_ARGV_LOG="$PWD/notification-$mode.jsonl" \
    QT_QPA_PLATFORM=wayland XDG_RUNTIME_DIR="$runtime" \
    timeout 12 quickshell -p notification.qml >"notification-$mode.log" 2>&1
done
python3 verify-notifications.py
```

The result JSON files and notification argv logs are repeatable artifacts. Fixtures use fictional canaries; keep real account data out of them. Read the script before running it: setup fixtures require a fresh scratch directory and isolate service/shell changes rather than exercising actual system installation.
