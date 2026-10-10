import QtQuick
import Quickshell.Io
import qs.Commons
import "Palette.js" as Palette

// The Omarchy theme's named colors that Color does not expose (green, yellow, blue, magenta,
// cyan and their bright forms), read from the current theme's colors.toml. Each missing color
// falls back to the theme's accent. `omarchy-theme-set` replaces the theme folder and then pushes
// the new colors into Color, so any change there re-reads the file. One instance serves every
// Tokens (Palette.js).
QtObject {
  id: root
  property var values: ({})
  readonly property color green: pick(["green", "color2"])
  readonly property color yellow: pick(["yellow", "color3"])
  readonly property color blue: pick(["blue", "color4"])
  readonly property color magenta: pick(["magenta", "color5"])
  readonly property color cyan: pick(["cyan", "color6"])
  readonly property color brightBlue: pick(["bright_blue", "color12"])
  readonly property color brightCyan: pick(["bright_cyan", "color14"])
  // A person in control is magenta. Agents working take the first of blue, cyan, bright cyan,
  // bright blue, the accent and the text color that is clearly apart (CIEDE2000 12 or more) from
  // magenta and from the other states' colors (red, yellow, green, muted): blue in most themes,
  // cyan in Permafrost, where blue was 8 from magenta. In the near-gray White and Vantablack,
  // where no color is, the one that differs most (about 11); the marks' shapes tell them apart.
  readonly property color human: magenta
  readonly property color working: Palette.distinct([magenta, Color.urgent, yellow, green, Color.muted].map(String),
    [blue, cyan, brightCyan, brightBlue, Color.accent, Color.foreground].map(String), 12) || Color.accent

  function pick(names) {
    for (var i = 0; i < names.length; i++) if (values[names[i]]) return values[names[i]]
    return Color.accent
  }
  function parse(raw) {
    var found = ({})
    var lines = String(raw || "").split("\n")
    for (var i = 0; i < lines.length; i++) {
      var match = lines[i].match(/^\s*([A-Za-z0-9_-]+)\s*=\s*["']?(#[0-9A-Fa-f]{6})\b/)
      if (match) found[match[1]] = match[2]
    }
    return found
  }
  function reload() { file.reload() }

  readonly property string themeSignature: String(Color.foreground) + String(Color.background) + String(Color.accent) + String(Color.urgent) + String(Color.muted)
  onThemeSignatureChanged: Qt.callLater(root.reload)

  property FileView file: FileView {
    path: Color.currentThemePath + "/colors.toml"
    watchChanges: false
    printErrors: false
    onLoaded: root.values = root.parse(text())
    onLoadFailed: root.values = ({})
  }
}
