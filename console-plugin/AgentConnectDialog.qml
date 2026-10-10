import QtQuick
import QtQuick.Controls
import qs.Commons

// Connect an Agent from the fleet's toolbar: the prompt and Copy Prompt (AgentConnect.qml) in a
// card attached to the button that opened it, in the same style as a confirmation
// (NearbyConfirm.qml): under the button, or over it when there is no room below, with a notch
// pointing at it. Copy Prompt has focus; Tab moves between Close and Copy Prompt; Escape, Close,
// a click elsewhere or the button again closes it. Copying says Copied on the button, then closes it.
Popup {
  id: root
  property var host: null
  readonly property bool wanted: !!host && host.connectOpen
  readonly property real gap: Style.space(3)
  readonly property real notch: Style.space(10)
  readonly property real tip: Math.ceil(notch * Math.SQRT1_2)
  property Item anchorItem: null
  property bool above: false

  function show() {
    var item = host ? host.connectAnchor : null
    if (!item || !item.visible || !item.Window.window) { if (host) host.closeConnect(); return }
    anchorItem = item
    parent = item
    place()
    if (!opened) open()
    prompt.focusDefault()
  }
  function place() {
    if (!anchorItem) return
    var at = anchorItem.mapToItem(null, 0, 0)
    var windowWidth = anchorItem.Window.width, windowHeight = anchorItem.Window.height
    var height = implicitHeight
    above = at.y + anchorItem.height + gap + height > windowHeight - margins && at.y - gap - height >= margins
    y = above ? -height - gap : anchorItem.height + gap
    x = at.x + width > windowWidth - margins ? Math.max(margins - at.x, anchorItem.width - width) : 0
  }

  width: Math.min(Style.space(600), anchorItem ? anchorItem.Window.width - margins * 2 : Style.space(600))
  margins: Style.space(8)
  padding: Style.space(14)
  topPadding: padding + (above ? 0 : tip)
  bottomPadding: padding + (above ? tip : 0)
  focus: true
  modal: false
  dim: false
  closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutsideParent
  transformOrigin: above ? Item.Bottom : Item.Top
  enter: Transition {
    NumberAnimation { property: "opacity"; from: 0; to: 1; duration: 120; easing.type: Easing.OutCubic }
    NumberAnimation { property: "scale"; from: 0.96; to: 1; duration: 120; easing.type: Easing.OutCubic }
  }
  onOpened: prompt.focusDefault()
  onClosed: if (host && host.connectOpen) host.closeConnect()
  onParentChanged: if (!parent && host && host.connectOpen) host.closeConnect()
  onWantedChanged: if (wanted) Qt.callLater(show); else close()
  onImplicitHeightChanged: if (opened) place()

  background: Item {
    Item {
      width: parent.width
      y: root.above ? 0 : root.tip
      height: parent.height - root.tip
      Rectangle { anchors.fill: parent; radius: 0; color: Color.popups.background }
      Rectangle { anchors.fill: parent; radius: 0; color: Qt.alpha(Color.accent, 0.08) }
      Rectangle { anchors.fill: parent; radius: 0; color: "transparent"; border.width: 1; border.color: Color.accent }
    }
    Item {
      clip: true
      width: root.notch * 2
      height: root.tip + 1
      x: root.anchorItem ? Math.max(Style.space(8), Math.min(parent.width - width - Style.space(8), root.anchorItem.width / 2 - root.x - width / 2)) : Style.space(8)
      y: root.above ? parent.height - root.tip - 1 : 0
      Rectangle {
        width: root.notch
        height: root.notch
        x: (parent.width - width) / 2
        y: root.above ? -height / 2 : root.tip - height / 2
        rotation: 45
        radius: 0
        color: Qt.tint(Color.popups.background, Qt.alpha(Color.accent, 0.08))
        border.width: 1
        border.color: Color.accent
      }
    }
  }
  contentItem: AgentConnect {
    id: prompt
    width: root.availableWidth
    framed: false
    closable: true
    service: root.host ? root.host.service : null
    onCloseRequested: if (root.host) root.host.closeConnect()
    onCopied: closeAfterCopy.restart()
  }
  Timer { id: closeAfterCopy; interval: 1600; onTriggered: if (root.host && root.host.connectOpen) root.host.closeConnect() }
}
