import QtQuick
import qs.Commons
import "Reveal.js" as Reveal

// A square single-line text field. Tab reaches the input; focus shows an accent border.
Rectangle {
  id: root
  property alias text: input.text
  property alias input: input
  property alias maximumLength: input.maximumLength
  property string placeholder: ""
  property string accessibleName: placeholder
  property string hint: ""
  property bool monospace: false
  property real fontSize: Style.font.body
  signal accepted()
  signal edited()
  signal editingFinished()
  readonly property color foreground: Color.popups.text
  function focusInput() { input.forceActiveFocus() }
  implicitWidth: Style.space(220)
  implicitHeight: Style.space(32)
  radius: 0
  color: Qt.alpha(foreground, 0.05)
  border.width: input.activeFocus ? Math.max(1, Style.space(2)) : 1
  border.color: input.activeFocus ? Color.accent : Qt.alpha(foreground, 0.22)

  TextInput {
    id: input
    anchors.fill: parent
    anchors.leftMargin: Style.space(10)
    anchors.rightMargin: Style.space(10) + (hintText.visible ? hintText.width + Style.space(6) : 0)
    verticalAlignment: TextInput.AlignVCenter
    clip: true
    activeFocusOnTab: true
    selectByMouse: true
    color: root.foreground
    selectionColor: Qt.alpha(Color.accent, 0.45)
    font.family: root.monospace ? "monospace" : Style.font.family
    font.pixelSize: root.fontSize
    Accessible.role: Accessible.EditableText
    Accessible.name: root.accessibleName
    onAccepted: root.accepted()
    onTextEdited: root.edited()
    onEditingFinished: root.editingFinished()
    onActiveFocusChanged: if (activeFocus) Reveal.reveal(root)
  }
  Text {
    anchors.left: parent.left
    anchors.leftMargin: Style.space(10)
    anchors.verticalCenter: parent.verticalCenter
    visible: !input.text
    text: root.placeholder
    color: Qt.alpha(root.foreground, 0.45)
    font.family: Style.font.family
    font.pixelSize: root.fontSize
    Accessible.ignored: true
  }
  Rectangle {
    id: hintText
    visible: root.hint !== ""
    anchors.right: parent.right
    anchors.rightMargin: Style.space(8)
    anchors.verticalCenter: parent.verticalCenter
    width: hintLabel.implicitWidth + Style.space(8)
    height: hintLabel.implicitHeight + Style.space(2)
    radius: 0
    color: "transparent"
    border.width: 1
    border.color: Qt.alpha(root.foreground, 0.3)
    Text {
      id: hintLabel
      anchors.centerIn: parent
      text: root.hint
      color: Qt.alpha(root.foreground, 0.55)
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      Accessible.ignored: true
    }
  }
}
