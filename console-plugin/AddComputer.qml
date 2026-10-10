import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Add Computer: the computers on your tailnet, each added with one choice. Your own computers
// add themselves; another person's computer shows the same code here and on its own screen, and
// someone there accepts, unless its owner gave you an invite code (I Have an Invite Code), which
// adds it at once. Tailscale comes first: until it is installed, on and signed in, the page
// says so in one sentence and offers the one fix. Up and Down move between the computers' buttons.
Item {
  id: root
  property var host: null
  property var service: null
  property bool showTailscaleInstall: false
  // The computers Add All started, so its button keeps its place (and the keyboard) while they add.
  property var addingAll: []
  readonly property bool allBusy: addingAll.some(function(node) { return !!pairings[node] && ["starting", "waiting"].indexOf(pairings[node].state) !== -1 })
  // Add All done but something needs a look (the page stays): the keyboard moves to it.
  onAllBusyChanged: if (!allBusy && visible) Qt.callLater(focusDefault)
  // The header is as tall as its tallest line plus a hairline margin, as on the fleet.
  readonly property real headerHeight: Math.ceil(Math.max(crumb.implicitHeight, backButton.implicitHeight)) + Style.space(6)
  readonly property Tokens tokens: Tokens {}
  // While ibara isn't running here, nothing it last heard from Tailscale is shown as current.
  readonly property bool stopped: !!service && service.serviceStopped === true
  readonly property var tailnet: service && !stopped ? service.tailnet : null
  readonly property string tailscaleState: tailnet ? tailnet.tailscale.state : ""
  readonly property bool tailscaleReady: tailscaleState === "running"
  readonly property string readError: service && service.readErrors["tailnet"] ? String(service.readErrors["tailnet"]) : ""
  readonly property var pairings: service ? service.pairings : ({})
  // Computers whose code someone just accepted, by node: when (ms). Their panel shows the match
  // for Tokens.matchMs; kept here, not in a row, because the list is rebuilt when they join.
  property var matched: ({})
  property var lastPairStates: ({})
  onPairingsChanged: {
    var states = ({}), next = null
    for (var node in pairings) {
      var p = pairings[node]
      states[node] = p.state
      if (p.state === "paired" && lastPairStates[node] === "waiting" && p.mode === "needs_approval" && p.code) {
        next = next || Object.assign({}, matched)
        next[node] = Date.now()
      }
    }
    lastPairStates = states
    if (next) { matched = next; matchClear.restart() }
  }
  Timer { id: matchClear; interval: root.tokens.matchMs; onTriggered: root.matched = ({}) }
  // This computer first, then computers ready to add (yours before other people's), those that
  // need ibara, and those that are off. A computer keeps its place while it is being added.
  readonly property var rows: {
    var list = tailnet ? tailnet.computers.slice() : []
    function group(c) { return c.is_self ? 0 : !c.online || c.ibara === "offline" ? 3 : c.ibara === "not_installed" ? 2 : 1 }
    list.sort(function(a, b) { return group(a) - group(b) || (a.same_owner === b.same_owner ? 0 : a.same_owner ? -1 : 1) || a.node.localeCompare(b.node) })
    return list
  }
  // Add All adds your own computers that are ready, except the one you're on: you use it already,
  // so it joins your fleet only when you add it by itself.
  readonly property var ownReady: rows.filter(function(c) { return !c.is_self && c.same_owner && root.rowState(c) === "addable" })
  readonly property var selfAddable: rows.filter(function(c) { return c.is_self && root.rowState(c) === "addable" })[0] || null

  function nameOf(c) { return c.label || StatusModel.computerName(c.node) }
  // What a row shows now: added, again (added, but it answers with a new identity, as after a
  // reinstall), adding, code (waiting for someone there), problem, offline, install (ibara isn't
  // there yet) or addable.
  function rowState(c) {
    var p = pairings[c.node]
    if ((c.paired && !c.changed) || (p && p.state === "paired")) return "added"
    if (p && p.state === "waiting" && p.mode === "needs_approval" && p.code) return "code"
    if (p && ["starting", "waiting", "canceling"].indexOf(p.state) !== -1) return "adding"
    if (p && ["declined", "expired", "failed"].indexOf(p.state) !== -1) return "problem"
    if (!c.online || c.ibara === "offline") return "offline"
    if (c.ibara === "not_installed") return "install"
    if (c.paired) return "again"
    return "addable"
  }
  function copy(text, notice) {
    Quickshell.clipboardText = text
    if (service) service.actionNotice = notice
  }
  function addAll() {
    var nodes = ownReady.map(function(c) { return c.node })
    addingAll = nodes
    for (var i = 0; i < nodes.length; i++) service.startPairing(nodes[i])
  }
  function reload() {
    if (!service || !visible) return
    service.loadTailnet()
    service.loadConnectPrompt()
  }
  function rowTarget(index) {
    var row = rowRepeater.itemAt(index)
    return row && row.visible && row.focusTarget && row.focusTarget.visible ? row.focusTarget : null
  }
  function focusDefault() {
    var target = tailscaleButton.visible ? tailscaleButton : addAllButton.visible ? addAllButton : null
    for (var i = 0; !target && i < rowRepeater.count; i++) target = rowTarget(i)
    ;(target || backButton).forceActiveFocus()
  }
  // Up and Down go to the next computer that has a button; Up from the first reaches Add All.
  function moveFocus(from, step) {
    for (var i = from + step; i >= 0 && i < rowRepeater.count; i += step) {
      var target = rowTarget(i)
      if (target) { target.forceActiveFocus(); return true }
    }
    if (step < 0 && addAllButton.visible) { addAllButton.forceActiveFocus(); return true }
    return false
  }
  // The computer whose button last had the keyboard, so a changed list puts the keyboard back
  // there; and whether the keyboard has reached the list since the page opened.
  property string focusNode: ""
  property bool rowsFocused: false
  function restoreFocus() {
    var focused = host ? host.focusedItem : null
    // The page opens on Back before the tailnet answers; once it does, the keyboard moves on, once.
    if (focused && focused.visible && !(focused === backButton && !rowsFocused)) return
    for (var i = 0; i < rowRepeater.count; i++) {
      var row = rowRepeater.itemAt(i), target = rowTarget(i)
      if (row && target && row.computer.node === focusNode) { target.forceActiveFocus(); rowsFocused = true; return }
    }
    focusDefault()
    rowsFocused = rowRepeater.count > 0
  }
  onVisibleChanged: { rowsFocused = false; reload() }
  onRowsChanged: if (visible) Qt.callLater(restoreFocus)
  onTailscaleStateChanged: if (visible && tailscaleState) Qt.callLater(focusDefault)
  // Computers come and go on the tailnet; while Tailscale is off, look again sooner.
  Timer {
    interval: root.tailscaleReady ? 10000 : 3000
    repeat: true
    running: root.visible && !!root.service
    onTriggered: root.service.loadTailnet()
  }

  component Heading: Copy { font.pixelSize: Style.font.heading; font.bold: true }
  // One command to run in a terminal, shown whole and selectable; its Copy button sits beside it.
  component CommandBox: Rectangle {
    property string command: ""
    height: commandText.implicitHeight + Style.space(14)
    radius: 0
    color: Qt.alpha(Color.popups.text, 0.05)
    border.width: 1
    border.color: root.tokens.rule
    TextEdit {
      id: commandText
      x: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: parent.width - Style.space(20)
      text: parent.command
      readOnly: true
      selectByMouse: true
      activeFocusOnTab: false
      wrapMode: TextEdit.WrapAnywhere
      textFormat: TextEdit.PlainText
      color: Color.popups.text
      font.family: "monospace"
      font.pixelSize: Style.font.body
      Accessible.role: Accessible.StaticText
      Accessible.name: "Command: " + text
    }
  }

  // ---- top bar: Back returns to the fleet, as Escape does; close stays apart past a rule.
  Item {
    id: header
    width: parent.width
    height: root.headerHeight
    IbaraMark {
      id: mark
      width: Style.space(22)
      height: width
      anchors.left: parent.left
      anchors.verticalCenter: parent.verticalCenter
      color: Color.accent
    }
    ActionButton {
      id: backButton
      anchors.left: mark.right
      anchors.leftMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      glyph: "‹"
      label: "Fleet"
      Accessible.name: "Back to the fleet"
      tooltipText: "Back to the fleet (Escape)"
      onClicked: if (root.host) root.host.showFleet()
    }
    Copy {
      id: crumb
      anchors.left: backButton.right
      anchors.leftMargin: Style.space(14)
      anchors.verticalCenter: parent.verticalCenter
      text: "Add Computer"
      font.pixelSize: Style.font.heading + Style.space(4)
      font.bold: true
    }
    Rectangle {
      anchors.right: closeButton.left
      anchors.rightMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: 1
      height: closeButton.height
      radius: 0
      color: root.tokens.rule
    }
    ActionButton {
      id: closeButton
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      label: "✕"
      role: "quiet"
      Accessible.name: "Close console"
      tooltipText: "Close the console"
      onClicked: if (root.host) root.host.requestClose()
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // Wide, the computers on your tailnet on the left, and Connect an Agent with what happens next
  // on the right; narrower, one column. Either way the page sits in the middle of the window.
  readonly property bool wide: width >= tokens.wideAt
  readonly property real sideWidth: Math.min(Style.space(560), Math.floor((width - tokens.columnGap) * 0.4))
  readonly property real listWidth: wide ? Math.min(Style.space(980), width - tokens.columnGap - sideWidth) : Math.min(width, tokens.columnMax)

  Flickable {
    id: scroll
    y: root.headerHeight + Style.space(14)
    width: parent.width
    height: parent.height - y
    clip: true
    contentWidth: width
    contentHeight: content.height + Style.space(16)
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Item {
      id: content
      width: root.wide ? root.listWidth + root.tokens.columnGap + root.sideWidth : root.listWidth
      x: Math.max(0, Math.round((scroll.width - width) / 2))
      height: root.wide ? Math.max(page.implicitHeight, side.implicitHeight) : side.y + side.implicitHeight
      Column {
        id: page
        width: root.listWidth
        spacing: Style.space(14)

        // ---- Tailscale first: not installed, off or signed out, one sentence and one button.
        Rectangle {
          visible: root.tailnet !== null && !root.tailscaleReady
          width: parent.width
          height: tailscaleColumn.implicitHeight + Style.space(36)
          radius: 0
          color: root.tokens.surface
          border.width: 1
          border.color: root.tokens.rule
          Column {
            id: tailscaleColumn
            x: Style.space(18)
            y: Style.space(18)
            width: parent.width - Style.space(36)
            spacing: Style.space(12)
            Copy {
              width: parent.width
              text: root.tailscaleState === "not_installed" ? "ibara finds and reaches your computers through Tailscale, which isn't installed on this computer yet."
                : root.tailscaleState === "stopped" ? "Tailscale is off on this computer, so ibara can't find your other computers."
                : "Sign in to Tailscale to find your computers here, with the same account on each of them."
              font.pixelSize: Style.font.title
            }
            ActionButton {
              id: tailscaleButton
              visible: root.tailnet !== null && !root.tailscaleReady
              role: "primary"
              label: root.tailscaleState === "not_installed" ? "Install Tailscale"
                : root.service && root.service.tailscaleSigningIn ? "Waiting for Tailscale…"
                : root.tailscaleState === "stopped" ? "Turn On Tailscale" : "Sign In to Tailscale"
              blocked: root.tailscaleState !== "not_installed" && !!root.service && root.service.tailscaleSigningIn
              disabledReason: blocked ? "Finish signing in to Tailscale in your browser." : ""
              onClicked: {
                if (blocked || !root.service) return
                if (root.tailscaleState === "not_installed") root.showTailscaleInstall = true
                else root.service.signInToTailscale()
              }
            }
            Copy {
              visible: root.tailscaleState !== "not_installed" && !!root.service && root.service.tailscaleSigningIn
              width: parent.width
              text: "Finish signing in to Tailscale in your browser. This page updates on its own."
              dimmed: true
              font.pixelSize: Style.font.bodySmall
            }
            Column {
              visible: root.tailscaleState === "not_installed" && root.showTailscaleInstall
              width: parent.width
              spacing: Style.space(8)
              Copy { width: parent.width; text: "Run this in a terminal, then come back here:"; font.pixelSize: Style.font.bodySmall }
              Row {
                width: parent.width
                spacing: Style.space(8)
                CommandBox { width: parent.width - copyTailscale.width - Style.space(8); command: root.service ? root.service.tailscaleInstallCommand : "" }
                ActionButton {
                  id: copyTailscale
                  label: "Copy Command"
                  Accessible.name: "Copy the Tailscale install command"
                  onClicked: root.copy(root.service.tailscaleInstallCommand, "Copied. Paste it into a terminal.")
                }
              }
            }
          }
        }

        // ---- the tailnet's computers
        Item {
          id: listHeader
          visible: root.tailscaleReady
          width: parent.width
          height: Math.max(listTitle.implicitHeight + signedIn.implicitHeight + (selfNote.visible ? selfNote.implicitHeight + Style.space(2) : 0) + Style.space(2), addAllButton.visible ? addAllButton.height : 0)
          Keys.onDownPressed: function(event) { event.accepted = root.moveFocus(-1, 1) }
          Heading { id: listTitle; text: "Computers on your tailnet" }
          Copy {
            id: signedIn
            anchors.top: listTitle.bottom
            anchors.topMargin: Style.space(2)
            text: root.tailnet && root.tailnet.tailscale.login ? "Signed in to Tailscale as " + root.tailnet.tailscale.login : ""
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Copy {
            id: selfNote
            visible: addAllButton.visible && !root.allBusy && !!root.selfAddable
            anchors.top: signedIn.bottom
            anchors.topMargin: Style.space(2)
            width: addAllButton.x - Style.space(16)
            text: root.selfAddable ? root.nameOf(root.selfAddable) + ", the computer you're on, isn't included: add it by itself if you want it on your fleet too." : ""
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          ActionButton {
            id: addAllButton
            visible: root.ownReady.length >= 2 || root.allBusy
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            role: "primary"
            glyph: root.allBusy ? "" : "+"
            label: root.allBusy ? "Adding " + root.addingAll.length + "…" : "Add Your " + (root.selfAddable ? "Other " : "") + root.ownReady.length + " Computers"
            blocked: root.allBusy
            disabledReason: blocked ? "ibara is adding them." : ""
            Accessible.name: label
            tooltipText: blocked ? disabledReason : "Adds " + root.ownReady.map(function(c) { return root.nameOf(c) }).join(", ")
            onClicked: if (!blocked) root.addAll()
          }
        }
        Copy {
          visible: root.tailnet === null
          width: parent.width
          text: root.stopped ? "ibara can't look for computers to add while it isn't running on this computer."
            : root.readError ? root.readError : "Looking for computers on your tailnet…"
          color: root.readError && !root.stopped ? Color.urgent : Qt.alpha(Color.popups.text, 0.64)
        }
        ActionButton {
          visible: root.tailnet === null && root.readError !== "" && !root.stopped
          label: "Try Again"
          onClicked: root.reload()
        }

        Column {
          visible: root.tailscaleReady
          width: parent.width
          spacing: 0
          Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
          Repeater {
            id: rowRepeater
            model: root.tailscaleReady ? root.rows : []
            delegate: Rectangle {
              id: row
              readonly property var computer: modelData
              readonly property string name: root.nameOf(computer)
              readonly property string kind: root.rowState(computer)
              readonly property var pairing: root.pairings[computer.node] || null
              readonly property string owner: computer.same_owner ? "Yours" : computer.owner || "Someone else's"
              // Another person's computer they shared with you: their invite code adds it at once.
              readonly property bool canUseCode: !computer.same_owner && !computer.is_self && (kind === "addable" || kind === "problem")
              property bool codeOpen: false
              function useCode() {
                if (!root.service || codeField.text.trim() === "") { codeField.focusInput(); return }
                root.service.startPairing(computer.node, codeField.text)
              }
              readonly property Item focusTarget: slot.visible ? slot : kind === "install" ? copyInstall : null
              readonly property bool focusInside: slot.activeFocus || copyInstall.activeFocus
              onFocusInsideChanged: if (focusInside) root.focusNode = computer.node
              // Added while the keyboard was on its button: the button goes, the keyboard moves on.
              onKindChanged: if (kind === "added" && root.focusNode === computer.node) Qt.callLater(function() {
                var focused = root.host ? root.host.focusedItem : null
                if (!focused || !focused.visible) root.moveFocus(index, 1) || root.moveFocus(index, -1) || root.focusDefault()
              })
              // Its code matched: the panel shows it for a moment. The pop plays once, at the match,
              // not again when the list is rebuilt around it.
              readonly property bool matched: !!root.matched[computer.node]
              onMatchedChanged: if (matched) matchAnim.restart()
              Component.onCompleted: if (matched && Date.now() - root.matched[computer.node] < 150) matchAnim.restart()
              readonly property int minutesLeft: pairing ? Math.max(1, Math.ceil((pairing.startedAt + 5 * 60000 - (root.service ? root.service.nowMs : Date.now())) / 60000)) : 5
              readonly property string stateText: kind === "added" ? "Added"
                : kind === "again" ? "It answers as a new computer, as after a reinstall"
                : kind === "adding" ? (pairing && pairing.state === "canceling" ? "Canceling…" : pairing && pairing.invite ? "Adding it with your invite code…" : "Adding it to your fleet…")
                : kind === "code" ? "Waiting for someone at " + name
                : kind === "problem" ? (pairing.state === "declined" ? "Declined on " + name : pairing.state === "expired" ? "Nobody accepted in time" : "Couldn't add it")
                : kind === "offline" ? "Offline · Turn it on to add it"
                : kind === "install" ? "ibara isn't installed there"
                : computer.ibara === "unknown" ? "ibara didn't answer; try adding it" : "Ready to add"
              width: parent.width
              height: line.height + detail.height + 1
              radius: 0
              color: focusInside ? Qt.alpha(Color.accent, 0.08) : "transparent"
              Keys.onUpPressed: function(event) { event.accepted = root.moveFocus(index, -1) }
              Keys.onDownPressed: function(event) { event.accepted = root.moveFocus(index, 1) }
              Accessible.role: Accessible.StaticText
              Accessible.name: name + (computer.is_self ? ", this computer" : "") + ", " + owner + ", " + stateText

              Item {
                id: line
                width: parent.width
                height: Style.space(46)
                opacity: row.kind === "offline" ? 0.55 : 1
                StateMarker {
                  id: marker
                  tokens: root.tokens
                  // Add Computer's own steps in the state colors: never an agent's or a person's mark.
                  plain: true
                  x: Style.space(10)
                  anchors.verticalCenter: parent.verticalCenter
                  fleetState: row.kind === "added" || row.kind === "addable" ? "ready" : row.kind === "problem" || row.kind === "again" ? "attention"
                    : row.kind === "code" ? "paused" : row.kind === "adding" ? "working" : "offline"
                }
                Copy {
                  id: nameText
                  anchors.left: marker.right
                  anchors.leftMargin: Style.space(10)
                  anchors.verticalCenter: parent.verticalCenter
                  width: Math.min(implicitWidth, parent.width * 0.26 - x)
                  text: row.name
                  font.bold: true
                  font.pixelSize: Style.font.title
                  wrapMode: Text.NoWrap
                  elide: Text.ElideRight
                }
                Copy {
                  anchors.left: nameText.right
                  anchors.leftMargin: Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                  visible: row.computer.is_self
                  text: "This computer"
                  color: Color.accent
                  font.pixelSize: Style.font.bodySmall
                }
                Copy {
                  x: parent.width * 0.34
                  anchors.verticalCenter: parent.verticalCenter
                  width: parent.width * 0.26
                  text: row.owner
                  dimmed: !row.computer.same_owner
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.NoWrap
                  elide: Text.ElideMiddle
                }
                Copy {
                  x: parent.width * 0.62
                  anchors.verticalCenter: parent.verticalCenter
                  width: (codeButton.visible ? codeButton.x : slot.x) - x - Style.space(8)
                  text: row.stateText
                  color: row.kind === "problem" ? Color.urgent : Qt.alpha(Color.popups.text, row.kind === "added" || row.kind === "addable" ? 1 : 0.72)
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.NoWrap
                  elide: Text.ElideRight
                }
                ActionButton {
                  id: codeButton
                  visible: row.canUseCode
                  anchors.right: slot.left
                  anchors.rightMargin: Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                  size: "small"
                  label: "I Have an Invite Code"
                  selected: row.codeOpen
                  Accessible.name: "I have an invite code for " + row.name
                  tooltipText: "The owner of " + row.name + " shared it with you and gave you a code"
                  onClicked: {
                    row.codeOpen = !row.codeOpen
                    if (row.codeOpen) Qt.callLater(codeField.focusInput)
                  }
                }
                ActionButton {
                  id: slot
                  visible: ["addable", "again", "adding", "code", "problem"].indexOf(row.kind) !== -1
                  anchors.right: parent.right
                  anchors.rightMargin: Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                  size: "small"
                  label: row.kind === "adding" ? (row.pairing && row.pairing.state === "canceling" ? "Canceling…" : "Adding…")
                    : row.kind === "code" ? "Cancel" : row.kind === "problem" ? "Try Again" : row.kind === "again" ? "Add Again" : "Add"
                  blocked: row.kind === "adding"
                  disabledReason: blocked ? "ibara is adding " + row.name + "." : ""
                  tooltipText: row.kind === "again" ? "Adds " + row.name + " again with the same checks as the first time, keeping its card" : ""
                  Accessible.name: (row.kind === "code" ? "Cancel adding " : row.kind === "problem" ? "Try adding again: " : row.kind === "again" ? "Add again: " : "Add ") + row.name + ", " + row.owner
                  onClicked: {
                    if (blocked || !root.service) return
                    if (row.kind === "code") root.service.cancelPairing(row.computer.node)
                    else root.service.startPairing(row.computer.node)
                  }
                }
              }

              // Under the row: the code to check on the other screen, or ibara's install command.
              // Why an add stopped (a refused invite code) is an error toast (Service.setPairing).
              Item {
                id: detail
                anchors.top: line.bottom
                width: parent.width
                height: detailColumn.visible ? detailColumn.implicitHeight + Style.space(14) : 0
                Column {
                  id: detailColumn
                  visible: ["code", "install"].indexOf(row.kind) !== -1 || row.matched || (row.codeOpen && row.canUseCode)
                  x: Style.space(29)
                  width: parent.width - x - Style.space(8)
                  spacing: Style.space(8)

                  Rectangle {
                    id: codePanel
                    readonly property color tone: row.matched ? root.tokens.readyColor : root.tokens.pausedColor
                    visible: row.kind === "code" || row.matched
                    width: parent.width
                    height: Math.max(bigCode.implicitHeight, codeWords.implicitHeight) + Style.space(24)
                    radius: 0
                    color: Qt.alpha(tone, row.matched ? 0.16 : 0.10)
                    border.width: row.matched ? Style.space(2) : 1
                    border.color: tone
                    Behavior on color { ColorAnimation { duration: 250 } }
                    Accessible.role: Accessible.StaticText
                    Accessible.name: row.matched ? "Matched. " + row.name + " accepted." : "Pairing code " + (row.pairing ? row.pairing.code : "")
                    Copy {
                      id: bigCode
                      x: Style.space(18)
                      anchors.verticalCenter: parent.verticalCenter
                      text: row.pairing ? row.pairing.code : ""
                      color: row.matched ? codePanel.tone : Color.popups.text
                      font.family: "monospace"
                      font.bold: true
                      font.pixelSize: Style.font.heading * 2 + Style.space(6)
                      font.letterSpacing: Style.space(2)
                      Accessible.ignored: true
                    }
                    Text {
                      id: matchCheck
                      visible: row.matched
                      anchors.left: bigCode.right
                      anchors.leftMargin: Style.space(12)
                      anchors.verticalCenter: parent.verticalCenter
                      text: "✓"
                      color: codePanel.tone
                      font.family: Style.font.family
                      font.bold: true
                      font.pixelSize: Style.font.heading * 2 + Style.space(6)
                      Accessible.ignored: true
                    }
                    Column {
                      id: codeWords
                      anchors.left: row.matched ? matchCheck.right : bigCode.right
                      anchors.leftMargin: Style.space(22)
                      anchors.right: parent.right
                      anchors.rightMargin: Style.space(18)
                      anchors.verticalCenter: parent.verticalCenter
                      spacing: Style.space(4)
                      Copy {
                        width: parent.width
                        text: row.matched ? "Matched. " + row.name + " accepted." : "Check that " + row.name + " shows " + (row.pairing ? row.pairing.code : "") + ", then accept there."
                        color: row.matched ? codePanel.tone : Color.popups.text
                        font.bold: true
                      }
                      Copy {
                        width: parent.width
                        text: row.matched ? "It's on your fleet now." : "Waiting for someone there. The code works for " + (row.minutesLeft === 1 ? "1 more minute." : row.minutesLeft + " more minutes.")
                        dimmed: true
                        font.pixelSize: Style.font.bodySmall
                      }
                      Copy {
                        visible: !row.matched
                        width: parent.width
                        text: "No screen on " + row.name + "? Run ibara join there."
                        dimmed: true
                        font.pixelSize: Style.font.bodySmall
                      }
                    }
                    // The match: the code pops, the check springs in after it.
                    SequentialAnimation {
                      id: matchAnim
                      ScriptAction { script: matchCheck.scale = 0 }
                      NumberAnimation { target: bigCode; property: "scale"; from: 1.25; to: 1; duration: 260; easing.type: Easing.OutCubic }
                      NumberAnimation { target: matchCheck; property: "scale"; from: 0; to: 1; duration: 420; easing.type: Easing.OutBack; easing.overshoot: 2.2 }
                    }
                  }

                  Copy {
                    visible: row.kind === "install"
                    width: parent.width
                    text: root.service && root.service.ibaraInstallCommand
                      ? "Run this on " + row.name + ", then come back here:"
                      : "Install ibara on " + row.name + " to add it."
                    font.pixelSize: Style.font.bodySmall
                  }
                  Row {
                    visible: row.kind === "install" && !!(root.service && root.service.ibaraInstallCommand)
                    width: parent.width
                    spacing: Style.space(8)
                    CommandBox { width: parent.width - copyInstall.width - Style.space(8); command: root.service ? root.service.ibaraInstallCommand : "" }
                    ActionButton {
                      id: copyInstall
                      label: "Copy Command"
                      Accessible.name: "Copy the command that installs ibara on " + row.name
                      onClicked: root.copy(root.service.ibaraInstallCommand, "Copied. Paste it into a terminal on " + row.name + ".")
                    }
                  }

                  Copy {
                    visible: row.codeOpen && row.canUseCode
                    width: parent.width
                    text: "Type the invite code the owner of " + row.name + " gave you. Their computer must be shared with you in Tailscale."
                    font.pixelSize: Style.font.bodySmall
                  }
                  Row {
                    visible: row.codeOpen && row.canUseCode
                    spacing: Style.space(8)
                    FieldInput {
                      id: codeField
                      width: Style.space(200)
                      placeholder: "4H7K-92QX"
                      accessibleName: "Invite code for " + row.name
                      monospace: true
                      maximumLength: 16
                      onAccepted: row.useCode()
                    }
                    ActionButton {
                      role: "primary"
                      label: "Add with Code"
                      Accessible.name: "Add " + row.name + " with the invite code"
                      onClicked: row.useCode()
                    }
                  }
                }
              }
              Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
            }
          }
        }
        Copy {
          visible: root.tailscaleReady
          width: Math.min(parent.width, root.tokens.proseWidth)
          text: "A computer that isn't listed isn't on your tailnet yet. Sign it in to Tailscale with the same account, and it shows up here."
          dimmed: true
          font.pixelSize: Style.font.bodySmall
        }
      }

      Column {
        id: side
        x: root.wide ? root.listWidth + root.tokens.columnGap : 0
        y: root.wide ? 0 : page.implicitHeight + Style.space(22)
        width: root.wide ? root.sideWidth : root.listWidth
        spacing: Style.space(14)
        // The prompt that connects an agent: always here, so a second agent can be connected any time.
        AgentConnect {
          visible: root.tailscaleReady && !!root.service && root.service.connectPrompt !== ""
          width: parent.width
          service: root.service
        }
        // What happens next, in a few plain steps.
        Rectangle {
          width: parent.width
          height: nextColumn.implicitHeight + Style.space(24)
          radius: 0
          color: root.tokens.surface
          border.width: 1
          border.color: root.tokens.rule
          Column {
            id: nextColumn
            x: Style.space(14)
            y: Style.space(12)
            width: Math.min(parent.width - Style.space(28), root.tokens.proseWidth)
            spacing: Style.space(10)
            Copy { text: "What happens next"; font.bold: true; font.pixelSize: Style.font.title }
            Repeater {
              model: [
                "Add a computer. Your own computers join at once. Another person's computer shows a code on its screen, and someone there accepts.",
                "It shows on your fleet with a live picture of its screen. Open it to watch, send files or take control.",
                "Give an agent the prompt from Connect an Agent. It can then use your computers, and by default it asks you before it sends, spends or deletes anything."
              ]
              delegate: Row {
                required property var modelData
                required property int index
                width: nextColumn.width
                spacing: Style.space(8)
                Copy { id: stepNumber; text: (index + 1) + "."; color: Color.accent; font.bold: true }
                Copy { width: parent.width - stepNumber.width - parent.spacing; text: modelData }
              }
            }
          }
        }
      }
    }
  }
}
