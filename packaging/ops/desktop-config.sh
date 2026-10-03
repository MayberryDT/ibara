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
-- GCC, is loaded once per desktop session at hyprland.start, at runtime, and
-- its input transport opened. Hyprland unloads a plugin declared with
-- hl.plugin.load whenever one config reload misses the declaration (a
-- config folder replaced non-atomically, a module missing for a moment);
-- loading it again then fails for the rest of the session ("input seat
-- lifetime unavailable"). A runtime-loaded plugin is never unloaded by a
-- reload. A session that loaded the plugin the old way (its seat marker
-- exists but not ours) keeps declaring it until the next sign-in, or the
-- next reload would unload it.
local plugin = "/opt/agent-computer/cua/cua-hyprland-plugin.so"
local present = io.open(plugin, "rb")
if present then
  present:close()
  local runtime, signature = os.getenv("XDG_RUNTIME_DIR"), os.getenv("HYPRLAND_INSTANCE_SIGNATURE")
  local dir = runtime and signature and (runtime .. "/hypr/" .. signature) or nil
  local function exists(path)
    local f = path and io.open(path, "r")
    if f then f:close() end
    return f ~= nil
  end
  local ours = dir and dir .. "/ibara-cua-runtime-load"
  if dir and exists(dir .. "/cua-input-seat-lifetime") and not exists(ours) then
    if hl.plugin and hl.plugin.load then
      pcall(hl.plugin.load, plugin)
    end
  else
    hl.on("hyprland.start", function()
      if ours then
        local f = io.open(ours, "w")
        if f then f:close() end
      end
      hl.exec_cmd("hyprctl plugin load " .. plugin)
    end)
  end
  pcall(hl.config, { plugin = { cua = { enabled = true } } })
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
