import QtQuick
import qs.Commons

// One toast: the frame every console message has, in the stack over the page's bottom-right
// corner (Toasts.qml) and at the bottom of the quick panel. The content is a column beside a
// colored edge; ✕ dismisses what may be dismissed. A toast with a `life` leaves by itself after
// it, but never while the pointer or the keyboard is on it, it holds open details, or it is hidden
// behind "+N more". A toast without one stays until it is answered, dismissed or its condition
// clears. `firstControl` is where the keyboard lands when the stack is focused.
FocusScope {
  id: root
  property color edge: Color.accent
  // Errors and questions: a wash of the edge color, and the edge color on the border.
  property bool tinted: false
  property bool dismissable: true
  property int life: 0
  property bool holding: false
  // Whether keyboard focus in it stops its clock: Toasts says so only for focus the person moved
  // there with the keyboard, not for a click or focus the console gave it.
  property bool focusHolds: true
  property string dismissName: "Dismiss this message"
  property Item firstControl: null
  // Request cards also render in the service's desktop stack, with the same actions.
  property bool popup: false
  signal bodyClicked()
  default property alias toastBody: body.data
  signal dismissed()
  signal expired()

  function restartClock() { if (clock.running) clock.restart() }
  function focusFirst() {
    var item = firstControl && firstControl.visible ? firstControl : dismiss.visible ? dismiss : null
    if (!item) return false
    item.forceActiveFocus()
    return true
  }

  width: parent ? parent.width : 0
  implicitHeight: Math.max(Style.space(44), body.implicitHeight + Style.space(20))
  Accessible.role: Accessible.AlertMessage

  Rectangle {
    anchors.fill: parent
    radius: 0
    color: Color.popups.background
    border.width: 1
    border.color: root.tinted ? Qt.alpha(root.edge, 0.7) : Qt.alpha(Color.popups.text, 0.24)
  }
  Rectangle { anchors.fill: parent; anchors.margins: 1; radius: 0; color: Qt.alpha(root.tinted ? root.edge : Color.popups.text, root.tinted ? 0.10 : 0.04) }
  Rectangle { x: 1; y: 1; width: Style.space(3); height: parent.height - 2; radius: 0; color: root.edge }
  MouseArea {
    anchors.fill: parent
    enabled: root.popup
    cursorShape: Qt.PointingHandCursor
    onClicked: root.bodyClicked()
  }
  Column {
    id: body
    x: Style.space(16)
    // One line sits in the middle; more start at the top.
    y: Math.max(Style.space(10), Math.round((root.height - implicitHeight) / 2))
    width: root.width - x - (dismiss.visible ? dismiss.width + Style.space(12) : Style.space(12))
    spacing: Style.space(6)
  }
  ActionButton {
    id: dismiss
    visible: root.dismissable
    anchors.right: parent.right
    anchors.rightMargin: Style.space(6)
    y: Math.min(Style.space(9), Math.round((root.height - height) / 2))
    label: "✕"
    role: "quiet"
    size: "small"
    Accessible.name: root.dismissName
    tooltipText: root.popup ? "Later · Keep the request in the console" : "Dismiss (Escape)"
    onClicked: root.dismissed()
  }
  HoverHandler { id: hover }
  Timer {
    id: clock
    running: root.life > 0 && root.visible && !hover.hovered && !(root.activeFocus && root.focusHolds) && !root.holding
    interval: root.life
    onTriggered: root.popup ? root.expired() : root.dismissed()
  }
  // Arrives from the right, where the stack sits, and settles.
  opacity: 0
  transform: Translate {
    id: slide
    x: Style.space(28)
    Behavior on x { NumberAnimation { duration: 180; easing.type: Easing.OutCubic } }
  }
  Component.onCompleted: { opacity = 1; slide.x = 0 }
  Behavior on opacity { NumberAnimation { duration: 160 } }
}
