import QtQuick
import qs.Commons
import qs.Ui

// A small fleet summary whose main job is opening the console. What waits for your answer comes
// first, right under its title: a request to use this computer, then approvals, login requests and
// agents' questions, oldest first (two per page, with Next requests for the rest). Then the
// counts and up to 5 computers, the ones that need you or are in use first; the console handles everything
// else. While ibara isn't running on this computer, it says so instead, with Start ibara, and the
// computers are greyed at their last known state.
Panel {
  id: root
  moduleName: "io.zet.ibara"
  manageIpc: false

  property var anchorItem: null
  property var service: null
  // The bar owns the popout identity, even though this panel lives in a Loader.
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root
  readonly property int maxRows: 5
  // Rows first, then Open console, which is where the cursor starts.
  property int cursorIndex: 0
  readonly property Tokens tokens: Tokens {}
  readonly property var computers: service && Array.isArray(service.computers) ? service.computers.filter(function(c) { return !!(c && c.computer_id) }) : []
  readonly property bool stopped: !!service && service.serviceStopped === true
  // Approvals, login requests and agents' questions, oldest first; the first two show here.
  readonly property var approvals: service && Array.isArray(service.approvals) ? service.approvals : []
  readonly property var logins: service && Array.isArray(service.logins) ? service.logins : []
  readonly property var questions: service && Array.isArray(service.questions) ? service.questions : []
  readonly property var asks: stopped ? [] : approvals.concat(logins, questions, service && service.needsYou ? service.needsYou.filter(function(n) { return n.unsupported }) : []).sort(function(a, b) { return a.at - b.at || (a.ref < b.ref ? -1 : a.ref > b.ref ? 1 : 0) })
  property int askOffset: 0
  onAsksChanged: if (askOffset >= asks.length) askOffset = 0
  readonly property var shownAsks: asks.slice(askOffset, askOffset + 2)
  readonly property string moreAsksName: {
    var a = approvals.length + logins.length, q = questions.length
    var words = (a ? (a === 1 ? "1 approval" : a + " approvals") : "") + (a && q ? " and " : "") + (q ? (q === 1 ? "1 question" : q + " questions") : "")
    return "Browse " + words
  }
  // A, D, Shift+A and I answer the first approval shown, once it has been on screen for a
  // moment, so a keypress can never land on one that just replaced the one being read.
  readonly property int headIndex: {
    for (var i = 0; i < shownAsks.length; i++) if (shownAsks[i].kind === "approval") return i
    return -1
  }
  readonly property string headRef: headIndex !== -1 ? String(shownAsks[headIndex].ref) : ""
  function headCard() { var slot = headIndex !== -1 ? askCards.itemAt(headIndex) : null; return slot ? slot.item : null }
  property real headShownAt: 0
  onHeadRefChanged: headShownAt = Date.now()
  Component.onCompleted: headShownAt = Date.now()
  function answerHead(answer) {
    if (!headRef || Date.now() - headShownAt < 1500) return
    service.answerApproval(headRef, answer)
  }
  // Attention first, then in use; ready computers fill any rows left so a small fleet is listed whole.
  readonly property var rows: tokens.sortComputers(computers.slice(), "attention").slice(0, maxRows)
  readonly property int moreCount: computers.length - rows.length
  readonly property string moreText: {
    if (moreCount <= 0) return ""
    var shown = {}
    for (var i = 0; i < rows.length; i++) shown[String(rows[i].computer_id)] = true
    var words = { attention: "needing attention", offline: "offline", locked: "locked", connecting: "connecting", human: "in use", working: "working", paused: "paused", ready: "ready" }
    var present = []
    for (var s = 0; s < tokens.stateOrder.length; s++) {
      var state = tokens.stateOrder[s]
      for (var n = 0; n < computers.length; n++)
        if (!shown[String(computers[n].computer_id)] && tokens.stateOf(computers[n]) === state) { present.push(words[state]); break }
    }
    var list = present.length > 1 ? present.slice(0, -1).join(", ") + " or " + present[present.length - 1] : present.join("")
    return "+" + moreCount + " more " + list
  }

  function summon(payload) {
    var shell = bar && bar.shell
    if (!shell || typeof shell.summon !== "function") return
    if (shell.summon("io.zet.ibara", JSON.stringify(payload)) !== false) close()
  }
  function openConsole() {
    if (service && typeof service.openFleet === "function") service.openFleet()
    summon({ route: "fleet" })
  }
  function openComputer(id) {
    if (service && typeof service.openComputer === "function" && service.openComputer(id) === false) { openConsole(); return }
    summon({ route: "computer", computerId: String(id) })
  }
  function activate(index) {
    if (index >= 0 && index < rows.length) openComputer(String(rows[index].computer_id))
    else openConsole()
  }
  function open() {
    cursorIndex = rows.length
    if (service && typeof service.setSurfaceOpen === "function") service.setSurfaceOpen("quick", true)
    controller.show()
    Qt.callLater(function() { if (opened) keySurface.forceActiveFocus() })
  }
  function close() {
    controller.hide()
    if (service && typeof service.setSurfaceOpen === "function") service.setSurfaceOpen("quick", false)
    if (anchorItem) anchorItem.forceActiveFocus()
  }
  onOpenedChanged: { if (opened) headShownAt = Date.now(); else if (service) service.setSurfaceOpen("quick", false) }
  onRowsChanged: if (cursorIndex > rows.length) cursorIndex = rows.length

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keySurface
    padding: Style.space(12)
    contentWidth: fittedContentWidth(Style.space(340))
    contentHeight: fittedContentHeight(content.implicitHeight, Style.space(720))

    Item {
      id: keySurface
      anchors.fill: parent
      focus: true
      Keys.priority: Keys.BeforeItem
      Keys.onPressed: function(event) {
        if (event.key === Qt.Key_Escape) { root.close(); event.accepted = true }
        else if (event.key === Qt.Key_F5) { if (root.service) root.service.refresh(); event.accepted = true }
        else if (event.key === Qt.Key_A && (event.modifiers & Qt.ShiftModifier) && root.headCard()) {
          if (root.headCard().canAlwaysAllow) root.answerHead("always")
          event.accepted = true
        }
        else if (event.key === Qt.Key_A && root.headRef) { root.answerHead("approve"); event.accepted = true }
        else if (event.key === Qt.Key_D && root.headRef) { root.answerHead("deny"); event.accepted = true }
        else if (event.key === Qt.Key_I && root.headCard()) { root.headCard().toggleDetails(); event.accepted = true }
        else if (event.key === Qt.Key_Down || event.key === Qt.Key_Tab) { root.cursorIndex = Math.min(root.rows.length, root.cursorIndex + 1); event.accepted = true }
        else if (event.key === Qt.Key_Up || event.key === Qt.Key_Backtab) { root.cursorIndex = Math.max(0, root.cursorIndex - 1); event.accepted = true }
        else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter || event.key === Qt.Key_Space) { root.activate(root.cursorIndex); event.accepted = true }
      }

      Column {
        id: content
        width: parent.width
        spacing: Style.space(8)
        Item {
          width: parent.width
          height: Style.space(26)
          IbaraMark {
            id: mark
            width: Style.space(18)
            height: width
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            color: Color.accent
          }
          Copy {
            anchors.left: mark.right
            anchors.leftMargin: Style.space(10)
            anchors.verticalCenter: parent.verticalCenter
            text: "ibara"
            font.pixelSize: Style.font.title
            font.bold: true
          }
          Copy {
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            text: root.computers.length === 1 ? "1 computer" : root.computers.length + " computers"
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
        }
        // ibara isn't running here: said once, with the way to start it.
        Toast {
          visible: root.stopped
          dismissable: false
          edge: Color.urgent
          tinted: true
          Accessible.name: "ibara isn't running on this computer. " + stoppedHow.text
          Copy { width: parent.width; text: "ibara isn't running on this computer."; font.bold: true; color: root.tokens.textTint(root.tokens.attentionColor); font.pixelSize: Style.font.bodySmall }
          Copy {
            id: stoppedHow
            visible: !startIbara.visible
            width: parent.width
            text: (root.service && root.service.startState === "failed" ? "ibara didn't start. " : "") + "To start it, run this in a terminal: " + (root.service ? root.service.ibaraStartCommand : "")
            font.pixelSize: Style.font.bodySmall
          }
          ActionButton {
            id: startIbara
            visible: !!root.service && root.service.canStartIbara && root.service.startState !== "failed"
            label: root.service && root.service.startState === "starting" ? "Starting…" : "Start ibara"
            role: "primary"
            size: "small"
            focusable: false
            blocked: !!root.service && root.service.startState === "starting"
            onClicked: if (!blocked) root.service.startIbara()
          }
        }
        // Waiting for your answer: another computer asking to use this one, then the first two
        // approvals and questions, oldest first; A, D and I answer the first approval.
        Repeater {
          model: !root.stopped && root.service && Array.isArray(root.service.pairRequests) ? root.service.pairRequests : []
          delegate: PairRequestCard { width: content.width; service: root.service; request: modelData }
        }
        Copy {
          visible: !!root.service && root.service.updateAvailable !== ""
          width: parent.width
          text: root.service ? "ibara " + root.service.updateAvailable + " available" : ""
          font.pixelSize: Style.font.bodySmall
        }
        Repeater {
          id: askCards
          // By index: an item handed over as a model row loses its lists (options, Details lines).
          model: root.shownAsks.length
          delegate: Loader {
            id: askSlot
            required property int index
            readonly property var entry: root.shownAsks[index] || null
            width: content.width
            sourceComponent: askSlot.entry && askSlot.entry.kind === "approval" ? approvalCard : askSlot.entry && askSlot.entry.kind === "login" ? loginCard : askSlot.entry && askSlot.entry.kind === "question" ? questionCard : unsupportedCard
            Component { id: approvalCard; ApprovalCard { service: root.service; item: askSlot.entry; compact: true; keyHints: askSlot.index === root.headIndex } }
            Component { id: loginCard; LoginCard { service: root.service; item: askSlot.entry; compact: true; onSettingsWanted: root.summon({ route: "logins" }) } }
            Component {
              id: unsupportedCard
              Toast {
                dismissable: false
                Copy { width: parent.width; text: "This needs a newer ibara."; font.pixelSize: Style.font.bodySmall }
                ActionButton {
                  label: "Update ibara Here"
                  blocked: !root.service || !root.service.thisComputerId
                  disabledReason: "Pair this computer first."
                  onClicked: if (!blocked) { root.service.power(root.service.thisComputerId, "update_ibara"); root.close() }
                }
              }
            }
            Component { id: questionCard; QuestionCard { service: root.service; item: askSlot.entry; compact: true } }
          }
        }
        Toast {
          visible: root.asks.length > root.shownAsks.length
          dismissable: false
          edge: Color.urgent
          Accessible.name: moreWaiting.text
          Item {
            width: parent.width
            height: Math.max(moreWaiting.implicitHeight, showAll.implicitHeight)
            Copy {
              id: moreWaiting
              anchors.left: parent.left
              anchors.right: showAll.left
              anchors.rightMargin: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              text: root.asks.length + " pending requests · " + (root.askOffset + 1) + "–" + Math.min(root.askOffset + 2, root.asks.length)
              font.pixelSize: Style.font.bodySmall
            }
            ActionButton {
              id: showAll
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              size: "small"
              label: "Next requests"
              focusable: false
              Accessible.name: root.moreAsksName
              onClicked: root.askOffset = root.askOffset + 2 >= root.asks.length ? 0 : root.askOffset + 2
            }
          }
        }
        // The fleet header's counts, in words, leaving out those at 0.
        Flow {
          width: parent.width
          spacing: Style.space(12)
          visible: root.computers.length > 0
          opacity: root.stopped ? 0.45 : 1
          Repeater {
            model: root.tokens.fleetCountRows(root.computers).filter(function(row) { return row.count > 0 })
            delegate: Row {
              spacing: Style.space(5)
              Accessible.role: Accessible.StaticText
              Accessible.name: modelData.label + " " + modelData.count
              StateMarker { tokens: root.tokens; fleetState: modelData.state; anchors.verticalCenter: parent.verticalCenter }
              Copy { text: modelData.label; dimmed: true; font.pixelSize: Style.font.bodySmall; wrapMode: Text.NoWrap; anchors.verticalCenter: parent.verticalCenter }
              Copy { text: String(modelData.count); font.bold: true; font.pixelSize: Style.font.bodySmall; anchors.verticalCenter: parent.verticalCenter }
            }
          }
        }
        Copy {
          visible: root.computers.length === 0
          width: parent.width
          text: root.service && root.service.ibaraMissing ? "ibara isn't set up on this computer yet. Open the console to finish."
            : root.stopped ? "" : root.service && root.service.lastError ? String(root.service.lastError) : "No computers yet. Open the console to add one."
          color: root.service && root.service.lastError ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.dim
          font.pixelSize: Style.font.bodySmall
        }
        Repeater {
          model: root.rows
          delegate: Rectangle {
            id: row
            readonly property bool cursor: root.cursorIndex === index
            readonly property string fleetState: root.tokens.stateOf(modelData)
            width: content.width
            // Two lines for what it is doing, so the end of the line (interrupted, waiting for
            // you) still shows; longer text ends in "…", never losing its start.
            height: Math.max(Style.space(30), rowActivity.implicitHeight + Style.space(10))
            radius: 0
            opacity: root.stopped ? 0.45 : 1
            color: cursor ? Qt.alpha(Color.accent, 0.14) : rowHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
            border.width: cursor ? 1 : 0
            border.color: Color.accent
            Accessible.role: Accessible.Button
            Accessible.name: root.tokens.label(modelData) + ", " + root.tokens.stateLabel(fleetState, modelData) + ". Open in the console"
            HoverHandler { id: rowHover; onHoveredChanged: if (hovered) root.cursorIndex = index }
            MouseArea { anchors.fill: parent; onClicked: root.activate(index) }
            StateMarker {
              id: rowMarker
              tokens: root.tokens
              x: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              fleetState: row.fleetState
            }
            Copy {
              id: rowName
              anchors.left: rowMarker.right
              anchors.leftMargin: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              width: Math.min(implicitWidth, Style.space(96))
              text: root.tokens.label(modelData)
              font.bold: true
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            Copy {
              id: rowActivity
              anchors.left: rowName.right
              anchors.leftMargin: Style.space(10)
              anchors.right: parent.right
              anchors.rightMargin: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              textFormat: Text.StyledText
              text: root.tokens.activityMarkup(modelData, root.service ? root.service.nowMs : 0)
              Accessible.name: root.tokens.activity(modelData, root.service ? root.service.nowMs : 0)
              font.pixelSize: Style.font.bodySmall
              maximumLineCount: 2
              elide: Text.ElideRight
            }
          }
        }
        Copy {
          visible: root.moreText !== ""
          width: parent.width
          leftPadding: Style.space(8)
          text: root.moreText
          dimmed: true
          font.pixelSize: Style.font.caption
        }
        ActionButton {
          width: parent.width
          label: "Open Console  ⏎"
          role: "primary"
          hasCursor: root.cursorIndex === root.rows.length
          focusable: false
          onClicked: root.openConsole()
        }
      }
    }
  }
}
