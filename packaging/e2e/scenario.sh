#!/usr/bin/env bash
# The install end to end, in the container of container.sh (created and
# booted first), as its desktop account alice:
#
#   1. a signature that does not match, or an ibara-stream or ibara-view
#      package that does not match the signed release, stops the installer
#   2. curl -fsSL URL/install | sh: ibara, ibara-stream and ibara-view, `ibara
#      setup`, and everything setup makes; Take Control's two programs start
#      and stream to each other on a desktop without a screen
#   3. `ibara setup` again changes nothing and restarts nothing; a Hyprland
#      upgrade only builds Cua's plugin again and leaves Tailscale as it was
#   4. `ibara update`: refuses a stream or viewer package that does not match,
#      then a newer signed release, What's New shown once
#   5. `ibara rollback`, putting back the journal the newer version had moved on,
#      with a computer removed since the update staying removed
#   6. `ibara uninstall` keeps this computer's identity
#   7. the marketplace route: the plugin first, its Install ibara command, same identity
#   8. `ibara uninstall --delete-data` leaves nothing
#
# All of it without Node (design §12.6): nodejs is not installed in the
# container, and a watcher records any node process from before the first
# install to the end.
#
#   scenario.sh EVIDENCE_DIR RELEASE_1 RELEASE_2 PLUGIN_GIT_DIR
#
# RELEASE_1 and RELEASE_2 are packaging/release.sh outputs signed with the key
# and address compiled into them, RELEASE_2 the newer; PLUGIN_GIT_DIR is the
# plugin's git checkout. container.sh channel serves each release at that address.
set -uo pipefail
evidence=${1:?evidence dir} r1=${2:?release 1} r2=${3:?release 2} plugin=${4:?plugin checkout}
v1=$(jq -r .version "$r1/stable.json") v2=$(jq -r .version "$r2/stable.json")
url=$(sed -n "s/^IBARA_BASE_URL='\(.*\)'$/\1/p" "$r1/install")
here=$(cd -- "$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")" && pwd -P)
c=$here/container.sh
mkdir -p "$evidence"
: >"$evidence/summary"
pass=0 fail=0

A() { "$c" alice "$1"; }
R() { "$c" root "$1"; }
step() { printf '\n== %s\n' "$1" | tee -a "$evidence/summary"; }
# check NAME COMMAND…: the command's output goes to checks.log.
check() {
  local name=$1
  shift
  printf '\n### %s\n$ %s\n' "$name" "$*" >>"$evidence/checks.log"
  if "$@" >>"$evidence/checks.log" 2>&1; then
    echo "PASS $name" | tee -a "$evidence/summary"
    pass=$((pass + 1))
  else
    echo "FAIL $name" | tee -a "$evidence/summary"
    fail=$((fail + 1))
  fi
}
# Run as alice, keeping the full output in the evidence folder.
run() { local log=$1; shift; A "$1" >"$evidence/$log" 2>&1; echo "exit=$?" >>"$evidence/$log"; }
sudo_since() { R "journalctl -q _COMM=sudo --since=@$1 -o cat | grep -c 'COMMAND=' || true"; }
now() { R 'date +%s' | tr -d '\r'; }
ask() { A "python3 /usr/local/bin/console-ask $*"; }

sudo install -m 0755 "$here/console-ask" /var/lib/machines/ibara-e2e/usr/local/bin/console-ask
sudo install -m 0755 "$here/stream-check" /var/lib/machines/ibara-e2e/usr/local/bin/stream-check

step "0. No Node"
check "nodejs is not installed and there is no node program" R '! pacman -Qq | grep -q "^nodejs" && ! command -v node && ! command -v nodejs'
# Twice a second: any process called node, or running a program called node.
R 'systemd-run --unit=ibara-e2e-node-watch --collect sh -c "while :; do pgrep -a -x node; for e in /proc/[0-9]*/exe; do readlink \$e; done 2>/dev/null | grep -E \"/nodejs?\$\"; sleep 0.5; done >>/var/log/ibara-e2e-node-watch.log"'
check "the node watcher runs" R 'systemctl is-active ibara-e2e-node-watch.service'

