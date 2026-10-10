import QtQuick
import QtQuick.Controls
import qs.Commons

// The console's one confirmation (Console.askConfirm), drawn as a small card attached to the
// control that asked: under it, or over it when there is no room below, with a notch pointing at
// it. It fades and grows in over 120 ms. Cancel has focus, Escape or a click anywhere else
// cancels, and a second click on the same control closes it. Console owns what runs.
Popup {
  id: root
  property var host: null
  readonly property var confirmation: host ? host.confirmation : null
  readonly property bool danger: !!confirmation && confirmation.danger === true
  readonly property color edge: danger ? Color.urgent : Color.accent
  readonly property real gap: Style.space(3)
  readonly property real notch: Style.space(10)
  // How far the notch's point stands off the card.
  readonly property real tip: Math.ceil(notch * Math.SQRT1_2)
  property Item anchorItem: null
  property bool above: false

  // Attaches to the confirmation's control and opens; a confirmation with no control left
  // on screen is canceled rather than shown somewhere else.
  function show() {
    var item = confirmation ? confirmation.anchor : null
    if (!item || !item.visible || !item.Window.window) { if (host) host.cancelConfirm(); return }
    anchorItem = item
    parent = item
    place()
    if (!opened) open()
    cancelButton.forceActiveFocus()
  }
  function place() {
    if (!anchorItem) return
    var at = anchorItem.mapToItem(null, 0, 0)
    var windowWidth = anchorItem.Window.width, windowHeight = anchorItem.Window.height
    var height = implicitHeight
    above = at.y + anchorItem.height + gap + height > windowHeight - margins && at.y - gap - height >= margins
    y = above ? -height - gap : anchorItem.height + gap
    // Left edges line up; near the right edge the right edges line up instead.
    x = at.x + width > windowWidth - margins ? Math.max(margins - at.x, anchorItem.width - width) : 0
  }

  width: Math.min(Style.space(400), anchorItem ? anchorItem.Window.width - margins * 2 : Style.space(400))
  margins: Style.space(8)
  padding: Style.space(12)
  topPadding: padding + (above ? 0 : tip)
  bottomPadding: padding + (above ? tip : 0)
  focus: true
  modal: false
  dim: false
  // Outside its control, so a second click on the control reaches Console.askConfirm, which closes it.
  closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutsideParent
  transformOrigin: above ? Item.Bottom : Item.Top
  enter: Transition {
    NumberAnimation { property: "opacity"; from: 0; to: 1; duration: 120; easing.type: Easing.OutCubic }
    NumberAnimation { property: "scale"; from: 0.96; to: 1; duration: 120; easing.type: Easing.OutCubic }
  }
  // Escape, a click elsewhere or a lost control closes it here first; Console then cancels.
  onOpened: cancelButton.forceActiveFocus()
  onClosed: if (host && host.confirmation) host.cancelConfirm()
  onParentChanged: if (!parent && host && host.confirmation) host.cancelConfirm()
  Connections {
    target: root.host
    function onConfirmationChanged() { if (root.host.confirmation) Qt.callLater(root.show); else root.close() }
  }

  background: Item {
    // The card sits below (or above) the notch's half that sticks out toward the control.
    Item {
      id: card
      width: parent.width
      y: root.above ? 0 : root.tip
      height: parent.height - root.tip
      Rectangle { anchors.fill: parent; radius: 0; color: Color.popups.background }
      Rectangle { anchors.fill: parent; radius: 0; color: Qt.alpha(root.edge, 0.08) }
      Rectangle { anchors.fill: parent; radius: 0; color: "transparent"; border.width: 1; border.color: root.edge }
    }
    // A square turned 45° and centered on the card's edge, clipped to the half outside it (plus
    // the edge's own pixel row, which its fill opens), pointing at the control's middle.
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
        color: Qt.tint(Color.popups.background, Qt.alpha(root.edge, 0.08))
        border.width: 1
        border.color: root.edge
      }
    }
  }
  contentItem: Column {
    spacing: Style.space(10)
    Accessible.role: Accessible.AlertMessage
    Accessible.name: root.confirmation ? root.confirmation.message : ""
    Copy {
      width: root.availableWidth
      text: root.confirmation ? root.confirmation.message : ""
      font.pixelSize: Style.font.bodySmall
    }
    Row {
      anchors.right: parent.right
      spacing: Style.space(8)
      ActionButton {
        id: cancelButton
        size: "small"
        label: "Cancel"
        KeyNavigation.right: confirmButton
        KeyNavigation.tab: confirmButton
        KeyNavigation.backtab: confirmButton
        onClicked: if (root.host) root.host.cancelConfirm()
      }
      ActionButton {
        id: confirmButton
        size: "small"
        role: root.danger ? "danger" : "primary"
        label: root.confirmation ? root.confirmation.confirmLabel : "Confirm"
        KeyNavigation.left: cancelButton
        KeyNavigation.tab: cancelButton
        KeyNavigation.backtab: cancelButton
        onClicked: if (root.host) root.host.runConfirm()
      }
    }
  }
}
