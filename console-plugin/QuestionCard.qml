import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// An agent's question on one computer, as a toast in the console's stack and at the top of the
// quick panel: the computer, the question, when it was asked, one button per answer the agent
// offered (in Title Case on the button; the answer sent is the agent's own text) or a field and
// Send when it offered none, and Dismiss, which tells the agent nobody will answer. It stays
// until it is answered or dismissed here or elsewhere, or its task's control ends; it leaves when
// the computer confirms.
Toast {
  id: root
  property var service: null
  property var host: null
  // { computer_id, label, ref, summary, options, at, unreachable } from fleet-attention
  // (StatusModel.attentionView).
  property var item: null
  readonly property Tokens tokens: Tokens {}
  // The quick panel is narrow: the heading has the line to itself.
  property bool compact: false
  property string chosen: ""
  property string selectedAnswer: ""
  readonly property string computerId: item ? String(item.computer_id || "") : ""
  readonly property string ref: item ? String(item.ref || "") : ""
  readonly property bool holdingControl: !!service && service.holdsControlOn(computerId)
  readonly property bool singleAffirmative: options.length === 1 && /^(done|yes|ok|okay|ready|solved|finished|continue|proceed|confirm|completed)(\b|[.!])/i.test(String(options[0]).trim())
  readonly property string doneAnswer: options.length === 0 ? "Done" : singleAffirmative ? String(options[0]) : selectedAnswer
  readonly property string name: item ? String(item.label || (service ? service.computerLabelFor(item.computer_id) : "")) : ""
  readonly property bool answering: !!service && !!item && !!service.busy["answer:" + item.ref]
  readonly property bool unreachable: !!item && item.unreachable === true
  readonly property string unreachableReason: name + " isn't answering right now. You can answer once it's back."
  readonly property var options: item && Array.isArray(item.options) ? item.options : []
  readonly property string asked: {
    var at = item ? Number(item.at || 0) : 0
    if (!at || !service) return ""
    return "asked " + StatusModel.ageLabel(new Date(at).toISOString(), service.nowMs)
  }
  readonly property string heading: "An agent on " + name + " asks you"
  readonly property string question: item ? String(item.summary || "An agent is waiting for your answer.") : ""

  function answer(value) {
    if (!service || !item || answering || unreachable) return
    chosen = String(value)
    if (!service.answerQuestion(item.ref, chosen)) chosen = ""
  }
  function dismissQuestion() {
    if (!service || !item || answering) return
    chosen = "\u0000dismiss"
    if (!service.dismissQuestion(item.ref)) chosen = ""
  }
  function doneAndHandBack() {
    if (!service || !item || answering || unreachable || !doneAnswer) return
    chosen = doneAnswer
    if (!service.answerAndHandBack(item.ref, chosen)) chosen = ""
  }
  property bool warmed: false
  function warm() { if (visible && !warmed && service && computerId) warmed = service.warmComputer(computerId) }
  Component.onCompleted: warm()
  onServiceChanged: { warmed = false; warm() }
  onVisibleChanged: { warmed = false; warm() }
  onComputerIdChanged: { warmed = false; selectedAnswer = ""; warm() }
  onRefChanged: selectedAnswer = ""
  Connections {
    target: root.service
    function onSessionsChanged() { root.warm() }
  }
  onAnsweringChanged: if (!answering) chosen = ""

  edge: popup ? tokens.workingColor : Color.urgent
  tinted: true
  dismissable: popup
  firstControl: holdingControl && (options.length === 0 || singleAffirmative) ? doneButton : options.length ? optionButtons.itemAt(0) : answerField
  Accessible.name: heading + ": " + question

  Row {
    width: parent.width
    spacing: Style.space(10)
    Copy {
      id: headingText
      width: Math.min(implicitWidth, parent.width - (askedText.visible ? askedText.implicitWidth + parent.spacing : 0))
      text: root.popup ? StatusModel.requestHeading(root.item, root.name, root.service && root.item ? root.service.sessions[root.item.computer_id] : null) : root.heading
      font.bold: true
      color: root.tokens.textTint(root.popup ? root.edge : root.tokens.attentionColor)
      wrapMode: Text.NoWrap
      elide: Text.ElideRight
    }
    Copy {
      id: askedText
      visible: text !== "" && !root.compact && !root.popup
      text: root.unreachable ? "not answering right now" : root.asked
      color: root.unreachable ? root.tokens.textTint(root.tokens.pausedColor) : root.tokens.dim
      font.pixelSize: Style.font.bodySmall
      anchors.baseline: headingText.baseline
      wrapMode: Text.NoWrap
    }
  }
  Copy { width: parent.width; text: root.popup ? StatusModel.requestPopupText(root.item) : root.question; maximumLineCount: root.popup ? 1 : 5; elide: Text.ElideRight }
  Copy { visible: root.unreachable && !root.popup; width: parent.width; text: root.unreachableReason; color: root.tokens.textTint(root.tokens.pausedColor); font.pixelSize: Style.font.bodySmall }
  // No answers offered: any short answer.
  FieldInput {
    id: answerField
    visible: root.options.length === 0 && !root.popup && !root.holdingControl
    width: parent.width
    maximumLength: 1000
    placeholder: "Your answer"
    accessibleName: "Your answer to the agent on " + root.name
    onAccepted: if (text.trim() !== "") root.answer(text)
  }
  Flow {
    width: parent.width
    spacing: Style.space(8)
    Repeater {
      id: optionButtons
      model: root.popup && root.options.length > 2 ? [] : root.options
      delegate: ActionButton {
        required property string modelData
        required property int index
        visible: !(root.holdingControl && root.singleAffirmative)
        label: root.answering && root.chosen === modelData ? "Sending…" : StatusModel.clip(StatusModel.titleCase(modelData), 60)
        role: !root.holdingControl && index === 0 ? "primary" : "secondary"
        selected: root.holdingControl && root.selectedAnswer === modelData
        size: "small"
        blocked: root.answering || root.unreachable
        disabledReason: root.answering ? "ibara is sending your answer." : root.unreachable ? root.unreachableReason : ""
        tooltipText: blocked ? disabledReason : modelData.length > 60 ? modelData : ""
        Accessible.name: "Answer " + modelData + " to the agent on " + root.name
        onClicked: if (!blocked) { if (root.holdingControl) root.selectedAnswer = modelData; else root.answer(modelData) }
      }
    }
    ActionButton {
    visible: root.options.length === 0 && !root.popup && !root.holdingControl
      label: root.answering && root.chosen !== "\u0000dismiss" ? "Sending…" : "Send"
      role: "primary"
      size: "small"
      blocked: root.answering || root.unreachable || answerField.text.trim() === ""
      disabledReason: root.answering ? "ibara is sending your answer." : root.unreachable ? root.unreachableReason : "Type an answer first."
      Accessible.name: "Send your answer to the agent on " + root.name
      onClicked: if (!blocked) root.answer(answerField.text)
    }
    ActionButton {
      visible: root.popup && (root.options.length === 0 || root.options.length > 2)
      label: "Open"
      role: "primary"
      size: "small"
      onClicked: root.bodyClicked()
    }
    ActionButton {
      id: doneButton
      visible: root.holdingControl
      label: root.answering ? "Sending…" : root.singleAffirmative ? StatusModel.titleCase(root.doneAnswer) + " & Hand Back" : "Done & Hand Back"
      role: "primary"
      size: "small"
      blocked: root.answering || root.unreachable || !root.doneAnswer || !!(root.service && root.service.mutating)
      disabledReason: !root.doneAnswer ? "Choose an answer first." : root.answering ? "Sending your answer." : root.unreachable ? root.unreachableReason : root.service && root.service.mutating ? "Wait for the current action to finish." : ""
      onClicked: if (!blocked) root.doneAndHandBack()
    }
    TakeControlButton {
      service: root.service
      host: root.host
      computerId: root.computerId
      visible: computerId !== "" && (!root.holdingControl || connecting)
      size: "small"
      role: "secondary"
    }
    ActionButton {
      visible: !root.popup
      label: root.answering && root.chosen === "\u0000dismiss" ? "Dismissing…" : "Dismiss"
      role: "quiet"
      size: "small"
      blocked: root.answering
      disabledReason: root.answering ? "ibara is sending your answer." : ""
      tooltipText: blocked ? disabledReason : "Tell the agent nobody will answer"
      Accessible.name: "Dismiss the question from the agent on " + root.name
      onClicked: if (!blocked) root.dismissQuestion()
    }
  }
}