step "1. A release whose signature does not match installs nothing"
tampered=$(mktemp -d)
cp "$r1"/* "$tampered/"
sed -i 's/"version": "/"version": "9/' "$tampered/stable.json"
"$c" channel "$tampered"
run 01-tampered.log "curl -fsSL $url/install | sh"
rm -rf "$tampered"
check "installer refuses a manifest the release key did not sign" grep -q "not signed with ibara's key" "$evidence/01-tampered.log"
check "nothing was installed" R '! pacman -Q ibara'
none_installed='! pacman -Q ibara && ! pacman -Q ibara-stream && ! pacman -Q ibara-view'
for name in ibara-stream ibara-view; do
  # A signed release whose package on the server is not the one it names.
  tampered=$(mktemp -d)
  cp "$r1"/* "$tampered/"
  printf 'x' >>"$(ls "$tampered/$name"-[0-9]*.pkg.tar.zst)"
  "$c" channel "$tampered"
  run "01-tampered-$name.log" "curl -fsSL $url/install | sh"
  rm -rf "$tampered"
  check "installer refuses an $name package that does not match the signed release" grep -q "the $name download does not match the signed release" "$evidence/01-tampered-$name.log"
  check "nothing was installed" R "$none_installed"
done

step "2. curl -fsSL URL/install | sh"
"$c" channel "$r1"
t=$(now)
run 02-install.log "curl -fsSL $url/install | sh"
check "installer and ibara setup finished" grep -q '^exit=0' "$evidence/02-install.log"
check "ibara, ibara-stream and ibara-view installed at the release's version" R 'pacman -Q ibara ibara-stream ibara-view && [ "$(pacman -Q ibara-stream ibara-view | cut -d" " -f2 | sort -u)" = "$(pacman -Q ibara | cut -d" " -f2)" ]'
check "ibara depends on exactly that ibara-stream and ibara-view" R "pacman -Qi ibara | tr -s ' \n' ' ' | grep -q 'ibara-stream=$v1 ibara-view=$v1'"
check "Take Control's host runs: every library found, its assets in place" A '! ldd /usr/bin/ibara-stream | grep "not found" && test -f /usr/share/ibara-stream/apps.json && test -f /usr/share/ibara-stream/shaders/opengl/Scene.frag && d=$(mktemp -d) && HOME=$d XDG_CONFIG_HOME=$d/.config /usr/bin/ibara-stream --version'
check "the viewer runs and makes the console's viewer identity" A '! ldd /usr/bin/ibara-view | grep "not found" && d=$(mktemp -d) && /usr/bin/ibara-view --create-identity $d/viewer && test -s $d/viewer/cert.pem'
check "Take Control starts: ibara-stream as the controller starts it, ibara-view with the console's bundle, admitted and streaming, then revoked and settled" A 'python3 /usr/local/bin/stream-check ~/stream-check.txt'
R 'cat /home/alice/stream-check.txt' >"$evidence/02-stream-check.txt" 2>&1
n=$(sudo_since "$t" | tr -d '\r')
check "three root commands (pacman, keeping the package, ibara setup's one) under one sudo sign-in" test "$n" -eq 3
check "setup said how to sign in to Tailscale" grep -q 'Not signed in yet' "$evidence/02-install.log"
check "root sockets belong to alice" R 'stat -c "%U %a %n" /run/ibara-access.sock /run/ibara-power/power.sock && [ "$(stat -c %U /run/ibara-access.sock)" = alice ] && [ "$(stat -c %U /run/ibara-power/power.sock)" = alice ]'
check "system units enabled" R 'systemctl is-enabled ibara-agent-sshd.service ibara-access.socket ibara-power.socket'
check "setup started the new agent entry and root helpers" bash -c "grep -q 'Agent entry restarted: it changed' $evidence/02-install.log && grep -q 'Root helpers restarted: they changed' $evidence/02-install.log"
check "restarting the agent entry leaves agents' sessions running" R 'systemctl show -p KillMode ibara-agent-sshd.service | grep -qx KillMode=process'
check "agent entry waits for Tailscale" R 'systemctl status ibara-agent-sshd.service --no-pager | head -5; journalctl -q -u ibara-agent-sshd.service -o cat | grep -q "Tailscale has no address yet"'
check "agent entry sshd_config valid, one Match for paired computers" R 'sshd -t -f /etc/agent-computer/ssh/sshd_config && grep -c "^Match User ibara-op-\*" /etc/agent-computer/ssh/sshd_config | grep -qx 1'
check "install root points at the package" R '[ "$(readlink /opt/agent-computer/current)" = /usr/lib/ibara ] && [ "$(readlink /usr/local/sbin/ibara-op-shell)" = /usr/lib/ibara/ops/ibara-op-shell ]'
check "agent account unlocked for keys, in ibara-runtime" R 'passwd -S ibara-agent && passwd -S ibara-agent | grep -q " P " && id -nG ibara-agent | grep -qw ibara-runtime'
check "keys, policy, station and access inventory" R 'ls -l /etc/agent-computer /etc/ibara-operator /etc/ibara && stat -c "%U:%G %a" /etc/agent-computer/gateway.key | grep -qx "root:ibara-runtime 640" && stat -c "%a" /etc/agent-computer/admin.key | grep -qx 600 && test -f /etc/agent-computer/access-transport.json && jq -e ".agent_account == \"alice\"" /etc/ibara/station.json'
check "alice reads the gateway key and controller folder through ACLs" R 'getfacl -p /etc/agent-computer/gateway.key | grep -q "^user:alice:r--" && getfacl -p /run/agent-computer | grep -q "^user:alice:rwx"'
check "firewall opens entry, pairing and streaming on tailscale0 only" R 'ufw status | grep "ibara"; [ "$(ufw status | grep -c "on tailscale0.*# ibara")" -ge 4 ] && ! ufw status | grep "ibara" | grep -v tailscale0'
check "Tailscale on, alice may sign in without sudo" R 'systemctl is-active tailscaled && tailscale debug prefs | jq -e ".OperatorUser == \"alice\""'
check "user services running" A 'systemctl --user is-active agent-computer.service ibara-operator.service'
check "Cua plugin built for this Hyprland; Hyprland loads it" R 'cat /opt/agent-computer/cua/build.json && test -s /opt/agent-computer/cua/cua-hyprland-plugin.so && grep -q /opt/agent-computer/cua/hyprland.lua /home/alice/.config/hypr/hyprland.lua'
check "browser page reader installed by policy" R 'ls /etc/chromium/policies/managed/ && test -f /etc/chromium/policies/managed/ibara.json'
check "plugin linked, enabled once, shell restarted once" A 'cat ~/omarchy-stub.log; [ "$(readlink ~/.config/omarchy/plugins/io.zet.ibara)" = /usr/share/ibara/omarchy-plugin ] && [ "$(grep -c putBarWidget ~/omarchy-stub.log)" = 1 ] && [ "$(grep -c omarchy-restart-shell ~/omarchy-stub.log)" = 1 ]'
check "console answers with no error on a fresh computer" A 'python3 /usr/local/bin/console-ask status | tee /dev/stderr | jq -e ".envelope.data.station_configured == false and .envelope.error == null"'
check "console tailnet: Tailscale running, not signed in" A 'python3 /usr/local/bin/console-ask tailnet | tee /dev/stderr | jq -e ".envelope.data.tailscale.state == \"logged_out\""'
check "no What's New after a first install" A 'python3 /usr/local/bin/console-ask whats-new | jq -e ".envelope.data.version == null"'
R 'ssh-keygen -lf /etc/agent-computer/ssh/host_ed25519.pub' >"$evidence/host-key-1.txt"

step "3. ibara setup again"
t=$(now)
run 03-setup-again.log 'ibara setup'
check "second setup finished" grep -q '^exit=0' "$evidence/03-setup-again.log"
n=$(sudo_since "$t" | tr -d '\r')
check "ibara setup asks for root once" test "$n" -eq 1
check "nothing to relink, enable or restart" bash -c "grep -q 'Already linked' $evidence/03-setup-again.log && grep -q 'Already showing' $evidence/03-setup-again.log && grep -q 'Already built' $evidence/03-setup-again.log"
check "still one shell restart in all" A '[ "$(grep -c omarchy-restart-shell ~/omarchy-stub.log)" = 1 ]'
check "agent entry and root helpers left running" bash -c "grep -q 'Agent entry unchanged, left running' $evidence/03-setup-again.log && grep -q 'Root helpers unchanged, left running' $evidence/03-setup-again.log"

step "3b. A Hyprland upgrade only builds Cua's plugin again"
# The person turned Tailscale off; Omarchy's update then reinstalls Hyprland.
R 'systemctl disable --now tailscaled.service' >>"$evidence/checks.log" 2>&1
R 'pacman -U --noconfirm $(ls /var/cache/pacman/pkg/hyprland-[0-9]*.pkg.tar.zst | tail -1)' >"$evidence/03b-hyprland.log" 2>&1
check "only the Cua hook ran, and found the plugin built for this Hyprland" bash -c "grep -q \"Building ibara's Hyprland plugin\" $evidence/03b-hyprland.log && grep -q 'Already built for Hyprland' $evidence/03b-hyprland.log && ! grep -q 'Refreshing ibara' $evidence/03b-hyprland.log"
check "Tailscale left off, as the person left it" R '[ "$(systemctl is-enabled tailscaled.service)" = disabled ] && ! systemctl is-active tailscaled.service'
R 'systemctl enable --now tailscaled.service' >>"$evidence/checks.log" 2>&1

step "4. ibara update"
for name in ibara-stream ibara-view; do
  tampered=$(mktemp -d)
  cp "$r2"/* "$tampered/"
  printf 'x' >>"$(ls "$tampered/$name"-[0-9]*.pkg.tar.zst)"
  "$c" channel "$tampered"
  run "04-tampered-$name.log" 'ibara update'
  rm -rf "$tampered"
  check "update refuses an $name package that does not match the signed release" grep -q "The downloaded $name package does not match the signed release" "$evidence/04-tampered-$name.log"
  check "nothing was updated or kept" R "[ \"\$(pacman -Q ibara ibara-stream ibara-view | grep -c -- ' $v1\$')\" = 3 ] && [ \"\$(ls /var/cache/ibara/packages | wc -l)\" = 3 ]"
done
"$c" channel "$r2"
run 04-check.log 'ibara update --check'
check "update --check names the newer release" grep -q 'is available' "$evidence/04-check.log"
run 04-update.log 'ibara update'
check "update finished" grep -q '^exit=0' "$evidence/04-update.log"
check "newer ibara, ibara-stream and ibara-view installed" R "[ \"\$(pacman -Q ibara ibara-stream ibara-view | grep -c -- ' $v2\$')\" = 3 ]"
check "the hook's setup left the agent entry running and Tailscale alone" bash -c "grep -q 'Agent entry unchanged, left running' $evidence/04-update.log && ! grep -qx '  Tailscale' $evidence/04-update.log"
check "pacman hook set the computer up again" R 'grep "ibara-refresh.hook" /var/log/pacman.log | tail -2 && [ "$(grep -c "running .ibara-refresh.hook" /var/log/pacman.log)" -ge 2 ]'
check "both releases kept for rollback, three packages each" R 'ls /var/cache/ibara/packages; [ "$(ls /var/cache/ibara/packages | wc -l)" = 6 ]'
check "journals copied before the update" A "ls -d ~/.local/state/ibara/backups/before-$v1-*"
check "user services running again" A 'systemctl --user is-active agent-computer.service ibara-operator.service'
check "console shows What's New for the new version" A "python3 /usr/local/bin/console-ask whats-new | tee /dev/stderr | jq -e '.envelope.data.version | endswith(\"$v2\")'"
check "Got It marks it seen" A 'python3 /usr/local/bin/console-ask whats-new-seen | jq -e ".envelope.error == null"'
check "What's New shows only once" A 'python3 /usr/local/bin/console-ask whats-new | jq -e ".envelope.data.version == null"'
check "same plugin, no shell restart" A '[ "$(grep -c omarchy-restart-shell ~/omarchy-stub.log)" = 1 ]'

step "5. ibara rollback over a journal the newer version moved on"
A 'systemctl --user stop agent-computer.service && sqlite3 ~/.local/state/agent-computer/journal.sqlite "UPDATE meta SET value = '"'"'99'"'"' WHERE key = '"'"'core_schema_version'"'"'"' >>"$evidence/checks.log" 2>&1
# A friend's computer was paired before the update and removed after it: the
# copy made before the update has it paired, the newer journal ended.
{ echo "v1=$v1"; cat <<'SQL'; } >"$evidence/05-access.sql.sh"
set -e
before=$(ls -d ~/.local/state/ibara/backups/before-$v1-* | tail -1)/journal.sqlite
sqlite3 "$before" "UPDATE meta SET value = json_set(value, '$.identities.friend', json('{\"kind\":\"computer\",\"computer\":\"friend\",\"key\":\"SHA256:friend\"}'), '$.pairings.friend', json('{\"key\":\"SHA256:friend\",\"endpoint\":null,\"active\":true,\"generation\":3}'), '$.grants.\"friend:watch\"', json('{\"subject\":\"friend\",\"capability\":\"watch\",\"rule\":\"allow\"}')) WHERE key = 'access_model'"
sqlite3 ~/.local/state/agent-computer/journal.sqlite "UPDATE meta SET value = json_set(value, '$.identities.friend', json('{\"kind\":\"computer\",\"computer\":\"friend\",\"key\":\"SHA256:friend\"}'), '$.pairings.friend', json('{\"key\":\"SHA256:friend\",\"endpoint\":null,\"active\":false,\"generation\":4}')) WHERE key = 'access_model'"
sqlite3 "$before" "SELECT json_extract(value, '$.pairings.friend.active'), json_extract(value, '$.grants.\"friend:watch\".rule') FROM meta WHERE key = 'access_model'"
SQL
A 'bash -s' <"$evidence/05-access.sql.sh" >>"$evidence/checks.log" 2>&1
run 05-rollback.log 'ibara rollback'
check "rollback finished" grep -q '^exit=0' "$evidence/05-rollback.log"
check "earlier ibara, ibara-stream and ibara-view installed again" R "[ \"\$(pacman -Q ibara ibara-stream ibara-view | grep -c -- ' $v1\$')\" = 3 ]"
check "the friend's computer removed after the update stays removed" A 'sqlite3 ~/.local/state/agent-computer/journal.sqlite "SELECT json_extract(value, '"'"'$.pairings.friend.active'"'"'), json_type(value, '"'"'$.grants.\"friend:watch\"'"'"') IS NULL FROM meta WHERE key = '"'"'access_model'"'"'" | tee /dev/stderr | grep -qx "0|1"'
check "journal from before the update put back, newer one kept" A 'q="SELECT value FROM meta WHERE key = '"'"'core_schema_version'"'"'"; before=$(sqlite3 "$(ls -d ~/.local/state/ibara/backups/before-'"$v1"'-* | tail -1)/journal.sqlite" "$q"); now=$(sqlite3 ~/.local/state/agent-computer/journal.sqlite "$q"); kept=$(sqlite3 "$(ls -d ~/.local/state/ibara/rolled-back-* | tail -1)/journal.sqlite" "$q"); echo "before=$before now=$now kept=$kept"; [ -n "$before" ] && [ "$now" = "$before" ] && [ "$kept" = 99 ]'
check "controller starts on the restored journal" A 'sleep 3; systemctl --user is-active agent-computer.service'

step "6. ibara uninstall (keeping this computer's identity)"
run 06-uninstall.log 'ibara uninstall --yes'
check "uninstall finished" grep -q '^exit=0' "$evidence/06-uninstall.log"
check "packages, units, links and Cua plugin gone" R '! pacman -Q ibara && ! pacman -Q ibara-stream && ! pacman -Q ibara-view && ! systemctl cat ibara-agent-sshd.service >/dev/null 2>&1 && [ ! -e /opt/agent-computer/current ] && [ ! -e /usr/local/sbin/ibara-op-shell ] && [ ! -e /opt/agent-computer/cua ]'
check "no ibara firewall rules left" R '! ufw status | grep -q ibara'
check "plugin link and Hyprland include gone" A '[ ! -e ~/.config/omarchy/plugins/io.zet.ibara ] && ! grep -q agent-computer ~/.config/hypr/hyprland.lua && ! systemctl --user cat agent-computer.service >/dev/null 2>&1'
check "identity kept for a later install" R 'test -f /etc/agent-computer/ssh/host_ed25519 && test -f /etc/ibara/station.json'

step "7. The marketplace route: the plugin first"
sudo rm -rf /var/lib/machines/ibara-e2e/srv/plugin.git
sudo git clone -q --bare "$plugin" /var/lib/machines/ibara-e2e/srv/plugin.git
check "the plugin cloned into Omarchy's plugin folder, as the marketplace does" A 'git -c safe.directory="*" clone -q /srv/plugin.git ~/.config/omarchy/plugins/io.zet.ibara'
# The console's Install ibara button: the install command from the plugin, in Omarchy's floating terminal.
command=$(sed -n 's/.*readonly property string ibaraBaseUrl: "\(.*\)"/curl -fsSL \1\/install | sh/p' "$plugin/Service.qml")
check "the plugin names the install command" test -n "$command"
run 07-marketplace.log "omarchy-launch-floating-terminal-with-presentation '$command'"
check "install from the console's command finished" grep -q '^exit=0' "$evidence/07-marketplace.log"
check "marketplace copy set aside, packaged plugin linked" A 'ls -d ~/.config/omarchy/plugins/.io.zet.ibara.marketplace-* && [ "$(readlink ~/.config/omarchy/plugins/io.zet.ibara)" = /usr/share/ibara/omarchy-plugin ]'
R 'ssh-keygen -lf /etc/agent-computer/ssh/host_ed25519.pub' >"$evidence/host-key-2.txt"
check "same host key as before uninstalling" diff "$evidence/host-key-1.txt" "$evidence/host-key-2.txt"
check "services running again" A 'systemctl --user is-active agent-computer.service ibara-operator.service'

step "8. ibara uninstall --delete-data"
run 08-uninstall-all.log 'ibara uninstall --delete-data --yes'
check "uninstall finished" grep -q '^exit=0' "$evidence/08-uninstall-all.log"
check "no ibara files, accounts or state left" R '! pacman -Q ibara && ! pacman -Q ibara-stream && ! pacman -Q ibara-view && for p in /etc/agent-computer /etc/ibara-operator /etc/ibara/station.json /opt/agent-computer /var/cache/ibara /var/lib/ibara-operator /home/alice/.local/state/ibara /home/alice/.local/state/agent-computer /home/alice/.ssh/ibara_agent_ed25519; do [ ! -e $p ] || { echo left: $p; exit 1; }; done; ! getent passwd ibara-agent && ! getent group ibara-runtime && ! getent passwd | grep ibara-op-'

step "9. No Node at any point"
check "the node watcher ran from before the first install to the end" R 'systemctl is-active ibara-e2e-node-watch.service'
R 'systemctl stop ibara-e2e-node-watch.service'
R 'cat /var/log/ibara-e2e-node-watch.log' >"$evidence/node-watch.log" 2>&1
check "no node process at any point" test ! -s "$evidence/node-watch.log"
check "nodejs is still not installed" R '! pacman -Qq | grep -q "^nodejs" && ! command -v node'

printf '\n%s passed, %s failed\n' "$pass" "$fail" | tee -a "$evidence/summary"
[ "$fail" -eq 0 ]
