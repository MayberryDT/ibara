import QtQuick
import QtQuick.Controls

Row {
  id: root
  property var options: []
  property string value: ""
  property bool focusable: true
  property color foreground: palette.text
  property color background: palette.button
  property color accent: palette.highlight
  property real fontSize: 14
  signal changed(string value)
  SystemPalette { id: palette }
  spacing: 6
  Repeater {
    model: root.options
    delegate: Button {
      required property var modelData
      text: String(modelData.label || modelData.value)
      checkable: true
      checked: String(modelData.value) === root.value
      focusPolicy: root.focusable ? Qt.StrongFocus : Qt.NoFocus
      font.pixelSize: root.fontSize
      Accessible.name: text
      onClicked: root.changed(String(modelData.value))
    }
  }
}
