import QtQuick
import qs.Commons

// A fleet state's mark, in its color: a small square, a person when a person has control, and
// Omarchy's agent robot while agents work, so color is never the only cue. Always paired with
// text; color alone proves nothing. Every mark takes the same width, so names beside marks line
// up. Owners pass their own `tokens`; a marker given none builds one for itself.
Item {
  id: root
  property string fleetState: "ready"
  property real size: Style.space(9)
  property Tokens tokens: null
  // Only the square, for a page that borrows the state colors for steps of its own (Add Computer).
  property bool plain: false
  readonly property color color: tokens ? tokens.stateColor(fleetState) : "transparent"
  // Nerd Font glyphs (Omarchy's monospace font): Material Design account and robot.
  readonly property string glyph: plain ? "" : fleetState === "human" ? "\u{F0004}" : fleetState === "working" ? "\u{F16A3}" : ""
  width: Math.round(size * 1.6)
  height: size
  Accessible.ignored: true
  // An agent at work breathes: the mark dims and swells every two seconds, the one moving thing
  // on a still wall. A change of state fades into its new color and pops once.
  Item {
    id: mark
    anchors.fill: parent
    SequentialAnimation on opacity {
      running: !root.plain && root.fleetState === "working" && root.visible
      loops: Animation.Infinite
      alwaysRunToEnd: true
      NumberAnimation { to: 0.2; duration: 1000; easing.type: Easing.InOutSine }
      NumberAnimation { to: 1; duration: 1000; easing.type: Easing.InOutSine }
    }
    SequentialAnimation on scale {
      running: !root.plain && root.fleetState === "working" && root.visible
      loops: Animation.Infinite
      alwaysRunToEnd: true
      NumberAnimation { to: 1.25; duration: 1000; easing.type: Easing.InOutSine }
      NumberAnimation { to: 1; duration: 1000; easing.type: Easing.InOutSine }
    }
    Item {
      anchors.fill: parent
      NumberAnimation on scale { id: statePop; running: false; from: 1.6; to: 1; duration: 350; easing.type: Easing.OutCubic }
      Rectangle {
        visible: root.glyph === ""
        anchors.centerIn: parent
        width: root.size
        height: root.size
        radius: 0
        color: root.color
        Behavior on color { ColorAnimation { duration: 400 } }
      }
      Text {
        visible: root.glyph !== ""
        anchors.centerIn: parent
        text: root.glyph
        color: root.color
        font.family: Style.font.family
        font.pixelSize: Math.round(root.size * 1.6)
        Behavior on color { ColorAnimation { duration: 400 } }
      }
    }
  }
  onFleetStateChanged: if (!plain && visible) statePop.restart()
  Component.onCompleted: if (!tokens) tokens = Qt.createComponent("Tokens.qml").createObject(root)
}
