pragma Singleton
import QtQuick

QtObject {
  // Keep the existing Console geometry; follow the desktop's configured font.
  readonly property var font: ({ family: Qt.application.font.family, body: 14, bodySmall: 12, caption: 11, heading: 18, title: 22 })
  readonly property var spacing: ({ controlPaddingX: space(10), controlPaddingY: space(6) })
  readonly property var bar: ({ iconCanvas: 20, iconSlot: 32 })
  readonly property real normalBorderWidth: 1
  function space(value) { return Math.round(value * Math.max(1, Qt.application.font.pixelSize / 14)) }
  function focusFillFor(ink, accent) { return Qt.alpha(accent, 0.18) }
  function hoverFillFor(ink, accent) { return Qt.alpha(ink, 0.12) }
  function selectedFillFor(ink, accent) { return Qt.alpha(accent, 0.16) }
  function selectedStateColor(ink, accent) { return ink }
}
