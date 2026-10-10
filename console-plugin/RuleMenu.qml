import QtQuick
import QtQuick.Controls
import qs.Commons

// An in-cell choice of Allowed, Ask First or Denied. The trigger is a small button as wide as the
// widest choice, so a column of them lines up; the list opens under it. Choosing a different rule
// emits `chosen` at once. Escape, or a click elsewhere, closes the list and changes nothing.
Item {
  id: root
  readonly property Tokens tokens: Tokens {}
  property string rule: "deny"
  property bool blocked: false
  property string disabledReason: ""
  property string accessibleName: ""
  signal chosen(string rule)

  readonly property var choices: [
    { value: "allow", label: "Allowed" }, { value: "ask", label: "Ask First" }, { value: "deny", label: "Denied" }
  ]
  readonly property bool opened: popup.opened
  function labelFor(value) { return value === "allow" ? "Allowed" : value === "ask" ? "Ask First" : "Denied" }
  function open() { if (!blocked) popup.open() }
  function close() { popup.close() }
  function focusTrigger() { trigger.forceActiveFocus() }

  implicitWidth: trigger.implicitWidth
  implicitHeight: trigger.implicitHeight
  // The widest label keeps every trigger in a column the same width.
  TextMetrics { id: widest; font.family: Style.font.family; font.pixelSize: Style.font.bodySmall; text: "Ask First ▾" }

  ActionButton {
    id: trigger
    anchors.fill: parent
    implicitWidth: Math.ceil(widest.advanceWidth + padding * 2)
    size: "small"
    label: root.labelFor(root.rule) + " ▾"
    labelColor: root.tokens.textTint(root.tokens.ruleColor(root.rule))
    blocked: root.blocked
    disabledReason: root.disabledReason
    selected: popup.opened
    Accessible.role: Accessible.ComboBox
    Accessible.name: (root.accessibleName ? root.accessibleName + ": " : "") + root.labelFor(root.rule)
    Keys.onDownPressed: root.open()
    onClicked: if (!blocked) { popup.opened ? popup.close() : popup.open() }
  }

  Popup {
    id: popup
    y: trigger.height + Style.space(2)
    width: Math.max(trigger.width, list.implicitWidth + padding * 2)
    margins: Style.space(8)
    padding: Style.space(3)
    focus: true
    closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutside
    background: Rectangle {
      radius: 0
      color: Color.popups.background
      border.width: 1
      border.color: Qt.alpha(Color.popups.text, 0.3)
    }
    onOpened: {
      for (var i = 0; i < root.choices.length; i++) if (root.choices[i].value === root.rule) options.itemAt(i).forceActiveFocus()
    }
    onClosed: if (root.visible) trigger.forceActiveFocus()
    contentItem: Column {
      id: list
      spacing: Style.space(2)
      Repeater {
        id: options
        model: root.choices
        delegate: ActionButton {
          required property var modelData
          required property int index
          width: Math.max(implicitWidth, popup.width - popup.padding * 2)
          size: "small"
          role: "quiet"
          glyph: modelData.value === root.rule ? "✓" : " "
          label: modelData.label
          labelColor: root.tokens.textTint(root.tokens.ruleColor(modelData.value))
          selected: modelData.value === root.rule
          Accessible.role: Accessible.MenuItem
          Keys.onUpPressed: if (index > 0) options.itemAt(index - 1).forceActiveFocus()
          Keys.onDownPressed: if (index < root.choices.length - 1) options.itemAt(index + 1).forceActiveFocus()
          onClicked: {
            var value = modelData.value, changed = value !== root.rule
            popup.close()
            if (changed) root.chosen(value)
          }
        }
      }
    }
  }
}
