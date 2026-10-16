# Ubuntu 26.04 packages

> **No longer supported (15 October 2026).** ibara is Omarchy-only. These scripts and the Ubuntu packages published with 0.3.0 and 0.3.1 are kept as history; they get no further maintenance, qualification or releases.

The shipping candidate is 0.3.0-1, for Ubuntu 26.04 amd64 with GNOME50 on Wayland. Qualification and publication are recorded in the project living plan; the presence of these scripts is not a support claim.

There are two installed roles, both using Package: ibara. `ibara-operator_VERSION_amd64.deb` provides the standalone Console, portable viewer and operator service. It creates no target accounts, SSH entry, input permissions or compositor integration. `ibara-target_VERSION_amd64.deb` additionally provides target services, the browser reader and GNOME helper; its exact dependencies include all four maintained native components. Target setup requires one output at scale1 and an unlocked matching session. Other displays and signed-in browser profiles remain outside current qualification.

| Role/environment | Delivery scope |
| --- | --- |
| Standalone operator | Ubuntu 26.04 amd64, GNOME 50 Wayland; Console, native viewing, default keyboard escape/handback, requests and file transfer |
| Native target | Ubuntu 26.04.1, GNOME 50.1 Wayland, maintained Mutter 50.1-0ubuntu2.4+ibara2, one output at scale1; software-rendered VM acceptance |
| Outside acceptance | Physical GPU capture/encoding, multiple outputs, fractional scale, signed-in browser profiles, explicit system-shortcut capture opt-in |

The first Ubuntu delivery has no published Ubuntu predecessor. Update/rollback proof uses signed private predecessor fixtures, including real interrupted installation and equal-version complete-set recovery. Resource measurements from a software VM do not imply hardware acceleration or low-memory performance.

The rendered `install-ubuntu` downloads a signed role manifest, verifies every package digest/size and actual DEB name/version/architecture/role, then installs and caches the complete signed set. Operator is the default. Explicit `target` installs maintained Mutter; it never enrolls Tailscale, enables the helper, restarts GNOME or creates target accounts from package hooks. Preserve an administrative SSH route and stock native packages before installing target support. Enable `ibara@zet.io`, sign out and back in, then run `ibara setup --role both` in the GNOME session. Use `--role target` to omit the operator service. Existing network enrollment remains yours to authorize.

## Install the published Ubuntu release

Run as your desktop user on Ubuntu26.04 amd64:

```bash
curl -fsSL https://github.com/MayberryDT/ibara/releases/download/v0.3.0-1/install-ubuntu -o install-ubuntu
bash install-ubuntu operator
```

For a computer agents will operate, select `target` instead of `operator`. Read the native tuple and recovery requirements above first. The signed installer installs the role's complete set; target activation still requires enabling `ibara@zet.io`, signing out/in and running `ibara setup --role both` inside the GNOME session. Pair from the Console with Add Computer after setup. Tailscale enrollment is separate.

Published0.3.0-1 has a fresh-target bootstrap defect: after apt installs the first complete set, its busy check can ask for an admin key that setup has not created yet. The verified recovery is to run `ibara setup --role both` in the actual GNOME session, then rerun the pinned target installer; retain the first failure receipt. The private frame-peer repair permits the signed fresh path only when all four setup markers are genuinely absent. Any existing or partial setup retains the authenticated busy gate. That repair has not yet been released.

Ubuntu uses the standalone Console rather than the Omarchy bar/widget. Browser/native control, Watch/Take Control, requests and file transfer have scoped E2E evidence; physical GPUs, fractional/multiple outputs and signed-in profiles do not. The private reader comparison uses Chrome155 initially fullscreen; ordinary-window and F11-resize geometry refusals remain separate limitations. A passing synthetic profile does not qualify a user's signed-in profile.

Initial installation from private native +ibara1 to shipping +ibara2 is explicit; channel update/rollback across those different pins is unsupported. `ibara update` and `ibara rollback` require complete signed sets for the installed role, supported platform and native pin. Cache retention counts distinct release versions so duplicate candidate sets cannot evict the predecessor. Retained identity after target removal does not grant target capability to an operator-only package. An absent/incorrect marker or unsettled ibara/native package tuple refuses desktop input, capture and active streaming. Keep retained identity and authority for explicit target reinstall; do not delete data to work around role selection.

For interrupted apt/dpkg installation, preserve the update receipt, run `sudo dpkg --configure -a`, then install the complete verified supported set again. Confirm all four native versions and the ibara version before graphical activation. A running session may still have an older library loaded: sign out/in explicitly after native changes. Package scripts never restart your desktop. Ubuntu target streaming remains native-only: an unavailable or failed native stream reports its reason and refuses Join; explicit Join revalidates repaired eligibility. It does not automatically switch to Sunshine. Signed exact-set installation uses apt --reinstall so equal-version components are installed from their verified DEBs. A failed signature, wrong role or missing component requires correcting the set, not disabling verification.

Stock recovery installs the saved Ubuntu `libmutter-18-0`, `gir1.2-mutter-18`, `mutter-common` and `mutter-common-bin` packages together, with apt's explicit downgrade option when needed, then signs out/in. Stock Mutter has no ibara guard; target input must refuse. Operator-only use can continue after explicit target removal/reinstallation as operator. Preserve retained data and existing remote access throughout.

Build outside the source tree, on the designated build host. Provide `IBARA_BUILD_ROOT`, `IBARA_PLUGIN_SOURCE`, `IBARA_QUICKSHELL_ROOT` (staged `/usr/lib/ibara/quickshell`) and `IBARA_QUICKSHELL_SOURCE` (its corresponding source/license). Shipping builds set `IBARA_PKGREL=1` and use `--release`, stripped binaries and neutral source paths. Private builds instead require a fresh `IBARA_BUILD_ID`. Existing staging is refused. `operator.sh` builds the operator package; `target.sh` builds both role packages. Build the pinned maintained Mutter integration separately using `../mutter/apply.py` and Ubuntu's exact patched source. Keep corresponding native and Quickshell source archives, licenses and build recipes alongside release assets.
