#!/usr/bin/env bash
# Install ibara's virtual screen independently of the Cua plugin build.
set -euo pipefail
desktop=${1:?desktop user}
[[ $(id -u) == 0 ]] || { echo 'Run as root (sudo).' >&2; exit 1; }
home=$(getent passwd "$desktop" | cut -d: -f6)
target=/opt/agent-computer/cua
as_user() { runuser -u "$desktop" -- "$@"; }
install -d -m 0755 "$target"
cat >"$target/hyprland.lua.new" <<'LUA'
-- ibara, installed by desktop-config.sh. The user's configuration only
-- includes this file.
--
-- Cua's Hyprland plugin, built on this computer for its exact Hyprland and
-- GCC, is loaded and its input transport opened. Replacing a loaded plugin
-- needs a new desktop session.
local plugin = "/opt/agent-computer/cua/cua-hyprland-plugin.so"
local present = io.open(plugin, "rb")
if present then
  present:close()
  if hl.plugin and hl.plugin.load then
    pcall(hl.plugin.load, plugin)
    pcall(hl.config, { plugin = { cua = { enabled = true } } })
  end
end

-- A computer without a display gets its headless output, IbaraVirtual, as
-- Hyprland starts, before session services look for a screen: Sunshine
-- started without any output falls back to the screencast portal, whose share
-- picker takes the keyboard focus from every window. At hyprland.start
-- Hyprland lists its own placeholder, FALLBACK, which is not a display.
-- Afterwards ibarad removes IbaraVirtual when a physical output appears and
-- recreates it when the last one goes.
hl.monitor({ output = "IbaraVirtual", mode = "1920x1080@30", position = "0x0", scale = 1 })
hl.on("hyprland.start", function()
  for _, m in ipairs(hl.get_monitors()) do
    if m.name ~= "FALLBACK" then
      return
    end
  end
  hl.exec_cmd("hyprctl output create headless IbaraVirtual")
end)
LUA
chmod 0644 "$target/hyprland.lua.new"
mv -f "$target/hyprland.lua.new" "$target/hyprland.lua"

config=$home/.config/hypr/hyprland.lua
if as_user test -f "$config" && ! as_user grep -q '/opt/agent-computer/cua/hyprland.lua' "$config"; then
  as_user install -d -m 0700 "$home/.local/state/ibara"
  as_user cp -p "$config" "$home/.local/state/ibara/hyprland.lua.pre-cua"
  printf '\n-- ibara: desktop input and virtual screen (see /opt/agent-computer/cua/hyprland.lua).\npcall(dofile, "/opt/agent-computer/cua/hyprland.lua")\n' | as_user tee -a "$config" >/dev/null
fi
