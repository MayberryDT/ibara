import QtQuick
import QtQuick.Controls

Switch {
  readonly property SystemPalette nativePalette: SystemPalette {}
  property color foreground: nativePalette.text
  property color accent: nativePalette.highlight
  palette.text: foreground
  palette.highlight: accent
}
