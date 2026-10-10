pragma Singleton
import QtQuick

QtObject {
  readonly property SystemPalette nativePalette: SystemPalette { colorGroup: SystemPalette.Active }
  readonly property color foreground: nativePalette.windowText
  readonly property color background: nativePalette.window
  readonly property color accent: nativePalette.highlight
  readonly property color muted: Qt.alpha(foreground, 0.55)
  readonly property color urgent: nativePalette.window.hslLightness > 0.5 ? "#b3261e" : "#ffb4ab"
  readonly property string currentThemePath: ""
  readonly property var popups: ({ text: foreground, background: background })
  readonly property var bar: ({ background: background })
  readonly property var tooltip: ({ text: foreground, background: background, border: muted })
}
