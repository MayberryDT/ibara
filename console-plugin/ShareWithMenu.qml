import QtQuick
import QtQuick.Controls
import qs.Commons

// Share With… on a Logins tab row: a list under the button of your other computers, each with a
// tick, and All Computers, then Share. All Computers makes the site Allowed everywhere; chosen
// computers get the login now. A computer on an older ibara stays in the list, blocked, with why.
// Escape or a click elsewhere closes it and changes nothing.
Item {
  id: root
  // [{ id, label, blocked?, reason? }]
  property var computers: []
  property string accessibleName: "Share With…"
  property bool blocked: false
  property string disabledReason: ""
  readonly property bool opened: popup.visible
  readonly property Item button: trigger
  // `to`: computer ids, or "all".
  signal shared(var to)
  property var ticked: ({})
  property bool everywhere: false
  readonly property var chosen: computers.filter(function(c) { return !!root.ticked[c.id] }).map(function(c) { return c.id })

  function focusTrigger() { trigger.forceActiveFocus() }
  implicitWidth: trigger.implicitWidth
  implicitHeight: trigger.implicitHeight

  ActionButton {
    id: trigger
    anchors.fill: parent
    size: "small"
    label: "Share With…"
    blocked: root.blocked
    disabledReason: root.disabledReason
    tooltipText: root.disabledReason || (popup.visible ? "" : "Share this login with other computers, fresh from your browser")
    selected: popup.visible
    Accessible.role: Accessible.ButtonMenu
    Accessible.name: root.accessibleName
    Keys.onDownPressed: if (!blocked) popup.open()
    onClicked: if (!blocked) { if (popup.visible) popup.close(); else { root.ticked = ({}); root.everywhere = false; popup.open() } }
  }

  Popup {
    id: popup
    y: trigger.height + Style.space(2)
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
    onOpened: allItem.forceActiveFocus()
    onClosed: if (root.visible) trigger.forceActiveFocus()
    contentItem: Column {
      id: list
      spacing: Style.space(2)
      ActionButton {
        id: allItem
        width: Math.max(implicitWidth, popup.width - popup.padding * 2)
        size: "small"
        role: "quiet"
        glyph: root.everywhere ? "✓" : " "
        label: "All Computers"
        selected: root.everywhere
        tooltipText: "Allowed on every computer from now on, without asking"
        Accessible.role: Accessible.CheckBox
        Accessible.checkable: true
        Accessible.checked: root.everywhere
        Keys.onDownPressed: if (options.count) options.itemAt(0).forceActiveFocus(); else shareItem.forceActiveFocus()
        onClicked: root.everywhere = !root.everywhere
      }
      Repeater {
        id: options
        model: root.computers
        delegate: ActionButton {
          required property var modelData
          required property int index
          readonly property bool on: root.everywhere || !!root.ticked[modelData.id]
          width: Math.max(implicitWidth, popup.width - popup.padding * 2)
          size: "small"
          role: "quiet"
          glyph: on ? "✓" : " "
          label: String(modelData.label)
          selected: on
          blocked: root.everywhere || modelData.blocked === true
          disabledReason: modelData.blocked === true ? String(modelData.reason || "") : root.everywhere ? "All Computers includes it." : ""
          Accessible.role: Accessible.CheckBox
          Accessible.checkable: true
          Accessible.checked: on
          Keys.onUpPressed: if (index > 0) options.itemAt(index - 1).forceActiveFocus(); else allItem.forceActiveFocus()
          Keys.onDownPressed: if (index < options.count - 1) options.itemAt(index + 1).forceActiveFocus(); else shareItem.forceActiveFocus()
          onClicked: {
            if (blocked) return
            var next = Object.assign({}, root.ticked)
            if (next[modelData.id]) delete next[modelData.id]; else next[modelData.id] = true
            root.ticked = next
          }
        }
      }
      ActionButton {
        id: shareItem
        width: Math.max(implicitWidth, popup.width - popup.padding * 2)
        size: "small"
        role: "primary"
        label: root.everywhere ? "Share With All Computers" : root.chosen.length > 1 ? "Share With " + root.chosen.length : "Share"
        blocked: !root.everywhere && !root.chosen.length
        disabledReason: blocked ? "Tick a computer, or All Computers." : ""
        Keys.onUpPressed: if (options.count) options.itemAt(options.count - 1).forceActiveFocus(); else allItem.forceActiveFocus()
        onClicked: {
          if (blocked) return
          var to = root.everywhere ? "all" : root.chosen
          popup.close()
          root.shared(to)
        }
      }
    }
  }
}
