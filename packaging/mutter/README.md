# Maintained Ubuntu Mutter integration

This is a maintained candidate for Ubuntu 26.04, native Wayland, Mutter source
`50.1-0ubuntu2.4`. General ibara GNOME input and Ubuntu target setup remain
unqualified until the living plan's acceptance is complete. Never install this
candidate on a host or fleet machine under the isolated VM authorization.

`apply.py` applies the small, checked source changes to a **fresh unpacked**
source tree after Ubuntu's Debian patch series. It refuses other versions and
records the private API in the Debian symbols list. Preserve failed trees and
unpack again after an anchor failure; do not run it twice on a tree.

In the authorized Ubuntu guest on Halla:

```sh
apt-get source mutter=50.1-0ubuntu2.4
sudo apt-get build-dep mutter
python3 /path/to/packaging/mutter/apply.py mutter-50.1
cd mutter-50.1
DEB_BUILD_OPTIONS='nocheck parallel=2' dpkg-buildpackage -b -uc -us
```

For an incremental repair of the same candidate, use `dpkg-buildpackage -nc -b
-uc -us`. `nocheck` skips upstream test execution; a successful build is not
qualification. Retain source hashes, the full Ubuntu-to-candidate diff, package
hashes and build logs outside the source checkout. The Mutter module and patches
are GPL-2.0-or-later and remain separate from the Rust daemon.

The Shell helper binds a transaction to one D-Bus connection and an exact
session/window identity. Native virtual devices carry an immutable generation
tag. Native input callbacks reject stale tasks before changing seat state;
person input disarms the transaction and settles owned modifiers/buttons before
constructing the person's event. The first display filter checks target identity,
and the Wayland dispatch check verifies the actual recipient and default input
handler. Admitted raw events go directly to the target Wayland route, bypassing
Shell shortcuts and native accessibility preference toggles. A trial XKB state
refuses latched/locked modifier or layout changes before the shared seat changes.
A candidate text method commits through an enabled Wayland text-input context on
the exact target surface; composing/pending contexts and nondefault grabs refuse.
It does not touch the clipboard and has no text fallback. Its compatibility and
application delivery require independent qualification.

Tagged pointer devices own coordinates and a separate Clutter sprite, preserving
the person's native pointer. Sprite removal detaches Wayland references before
freeing it; a stale reference previously crashed Shell on a later GTK cursor
request. Retain that failure and test subsequent pointer/text transactions.
The native `ibara-person-input` signal runs before ordinary delivery. The helper
uses it to retire its connection-owned named cursor and balance only its own
visibility/unfocus inhibitors. Cursor-bound input must use that same connection.
Lock, extension disable, owner loss and physical input settle the transaction
and restore person control. Stage painting is an acknowledgement layer, not
final physical-display or product MCP proof.

Cancellation disposes owned devices and emits cleanup releases. A
20-second native expiry bounds a lost helper; no global portal fallback exists.

Receipts report queue admission and internal settlement, not application
delivery. `e2e/linux/editor-fixture.py` and `mutter-proof.py` use separate GTK
processes, saved bytes and event records as independent witnesses. Physical-input
cells use QEMU input only in the isolated mechanism fixture. Final workstation
and product workflow acceptance must use ibara MCP and remains separate.

Before installation, cache stock `libmutter-18-0`, `gir1.2-mutter-18`,
`mutter-common` and `mutter-common-bin` at `50.1-0ubuntu2.4`. Install matching
candidate packages together. Restart only the isolated GNOME session to load the
library and refreshed extension; administrative SSH must remain available.
Rollback installs those cached stock packages with an explicit downgrade, then
restarts that isolated session. Stock Mutter must report no guarded API and ibara
must refuse native input. Never weaken the stock version checks to absorb a
Mutter update: inspect the new ABI, port the narrow patch and repeat affected
acceptance first.

Private Desktop point/key/text/wheel routes now use the guarded API and connection-owned cursor. Target installation still refuses until full acceptance. Text commits are limited to 4,000 UTF-8 bytes, reserving overhead below libwayland’s 4,096-byte message limit; Desktop splits larger text at character boundaries and verifies value, caret and selection after each commit. No clipboard or global input fallback.

The shipping candidate uses native version `50.1-0ubuntu2.4+ibara2`; private `+ibara1` bundles remain evidence only. The C/H guard code is unchanged from iteration17/build21, but shipping DEBs, introspection and source archives have their own digests. Initial private→shipping installation is explicit. Signed rollback stays within the supported native pin; do not mix private and shipping cache sets or claim an older version is compatible merely because its ABI number matches. Full corresponding Ubuntu patched source and package copyright files accompany native release assets. Shipping qualification is recorded separately.
