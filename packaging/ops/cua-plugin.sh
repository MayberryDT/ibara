#!/usr/bin/env bash
# Build Cua's Hyprland plugin for this computer's exact Hyprland and GCC,
# install it under /opt/agent-computer/cua, and load it in the desktop session.
#
#   /usr/lib/ibara/ops/cua-plugin.sh PLUGIN_SOURCE DESKTOP_USER   (root)
#
# `ibara setup` runs it, and `ibara system refresh` (the package's pacman
# hook) runs it again after Hyprland, GCC or ibara changed. Root writes only
# under /opt/agent-computer/cua. Everything the desktop user owns (the private
# build directory, hyprland.lua and its preimage) is read and written as that
# user, so a link the user placed there never leads a root write elsewhere.
# The user's hyprland.lua gains one line that loads
# /opt/agent-computer/cua/hyprland.lua, which loads the plugin and gives a
# computer without a display its headless output at session start. A module
# that is already loaded stays in the running session (the new file replaces
# the old one on disk only); the next sign-in loads the new build.
set -euo pipefail
source=${1:?plugin source directory}
desktop=${2:?desktop user}
[[ $(id -u) == 0 ]] || { echo 'Run as root (sudo).' >&2; exit 1; }
[[ -f $source/CMakeLists.txt && -f $source/src/input_experiment.cpp ]] || { echo 'Not the plugin source.' >&2; exit 1; }
for tool in cmake ninja g++ pkg-config; do command -v "$tool" >/dev/null || { echo "Missing $tool (pacman: cmake ninja gcc)." >&2; exit 1; }; done
uid=$(id -u "$desktop")
home=$(getent passwd "$desktop" | cut -d: -f6)
target=/opt/agent-computer/cua
hypr_version=$(pkg-config --modversion hyprland)
as_user() { runuser -u "$desktop" -- "$@"; }

work=$(as_user mktemp -d)
trap 'as_user rm -rf "$work"' EXIT
as_user mkdir "$work/src"
# Root reads the source; the user writes the copy into its own directory.
tar -C "$source" -cf - . | as_user tar -C "$work/src" -xf -
as_user cmake -S "$work/src" -B "$work/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_TESTING=OFF -DCUA_HYPRLAND_INPUT=ON -DCUA_HYPRLAND_EXPECTED_VERSION="$hypr_version" >/dev/null
as_user cmake --build "$work/build" -j 2 >/dev/null

install -d -m 0755 "$target"
staged=$target/cua-hyprland-plugin.so.new
# The user reads the module out of its build tree; root writes the copy.
as_user cat "$work/build/cua-hyprland-plugin.so" >"$staged" || { rm -f "$staged"; echo 'Build produced no module.' >&2; exit 1; }
chmod 0644 "$staged"

bash "$(dirname "$0")/desktop-config.sh" "$desktop"

session() {
  local signature
  signature=$(runuser -u "$desktop" -- env XDG_RUNTIME_DIR="/run/user/$uid" hyprctl -j instances 2>/dev/null | sed -n 's/.*"instance": *"\([^"]*\)".*/\1/p' | head -1)
  [[ -n $signature ]] || return 1
  runuser -u "$desktop" -- env XDG_RUNTIME_DIR="/run/user/$uid" HYPRLAND_INSTANCE_SIGNATURE="$signature" "$@"
}
loaded=$(session hyprctl -j plugin list 2>/dev/null | grep -c '"cua-hyprland-plugin"' || true)
new_sha=$(sha256sum "$staged" | cut -d' ' -f1)
# Renamed into place: a running session keeps the module it loaded.
mv -f "$staged" "$target/cua-hyprland-plugin.so"
rm -f "$target/cua-hyprland-plugin.so.next"
printf '{"hyprland":"%s","gcc":"%s","source":"%s","sha256":"%s","built_at":"%s"}\n' \
  "$(pacman -Q hyprland | cut -d' ' -f2)" "$(pacman -Q gcc | cut -d' ' -f2)" "${IBARA_CUA_SOURCE:-}" "$new_sha" "$(date -u +%FT%TZ)" >"$target/build.json"

hypr_built=$(pacman -Q hyprland | cut -d' ' -f2)
if ! session hyprctl version >/dev/null 2>&1; then
  echo "Built for Hyprland $hypr_built; it loads when you next sign in."
elif [[ $loaded == 0 ]]; then
  session hyprctl reload >/dev/null
  sleep 1
  # Loading is not enough: a session that already loaded and unloaded the
  # plugin (an uninstall, then an install) keeps it from taking the input seat,
  # and agents can't click or type until the person signs in again.
  status=$(session hyprctl -j cua:status 2>/dev/null || true)
  if [[ -n $status ]] && jq -e '.transport.ready == true' <<<"$status" >/dev/null 2>&1; then
    echo "Built for Hyprland $hypr_built and loaded."
  elif [[ -n $status ]]; then
    echo "Built for Hyprland $hypr_built. Sign out and back in to finish: until then agents can't click or type on this computer."
  else
    echo "Built for Hyprland $hypr_built; it loads when you next sign in."
  fi
else
  echo "Built for Hyprland $hypr_built; the running session keeps the plugin it loaded until you next sign in."
fi
