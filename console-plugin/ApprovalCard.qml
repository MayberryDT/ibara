import QtQuick
import Quickshell
import qs.Commons
import "StatusModel.js" as StatusModel

// An agent asking for approval on one computer, as a toast in the console's stack and at the
// top of the quick panel: the computer, what the agent wants to do in plain words, when it
// asked, and Approve, Deny and Details. Details opens labeled lines inside the toast (who asks,
// what, where, the page, the window's title and the task), scrolling when they are long; the
// request itself is never shown, and Copy Request copies it exactly as the computer sent it. A
// step that sends, spends or deletes also offers Always Allow: approve it, and let that agent do
// that kind of step on that computer without asking from now on. An agent's request to stop
// asking reads Allow and Not Now. It stays until it is answered here, on the desktop
// notification or elsewhere; the answer goes at once and the toast leaves when the computer
// confirms it.
Toast {
  id: root
  property var service: null
  // { computer_id, label, ref, summary, facts, request, at, unreachable, agent, effect, stopAsking }
  // from fleet-attention (StatusModel.attentionView): `summary` is plain words, `facts` the
  // Details as [{ label, value }], `request` the request as the computer sent it.
  property var item: null
  readonly property Tokens tokens: Tokens {}
  readonly property bool stopAsking: !!item && item.stopAsking === true
  readonly property bool canAlwaysAllow: !!item && !stopAsking && String(item.effect || "") !== ""
  // The quick panel: A, D, Shift+A and I beside the first, and a shorter Details box.
  property bool compact: false
  property bool keyHints: false
  property string chosen: ""
  // The approval whose details are open: another approval in this toast's place starts closed.
  property string detailsRef: ""
  readonly property bool detailsOpen: !!item && detailsRef !== "" && detailsRef === String(item.ref)
  readonly property string name: item ? String(item.label || (service ? service.computerLabelFor(item.computer_id) : "")) : ""
  readonly property bool answering: !!service && !!item && !!service.busy["answer:" + item.ref]
  // The computer stopped answering: the approval still waits there, but an answer can't reach it.
  readonly property bool unreachable: !!item && item.unreachable === true
  readonly property string unreachableReason: name + " isn't answering right now. You can answer once it's back."
  readonly property string asked: {
    var at = item ? Number(item.at || 0) : 0
    if (!at || !service) return ""
    return "asked " + StatusModel.ageLabel(new Date(at).toISOString(), service.nowMs)
  }
  readonly property string heading: name + (stopAsking ? " needs your answer" : " needs your approval")
  readonly property string summary: item ? String(item.summary || "An agent is waiting for your answer.") : ""
  readonly property var facts: item && Array.isArray(item.facts) ? item.facts : []
  readonly property string request: item ? String(item.request || "") : ""
  readonly property bool hasDetails: facts.length > 0 || request !== ""

  function answer(value) {
    if (!service || !item || answering || unreachable) return
    chosen = value
    service.answerApproval(item.ref, value, popup, name)
  }
  function toggleDetails() { if (item && hasDetails) detailsRef = detailsOpen ? "" : String(item.ref) }
  function closeDetails() { if (!detailsOpen) return false; detailsRef = ""; return true }
  function focusApprove() { approve.forceActiveFocus() }
  onAnsweringChanged: if (!answering) chosen = ""

  edge: popup ? tokens.attentionColor : Color.urgent
  tinted: true
  dismissable: popup
  holding: detailsOpen
  firstControl: approve
  Accessible.name: heading + ": " + summary

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
      // The quick panel is narrow: the heading has the line to itself.
      visible: text !== "" && !root.compact && !root.popup
      text: root.unreachable ? "not answering right now" : root.asked
      color: root.unreachable ? root.tokens.textTint(root.tokens.pausedColor) : root.tokens.dim
      font.pixelSize: Style.font.bodySmall
      anchors.baseline: headingText.baseline
      wrapMode: Text.NoWrap
    }
  }
  // What the agent asks for, whole (at most 400 characters): an approval is decided from it, so
  // it wraps and is never cut.
  Copy {
    width: parent.width
    text: root.popup ? StatusModel.requestPopupText(root.item) : root.summary
    maximumLineCount: root.popup ? 1 : 2147483647
    elide: Text.ElideRight
    font.pixelSize: root.compact ? Style.font.bodySmall : Style.font.body
  }
  Copy { visible: root.unreachable && !root.popup; width: parent.width; text: root.unreachableReason; color: root.tokens.textTint(root.tokens.pausedColor); font.pixelSize: Style.font.bodySmall }
  // Details: what the agent asks for, one labeled line each, in a box that scrolls rather than
  // push the answers out of the toast.
  Flickable {
    id: detailsBox
    visible: root.detailsOpen && !root.popup
    width: parent.width
    height: Math.min(factsColumn.implicitHeight, Style.space(root.compact ? 120 : 200))
    contentWidth: width
    contentHeight: factsColumn.implicitHeight
    clip: true
    interactive: contentHeight > height
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Accessible.role: Accessible.StaticText
    Accessible.name: "Details: " + root.facts.map(function(f) { return f.label + ": " + f.value }).join(". ")
    Column {
      id: factsColumn
      width: detailsBox.width
      spacing: Style.space(3)
      Repeater {
        model: root.facts
        delegate: Item {
          required property var modelData
          width: factsColumn.width
          height: Math.max(factLabel.implicitHeight, factValue.implicitHeight)
          Copy { id: factLabel; width: Style.space(96); text: modelData.label; dimmed: true; font.pixelSize: Style.font.bodySmall }
          Copy { id: factValue; x: Style.space(104); width: parent.width - x; textFormat: Text.StyledText; text: root.tokens.detailMarkup(modelData.label, modelData.value, root.item ? root.item.effect : ""); font.pixelSize: Style.font.bodySmall; wrapMode: Text.WrapAtWordBoundaryOrAnywhere }
        }
      }
      Copy {
        visible: root.facts.length === 0
        width: parent.width
        text: "ibara can't say this request in plain words. Copy Request copies it exactly as the computer sent it."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
    }
  }
  Flow {
    width: parent.width
    spacing: Style.space(8)
    ActionButton {
      id: approve
      readonly property string word: root.stopAsking ? "Allow" : "Approve"
      label: root.answering && root.chosen === "approve" ? (root.stopAsking ? "Allowing…" : "Approving…") : word + (root.keyHints ? " (A)" : "")
      role: "primary"
      size: "small"
      blocked: root.answering || root.unreachable
      disabledReason: root.answering ? "ibara is sending your answer." : root.unreachable ? root.unreachableReason : ""
      Accessible.name: word + " on " + root.name + ": " + root.summary
      onClicked: if (!blocked) root.answer("approve")
    }
    ActionButton {
      readonly property string word: root.stopAsking ? "Not Now" : "Deny"
      label: root.answering && root.chosen === "deny" ? (root.stopAsking ? "Answering…" : "Denying…") : word + (root.keyHints ? " (D)" : "")
      role: root.stopAsking ? "quiet" : "danger"
      size: "small"
      blocked: root.answering || root.unreachable
      disabledReason: root.answering ? "ibara is sending your answer." : root.unreachable ? root.unreachableReason : ""
      Accessible.name: word + " on " + root.name + ": " + root.summary
      onClicked: if (!blocked) root.answer("deny")
    }
    ActionButton {
      visible: root.canAlwaysAllow
      label: root.answering && root.chosen === "always" ? "Allowing…" : "Always Allow" + (root.keyHints ? " (Shift+A)" : "")
      size: "small"
      blocked: root.answering || root.unreachable
      disabledReason: root.answering ? "ibara is sending your answer." : root.unreachable ? root.unreachableReason : ""
      tooltipText: "Approve, and let " + String(root.item && root.item.agent || "this agent") + " do this kind of step on " + root.name + " without asking from now on"
      Accessible.name: "Always allow on " + root.name + ": " + root.summary
      onClicked: if (!blocked) root.answer("always")
    }
    ActionButton {
      visible: root.hasDetails && !root.popup
      label: (root.detailsOpen ? "Hide Details" : "Details") + (root.keyHints ? " (I)" : "")
      role: "quiet"
      size: "small"
      selected: root.detailsOpen
      Accessible.name: (root.detailsOpen ? "Hide the details of " : "Show the details of ") + "the approval on " + root.name
      onClicked: root.toggleDetails()
    }
    ActionButton {
      visible: root.detailsOpen && !root.popup && root.request !== ""
      label: "Copy Request"
      role: "quiet"
      size: "small"
      tooltipText: "Copy the request exactly as " + root.name + " sent it"
      Accessible.name: "Copy the request behind the approval on " + root.name
      onClicked: { Quickshell.clipboardText = root.request; if (root.service) root.service.actionNotice = "Copied the request from " + root.name + "." }
    }
  }
}
