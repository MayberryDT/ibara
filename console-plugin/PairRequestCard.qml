import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// A request from another computer to use this one, as a toast in the console's stack and at the
// bottom of the quick panel: who asks, the code both screens show, and Accept or Decline. The
// person here checks that the other screen shows the same code before accepting; nothing else is
// asked. It stays until it is answered or the request ends. Accepted, it turns to the ready color
// for a moment (Service holds it 2.5 s): a check springs in beside the code and it says so.
Toast {
  id: root
  property var service: null
  property var request: null
  readonly property Tokens tokens: Tokens {}
  readonly property string name: StatusModel.computerName(request ? request.from_computer : "")
  readonly property string who: name + (request && request.from_owner ? " (" + request.from_owner + ")" : "")
  readonly property string code: request ? String(request.code) : ""
  readonly property bool accepted: !!request && request.accepted === true
  readonly property string answering: service && request && service.pairAnswers[request.request_id] ? String(service.pairAnswers[request.request_id]) : ""
  readonly property string busyReason: answering ? "ibara is sending your answer." : ""

  function answer(value) { if (service && request && !answering && !accepted) service.answerPairRequest(request.request_id, value) }

  edge: accepted ? tokens.readyColor : tokens.pausedColor
  tinted: true
  dismissable: false
  firstControl: accepted ? null : accept
  Accessible.name: accepted ? root.name + " can use this computer now." : who + " wants to use this computer. Code " + code

  onAcceptedChanged: if (accepted) matchAnim.restart()
  Component.onCompleted: if (accepted) matchAnim.restart()

  Copy { width: parent.width; text: root.accepted ? root.name + " can use this computer now" : root.who + " wants to use this computer"; color: root.tokens.textTint(root.accepted ? root.tokens.readyColor : root.tokens.pausedColor); font.bold: true }
  Row {
    spacing: Style.space(8)
    Copy { anchors.baseline: codeText.baseline; text: root.accepted ? "Matched" : "Accept only if " + root.name + " shows"; color: root.accepted ? root.tokens.textTint(root.tokens.readyColor) : root.tokens.dim; font.pixelSize: Style.font.bodySmall }
    Copy { id: codeText; text: root.code; color: root.accepted ? root.tokens.textTint(root.tokens.readyColor) : root.tokens.foreground; font.family: "monospace"; font.bold: true; font.pixelSize: Style.font.heading + Style.space(2); Accessible.ignored: true }
    Text {
      id: check
      visible: root.accepted
      anchors.baseline: codeText.baseline
      text: "✓"
      color: root.tokens.textTint(root.tokens.readyColor)
      font.family: Style.font.family
      font.bold: true
      font.pixelSize: Style.font.heading + Style.space(6)
      Accessible.ignored: true
    }
  }
  Row {
    visible: !root.accepted
    spacing: Style.space(8)
    ActionButton {
      id: accept
      label: root.answering === "accept" ? "Accepting…" : "Accept"
      role: "primary"
      size: "small"
      blocked: root.answering !== ""
      disabledReason: root.busyReason
      Accessible.name: "Accept " + root.name + ", code " + root.code
      onClicked: if (!blocked) root.answer("accept")
    }
    ActionButton {
      label: root.answering === "decline" ? "Declining…" : "Decline"
      size: "small"
      blocked: root.answering !== ""
      disabledReason: root.busyReason
      Accessible.name: "Decline " + root.name
      onClicked: if (!blocked) root.answer("decline")
    }
  }
  // The match: the code pops, the check springs in after it.
  SequentialAnimation {
    id: matchAnim
    ScriptAction { script: check.scale = 0 }
    NumberAnimation { target: codeText; property: "scale"; from: 1.3; to: 1; duration: 260; easing.type: Easing.OutCubic }
    NumberAnimation { target: check; property: "scale"; from: 0; to: 1; duration: 420; easing.type: Easing.OutBack; easing.overshoot: 2.2 }
  }
}
