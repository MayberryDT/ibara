import QtQuick
import QtQuick.Controls
import qs.Commons

// A short list of actions under its button: the fleet's Fleet Actions and Sort, and each card's More.
// items: [{ id, label, danger?, blocked?, reason?, selected? }]. Choosing one closes the list, puts the
// keyboard back on the button and emits `triggered(id)`; a confirmation that follows attaches to
// the button. Up and Down move, Enter or Space chooses, Escape or a click elsewhere closes and
// changes nothing. A blocked item stays in the list with its reason in the tooltip. A selected item
// (the current choice, as in Sort) is marked. The button's tooltip hides while its list is open.
Item {
  id: root
  property string label: "⋯"
  property string glyph: ""
  property string accessibleName: label
  property string tooltipText: ""
  property string role: "quiet"
  property string size: "normal"
  property bool selectedLook: false
  property var items: []
  property bool blocked: false
  property string disabledReason: ""
  readonly property Item button: trigger
  // Visible from the moment it opens, so a card that shows its buttons only while the menu is
  // open keeps them through the menu's opening.
  readonly property bool opened: popup.visible
  signal triggered(string id)

  function open() { if (!blocked && items.length) popup.open() }
  function close() { popup.close() }
  function focusTrigger() { trigger.forceActiveFocus() }

  implicitWidth: trigger.implicitWidth
  implicitHeight: trigger.implicitHeight

  ActionButton {
    id: trigger
    anchors.fill: parent
    glyph: root.glyph
    label: root.label
    role: root.role
    size: root.size
    blocked: root.blocked
    disabledReason: root.disabledReason
    tooltipText: root.disabledReason || (popup.visible ? "" : root.tooltipText)
    selected: popup.opened || root.selectedLook
    Accessible.role: Accessible.ButtonMenu
    Accessible.name: root.accessibleName
    Keys.onDownPressed: root.open()
    onClicked: if (!blocked) { if (popup.opened) popup.close(); else root.open() }
  }

  Popup {
    id: popup
    y: trigger.height + Style.space(2)
    // Near the window's right edge the list lines up with the button's right edge instead.
    x: {
      var at = root.mapToItem(null, 0, 0)
      return root.Window.window && at.x + width > root.Window.width - margins ? root.width - width : 0
    }
    width: Math.max(trigger.width, list.implicitWidth + padding * 2)
    margins: Style.space(8)
    padding: Style.space(3)
    focus: true
    closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutsideParent
    background: Rectangle {
      radius: 0
      color: Color.popups.background
      border.width: 1
      border.color: Qt.alpha(Color.popups.text, 0.3)
    }
    // The keyboard starts on the current choice, else on the first item that can be chosen.
    onOpened: {
      for (var s = 0; s < options.count; s++) if (root.items[s].selected === true) { options.itemAt(s).forceActiveFocus(); return }
      for (var i = 0; i < options.count; i++) if (!root.items[i].blocked) { options.itemAt(i).forceActiveFocus(); return }
      if (options.count) options.itemAt(0).forceActiveFocus()
    }
    onClosed: if (root.visible) trigger.forceActiveFocus()
    contentItem: Column {
      id: list
      spacing: Style.space(2)
      Repeater {
        id: options
        model: root.items
        delegate: ActionButton {
          required property var modelData
          required property int index
          width: Math.max(implicitWidth, popup.width - popup.padding * 2)
          size: "small"
          role: modelData.danger ? "danger" : "quiet"
          label: String(modelData.label)
          blocked: modelData.blocked === true
          disabledReason: String(modelData.reason || "")
          selected: modelData.selected === true
          Accessible.checkable: modelData.selected !== undefined
          Accessible.checked: modelData.selected === true
          Accessible.role: Accessible.MenuItem
          Keys.onUpPressed: if (index > 0) options.itemAt(index - 1).forceActiveFocus()
          Keys.onDownPressed: if (index < options.count - 1) options.itemAt(index + 1).forceActiveFocus()
          onClicked: {
            if (blocked) return
            var id = String(modelData.id)
            popup.close()
            root.triggered(id)
          }
        }
      }
    }
  }
}
