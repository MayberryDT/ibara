import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// One login request, with human intent kept separate from sharing rules.
Toast {
  id: root
  property var service: null
  property var item: null
  property bool compact: false
  property var host: null
  signal settingsWanted()
  readonly property Tokens tokens: Tokens {}
  readonly property var login: item && item.login ? item.login : null
  readonly property var sites: login ? login.sites : []
  readonly property var assistance: login && login.assistance ? login.assistance : null
  readonly property bool canAssist: !!assistance && assistance.version === 1
  readonly property var signup: assistance && assistance.signup ? assistance.signup : null
  readonly property bool proposed: !!signup && signup.state === "proposed"
  readonly property string ref: item ? String(item.ref) : ""
  readonly property string computerId: item ? String(item.computer_id || "") : ""
  readonly property string name: item ? String(item.label || (service ? service.computerLabelFor(computerId) : "")) : ""
  readonly property var settings: service ? service.loginSettings : null
  readonly property string browser: service ? service.loginBrowserName : "your browser"
  readonly property string elsewhere: service && item ? service.loginSourceElsewhere(item) : ""
  readonly property var outcome: service && service.loginOutcomes[ref] ? service.loginOutcomes[ref] : null
  readonly property bool answering: !!service && !!service.busy["answer:" + ref]
  readonly property bool unreachable: !!item && item.unreachable === true
  readonly property string busyReason: answering ? "ibara is sending your answer." : unreachable ? name + " is not answering right now." : ""
  property var unticked: ({})
  function settled(site) { return ["resolved","declined","denied","cancelled"].indexOf(site.state) >= 0 }
  readonly property var selectedSites: sites.filter(function(s) { return !root.unticked[s.site] && !root.settled(s) })
  readonly property bool canShare: !!settings && settings.enabled && !elsewhere && selectedSites.length > 0 &&
    selectedSites.every(function(s) { return s.method !== "independent_session" && (!s.state || ["asking","deferred","deliver","deliver_once","shared","rejected","unknown","verify_session"].indexOf(s.state)>=0) }) && !proposed && !(popup && sites.length > 1)
  property bool detailsOpen: false
  property bool optionsOpen: false
  property bool remember: false
  property string chosen: ""
  readonly property string explanation: !settings ? "Update ibara on this computer to use the current login choices."
    : proposed ? "This approval covers only the displayed account setup. It does not enable browser login sharing."
    : sites.some(function(s){return s.method === "independent_session"}) ? "Sign in separately on this computer. Cookie copying is disabled for this site."
    : elsewhere ? "Browser logins come from " + elsewhere + ". You can still answer this request or join the target."
    : !settings.enabled ? "Browser login sharing is off. Join the target to sign in, or choose another route."
    : outcome && outcome.signedOut && outcome.signedOut.length ? StatusModel.loginSignedOutWords(outcome.signedOut, browser)
    : "Sharing delivers the selected login. The browser session may remain after this task."
  readonly property var facts: {
    if (!login) return []
    var rows = [{label:"Agent",value:login.agent},{label:"Computer",value:name},{label:"Task",value:login.goal},
      {label:"Sites",value:StatusModel.listWords(sites.map(function(s){return s.site}))}]
    if (login.page) rows.push({label:"Page",value:login.page})
    if (item && item.at && service) rows.push({label:"Requested",value:StatusModel.ageLabel(new Date(item.at).toISOString(),service.nowMs)})
    StatusModel.loginThrough(sites).forEach(function(v){rows.push({label:"Sign-in provider",value:v})})
    rows.push({label:"Login method",value:explanation})
    if (canAssist) rows.push({label:"Progress",value:assistance.continuation === "ready_to_resume" ? "Ready to resume when the agent returns" : assistance.continuation === "waiting_for_you" ? "Waiting for your answer; the computer is free" : "Request is attached to the current task"})
    return rows
  }
  function assist(choice) {
    if (choice === "defer" && service) service.hideRequestPopup(ref)
    if (!canAssist || !service || answering || unreachable || !selectedSites.length) return
    var decisions = {}
    selectedSites.forEach(function(s){decisions[s.site]=choice})
    if (choice === "approve_signup") { decisions = {}; decisions[signup.proposal.site] = choice }
    chosen = choice
    service.answerLoginAssistance(ref, decisions, assistance.revision)
  }
  function answer(choice) {
    if (!service || answering || unreachable || !selectedSites.length) return
    chosen = choice
    service.answerLogin(ref, StatusModel.loginDecisions(selectedSites, {}, choice), false, popup, canAssist ? assistance.revision : undefined)
  }
  function toggleSite(site) {
    var next = Object.assign({},unticked)
    if (next[site]) delete next[site]; else next[site] = true
    unticked = next
  }
  function focusApprove() { if (firstControl && firstControl.visible) firstControl.forceActiveFocus() }
  function closeDetails() {
    if (!detailsOpen && !optionsOpen) return false
    detailsOpen = false; optionsOpen = false; return true
  }
  onRefChanged: { unticked=({}); detailsOpen=false; optionsOpen=false; remember=false }
  onAnsweringChanged: if (!answering) chosen=""
  edge: tokens.attentionColor
  tinted: false
  dismissable: popup
  holding: detailsOpen || optionsOpen
  firstControl: proposed ? createButton : canShare ? shareButton : joinButton.visible ? joinButton : optionsButton
  Accessible.name: "Sign-in needed for " + StatusModel.listWords(sites.map(function(s){return s.site})) + " on " + name

  Copy {
    width: parent.width
    text: "Sign-in needed · " + (sites.length === 1 ? sites[0].site : sites.length + " sites")
    font.bold: true
    wrapMode: Text.WrapAtWordBoundaryOrAnywhere
  }
  Copy {
    width: parent.width
    text: (login && login.agent ? login.agent : "Agent") + " · " + root.name
    font.pixelSize: Style.font.bodySmall
    wrapMode: Text.WrapAtWordBoundaryOrAnywhere
  }
  Flow {
    visible: root.sites.length > 1 && !root.popup
    width: parent.width
    spacing: Style.space(6)
    Repeater {
      model: root.sites
      delegate: ActionButton {
        required property var modelData
        readonly property bool ticked: !root.unticked[modelData.site]
        label: modelData.site + (modelData.via ? " via " + modelData.via : "")
        glyph: ticked ? "✓" : ""
        size: "small"
        role: "quiet"
        selected: ticked
        blocked: root.answering || root.settled(modelData)
        Accessible.role: Accessible.CheckBox
        Accessible.checkable: true
        Accessible.checked: ticked
        tooltipText: "Unselected sites remain unanswered."
        onClicked: root.toggleSite(modelData.site)
      }
    }
  }
  Copy {
    visible: root.proposed
    width: parent.width
    text: root.proposed ? "Create a free account at " + root.signup.proposal.site + " for " + root.signup.proposal.identity +
      ". Save access in " + root.signup.proposal.credential_store + ". " + root.signup.proposal.summary : ""
    wrapMode: Text.WrapAtWordBoundaryOrAnywhere
  }
  Copy {
    visible: root.unreachable || (!root.canAssist && root.detailsOpen)
    width: parent.width
    text: root.unreachable ? root.busyReason : "Update this computer for Not now and No account choices. Joining remains available."
    dimmed: true
  }
  Flow {
    width: parent.width
    spacing: Style.space(8)
    ActionButton {
      id: createButton
      visible: root.proposed && !(root.popup && root.sites.length > 1)
      label: root.answering && root.chosen === "approve_signup" ? "Answering…" : "Create free account"
      size: "small"
      role: "primary"
      blocked: root.answering || root.unreachable
      onClicked: if (!blocked) root.assist("approve_signup")
    }
    ActionButton {
      id: shareButton
      visible: root.canShare && !root.proposed
      label: root.answering ? "Sharing…" : "Share sign-in"
      size: "small"
      role: "primary"
      blocked: root.answering || root.unreachable
      tooltipText: root.canAssist && !root.remember ? "Deliver once for this task; do not change saved sharing rules." : "Allow sharing on this computer from now on."
      onClicked: if (!blocked) root.answer(root.canAssist && !root.remember ? "share_once" : "share")
    }
    TakeControlButton {
      id: joinButton
      service: root.service
      host: root.host
      computerId: root.computerId
      actionLabel: "Join to sign in"
      onJoinRequested: if (root.canAssist) root.assist("person_sign_in")
      visible: computerId !== "" && (!root.service || !root.service.holdsControlOn(computerId) || connecting)
      size: "small"
      role: root.canShare || root.proposed ? "secondary" : "primary"
    }
    ActionButton {
      visible: root.canAssist
      label: "Not now"
      size: "small"
      role: "secondary"
      blocked: false
      onClicked: root.assist("defer")
    }
    ActionButton {
      id: optionsButton
      visible: root.canAssist
      label: root.optionsOpen ? "Fewer options" : "Other options"
      size: "small"
      role: "quiet"
      selected: root.optionsOpen
      onClicked: root.optionsOpen = !root.optionsOpen
    }
    ActionButton {
      label: root.detailsOpen ? "Hide details" : "Details"
      size: "small"
      role: "quiet"
      selected: root.detailsOpen
      onClicked: root.detailsOpen = !root.detailsOpen
    }
  }
  Flow {
    visible: root.optionsOpen && root.canAssist
    width: parent.width
    spacing: Style.space(8)
    Repeater {
      model: [{label:"I don’t have an account",choice:"no_account"}, {label:"Continue without signing in",choice:"without_account"},
        {label:"I’ve signed in",choice:"signed_in"}, {label:"Don’t use my login",choice:"decline"}, {label:"Stop this sign-in",choice:"cancel"}]
      delegate: ActionButton {
        required property var modelData
        label: modelData.label
        size: "small"
        role: "secondary"
        blocked: root.answering || root.unreachable || !root.selectedSites.length
        onClicked: if (!blocked) root.assist(modelData.choice)
      }
    }
  }
  Column {
    visible: root.detailsOpen
    width: parent.width
    spacing: Style.space(4)
    Repeater {
      model: root.facts
      delegate: Copy {
        required property var modelData
        width: parent.width
        text: modelData.label + ": " + modelData.value
        textFormat: Text.PlainText
        wrapMode: Text.WrapAtWordBoundaryOrAnywhere
        font.pixelSize: Style.font.bodySmall
      }
    }
    ActionButton {
      visible: root.canShare && root.canAssist
      label: "Remember for this computer"
      glyph: root.remember ? "✓" : ""
      selected: root.remember
      role: "quiet"
      size: "small"
      Accessible.role: Accessible.CheckBox
      Accessible.checkable: true
      Accessible.checked: root.remember
      tooltipText: "When you choose Share sign-in, also allow future sharing here."
      onClicked: root.remember = !root.remember
    }
    ActionButton {
      visible: root.canShare
      label: "Never share on any computer"
      role: "quiet"
      size: "small"
      blocked: root.answering || root.unreachable || !root.selectedSites.length
      tooltipText: "Save a Denied sharing rule for the selected sites on every computer."
      onClicked: if (!blocked) root.answer("never")
    }
  }
}
