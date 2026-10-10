import QtQuick
import Quickshell
import qs.Commons

// Connect an Agent: one prompt the person pastes to any agent program, which then adds ibara to
// its own settings. ibara has nothing to set up per program. The prompt comes from ibara
// (`connect-prompt`, the same text `ibara prompt` prints). Add Computer shows it as a card at the
// end of its page; the fleet's Connect an Agent button shows it in a card attached to the button
// (AgentConnectDialog.qml), where it is narrow and has Close beside Copy Prompt.
Rectangle {
  id: root
  property var service: null
  property bool framed: true
  property bool closable: false
  readonly property Tokens tokens: Tokens {}
  readonly property string prompt: service ? service.connectPrompt : ""
  // Narrow: the sentence under the title, and the buttons under the prompt.
  readonly property bool narrow: width < Style.space(720)
  readonly property real pad: framed ? Style.space(14) : 0
  // Just copied: the button says so, with where it goes next, for a moment.
  property bool justCopied: false
  signal copied()
  signal closeRequested()
  Timer { id: copiedClock; interval: 2500; onTriggered: root.justCopied = false }

  function focusDefault() { copyPrompt.forceActiveFocus() }

  width: parent ? parent.width : 0
  implicitHeight: column.implicitHeight + (framed ? Style.space(24) : 0)
  radius: 0
  color: framed ? tokens.surface : "transparent"
  border.width: framed ? 1 : 0
  border.color: tokens.rule
  Accessible.role: Accessible.Grouping
  Accessible.name: title.text

  Column {
    id: column
    x: root.pad
    y: root.framed ? Style.space(12) : 0
    width: parent.width - root.pad * 2
    spacing: Style.space(10)
    Item {
      width: parent.width
      height: Math.max(title.implicitHeight, sentence.y + sentence.implicitHeight)
      Copy { id: title; text: "Connect an Agent"; font.bold: true; font.pixelSize: Style.font.title }
      // Beside the title, on its baseline; narrow, under it. Positions only (no baseline anchor), so
      // a width crossing the narrow line either way moves it.
      Copy {
        id: sentence
        x: root.narrow ? 0 : title.implicitWidth + Style.space(12)
        y: root.narrow ? title.implicitHeight + Style.space(4) : title.baselineOffset - baselineOffset
        width: root.narrow ? parent.width : Math.min(implicitWidth, parent.width - x)
        text: "Copy this prompt and paste it to any AI agent on this computer. The agent connects itself."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
    }
    Item {
      width: parent.width
      height: root.narrow ? promptBox.height + Style.space(8) + buttons.height : Math.max(promptBox.height, buttons.height)
      // The prompt as it will be pasted; it can be selected with the mouse too.
      Rectangle {
        id: promptBox
        width: root.narrow ? parent.width : parent.width - buttons.width - Style.space(8)
        height: promptText.implicitHeight + Style.space(14)
        radius: 0
        color: Qt.alpha(Color.popups.text, 0.05)
        border.width: 1
        border.color: root.tokens.rule
        TextEdit {
          id: promptText
          x: Style.space(10)
          anchors.verticalCenter: parent.verticalCenter
          width: parent.width - Style.space(20)
          text: root.prompt || "Loading the prompt…"
          readOnly: true
          selectByMouse: true
          activeFocusOnTab: false
          wrapMode: TextEdit.Wrap
          textFormat: TextEdit.PlainText
          color: root.prompt ? Color.popups.text : Qt.alpha(Color.popups.text, 0.64)
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          // The prompt is text to copy: show "<!--" and "-->" as typed, not as arrows.
          font.features: { "calt": 0, "liga": 0 }
          Accessible.role: Accessible.StaticText
          Accessible.name: "Prompt: " + text
        }
      }
      Row {
        id: buttons
        anchors.right: parent.right
        y: root.narrow ? promptBox.height + Style.space(8) : 0
        spacing: Style.space(8)
        Copy {
          visible: root.justCopied
          anchors.verticalCenter: parent.verticalCenter
          text: "Now paste it into your agent."
          color: Color.accent
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.NoWrap
        }
        ActionButton {
          id: closeButton
          visible: root.closable
          label: "Close"
          tooltipText: "Close (Escape)"
          Accessible.name: "Close Connect an Agent"
          KeyNavigation.tab: copyPrompt
          KeyNavigation.backtab: copyPrompt
          KeyNavigation.right: copyPrompt
          onClicked: root.closeRequested()
        }
        ActionButton {
          id: copyPrompt
          label: root.justCopied ? "Copied ✓" : "Copy Prompt"
          role: root.closable ? "primary" : "secondary"
          blocked: root.prompt === ""
          disabledReason: blocked ? "ibara is still reading the prompt." : ""
          Accessible.name: "Copy the prompt that connects an agent"
          KeyNavigation.tab: root.closable ? closeButton : null
          KeyNavigation.backtab: root.closable ? closeButton : null
          KeyNavigation.left: root.closable ? closeButton : null
          onClicked: {
            if (blocked) return
            Quickshell.clipboardText = root.prompt
            root.justCopied = true
            copiedClock.restart()
            root.copied()
          }
        }
      }
    }
  }
}
