import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// Files dropped on a computer's fleet card, on their way there (Service.drops), over the bottom of
// its picture: which one is going, how many have gone, and what didn't. It leaves by itself a few
// seconds after everything arrived; a failure stays until dismissed. The computer's own page says
// the same in a toast (Toasts.qml).
Rectangle {
  id: root
  property var service: null
  property string computerId: ""
  readonly property Tokens tokens: Tokens {}
  readonly property var drop: service && service.drops[computerId] ? service.drops[computerId] : null
  readonly property int total: drop ? drop.files.length : 0
  readonly property bool going: !!drop && drop.state !== "done"
  readonly property bool failed: !!drop && drop.failed.length > 0
  readonly property real done: !drop ? 0 : drop.state === "done" ? 1 : total ? Math.min(1, (drop.index + 0.5) / total) : 0
  readonly property string message: drop && service ? StatusModel.dropMessage(drop, service.computerLabelFor(computerId)) : ""

  visible: !!drop
  width: parent ? parent.width : 0
  height: visible ? Math.max(Style.space(30), line.implicitHeight + Style.space(12)) : 0
  radius: 0
  color: Qt.alpha(Color.popups.background, 0.92)
  Accessible.role: Accessible.StatusBar
  Accessible.name: message

  Copy {
    id: line
    anchors.left: parent.left
    anchors.leftMargin: Style.space(8)
    anchors.right: dismiss.visible ? dismiss.left : parent.right
    anchors.rightMargin: Style.space(8)
    anchors.verticalCenter: parent.verticalCenter
    text: root.message
    color: root.failed && !root.going ? Color.urgent : Color.popups.text
    font.pixelSize: Style.font.bodySmall
    wrapMode: Text.NoWrap
    maximumLineCount: 1
    elide: Text.ElideRight
  }
  ActionButton {
    id: dismiss
    visible: !root.going && root.failed
    anchors.right: parent.right
    anchors.rightMargin: Style.space(4)
    anchors.verticalCenter: parent.verticalCenter
    label: "✕"
    role: "quiet"
    size: "small"
    Accessible.name: "Dismiss"
    tooltipText: "Dismiss"
    onClicked: if (root.service) root.service.dismissDrop(root.computerId)
  }
  // How far along the files are, along the bottom edge.
  Rectangle {
    anchors.bottom: parent.bottom
    width: parent.width * root.done
    height: Math.max(2, Style.space(2))
    radius: 0
    color: root.failed ? Color.urgent : root.going ? Color.accent : root.tokens.readyColor
    Behavior on width { NumberAnimation { duration: 200 } }
  }
}
