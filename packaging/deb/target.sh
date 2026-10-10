#!/usr/bin/env bash
# Stage the private matched GNOME target foundation; never activates a session.
set -euo pipefail
core=$(cd -- "$(dirname -- "$0")/../.." && pwd)
: "${IBARA_BUILD_ROOT:?Set the build output directory}"
build=$IBARA_BUILD_ROOT
profile=debug
if [[ -n ${IBARA_PKGREL:-} ]]; then
  profile=release
  stage_id="release-$IBARA_PKGREL"
else
  : "${IBARA_BUILD_ID:?Choose a fresh private build ID}"
  stage_id=$IBARA_BUILD_ID
fi
bash "$core/packaging/deb/operator.sh"
version=$("$build/target/$profile/ibara" --version)
version=${version#ibara }
asset_name=ibara
if [[ $profile == release ]]; then
  stage_id="release-$version"
  asset_name=ibara-target
fi
stage="$build/deb-target-root-$stage_id"
[[ ! -e $stage ]] || { echo "Existing target stage: $stage" >&2; exit 1; }
cp -a "$build/deb-operator-root-$stage_id" "$stage"
lib="$stage/usr/lib/ibara"
for script in entry.sh ibara-op-shell ibara-agent-sshd ibara-chrome-native; do
  install -Dm755 "$core/packaging/ops/$script" "$lib/ops/$script"
done
install -Dm755 "$core/packaging/ops/computerctl" "$stage/usr/bin/computerctl"
install -d "$lib/chrome-extension"
install -m644 "$core"/chrome-extension/{manifest.json,worker.js,document.js} "$lib/chrome-extension/"
for unit in ibara-agent-sshd.service ibara-access.socket ibara-access@.service ibara-power.socket ibara-power@.service; do
  install -Dm644 "$core/packaging/systemd/$unit" "$stage/usr/lib/systemd/system/$unit"
done
install -Dm644 "$core/packaging/systemd/agent-computer.service" "$stage/usr/lib/systemd/user/agent-computer.service"
install -Dm644 "$core/packaging/ibara.sysusers" "$stage/usr/lib/sysusers.d/ibara.conf"
install -Dm644 "$core/packaging/ibara.tmpfiles" "$stage/usr/lib/tmpfiles.d/ibara.conf"
install -Dm644 "$core/packaging/60-ibara-uinput.rules" "$stage/usr/lib/udev/rules.d/60-ibara-uinput.rules"
install -d "$stage/usr/share/gnome-shell/extensions/ibara@zet.io"
install -m644 "$core"/packaging/gnome/ibara@zet.io/{extension.js,metadata.json} "$stage/usr/share/gnome-shell/extensions/ibara@zet.io/"
python3 - "$stage" "$version" <<'PY'
from pathlib import Path
import json,sys
stage=Path(sys.argv[1]);version=sys.argv[2]
(stage/'usr/lib/ibara/gnome-target.json').write_text(json.dumps({'schema':1,'candidate_version':version,'mutter_version':'50.1-0ubuntu2.4+ibara2','development_only':'-dev.' in version})+'\n')
(stage/'usr/lib/ibara/gnome-target.json').chmod(0o644)
control=stage/'DEBIAN/control';text=control.read_text()
text=text.replace('libpipewire-0.3-0t64\n','libpipewire-0.3-0t64, libatspi2.0-0t64, openssh-server, acl, sudo, tailscale, gnome-text-editor, ptyxis, nautilus, libmutter-18-0 (= 50.1-0ubuntu2.4+ibara2), mutter-common (= 50.1-0ubuntu2.4+ibara2), mutter-common-bin (= 50.1-0ubuntu2.4+ibara2), gir1.2-mutter-18 (= 50.1-0ubuntu2.4+ibara2)\n')
text=text[:text.index('Description:')]+'Description: ibara GNOME target and operator Console for Ubuntu 26.04\n Requires explicit matching Mutter/helper activation and Tailscale enrollment.\n Desktop services and compositor activation remain explicit setup operations.\n'
control.write_text(text)
PY
# No postinst enables services/accounts, loads the helper or restarts GDM.
# Explicit setup checks the exact native tuple and live guard before effects.
install -d "$build/packages/target"
dpkg-deb --threads-max=2 -Zzstd -z3 --root-owner-group --build "$stage" "$build/packages/target/${asset_name}_${version}_amd64.deb"
sha256sum "$build/packages/target/${asset_name}_${version}_amd64.deb"
