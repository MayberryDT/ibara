import QtQuick
import qs.Commons

// One line of text whose new value slides up over the old one (380 ms): a card's current step
// and the header's counts. Width follows the text unless the owner sets it; then it elides.
Item {
  id: root
  property string text: ""
  property int textFormat: Text.PlainText
  property string accessibleText: text
  property color color: Color.popups.text
  property int size: Style.font.body
  property bool bold: false
  // False: a new value just replaces the old one.
  property bool rolls: true
  implicitWidth: Math.max(cur.implicitWidth, 1)
  implicitHeight: cur.implicitHeight
  clip: true
  Accessible.role: Accessible.StaticText
  Accessible.name: accessibleText
  Text {
    id: old
    width: root.width
    color: root.color
    font.family: Style.font.family
    font.pixelSize: root.size
    font.bold: root.bold
    textFormat: root.textFormat
    elide: Text.ElideRight
    opacity: 0
    Accessible.ignored: true
  }
  Text {
    id: cur
    width: root.width
    color: root.color
    font.family: Style.font.family
    font.pixelSize: root.size
    font.bold: root.bold
    textFormat: root.textFormat
    elide: Text.ElideRight
    Accessible.ignored: true
  }
  Component.onCompleted: cur.text = text
  onTextChanged: {
    if (text === cur.text) return
    old.text = cur.text
    cur.text = text
    if (visible && rolls) roll.restart()
    else { roll.stop(); old.opacity = 0; cur.y = 0; cur.opacity = 1 }
  }
  ParallelAnimation {
    id: roll
    NumberAnimation { target: old; property: "y"; from: 0; to: -root.height; duration: 380; easing.type: Easing.OutCubic }
    NumberAnimation { target: old; property: "opacity"; from: 1; to: 0; duration: 380 }
    NumberAnimation { target: cur; property: "y"; from: root.height; to: 0; duration: 380; easing.type: Easing.OutCubic }
    NumberAnimation { target: cur; property: "opacity"; from: 0; to: 1; duration: 380 }
  }
}
