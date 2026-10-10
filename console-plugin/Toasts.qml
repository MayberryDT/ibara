import QtQuick
import Quickshell
import qs.Commons
import "StatusModel.js" as StatusModel

// Every message the console has, as toasts over the page's bottom-right corner: never part of the
// page's layout, so nothing ever sits between a page's header and its content. The newest is at
// the bottom and older ones stack upward. Console owns the rows (Console.toastModel): messages
// (results, errors, Undo) and standing toasts that last as long as their condition (approvals,
// agents' questions and requests to use this computer, which wait for an answer; computers that
// need a person or are offline or need attention, whatever makes the bar red; files on their way;
// What's New; a theme run; While you were away; the pointer to Connect an Agent).
// Only a few show while the stack is closed: whatever waits for an answer (approvals, agents'
// questions, requests to use this computer, ibara not running) always shows, and the newest of
// the rest fill what room is left (StatusModel.toastLayout); older notes collapse first, behind a
// "+N more" toast whose Show All opens the whole list, scrolling when it is taller than the page.
// The keyboard: F6 (Console) moves here and back, Tab goes through the toasts' buttons, and
// Escape closes the focused toast's details, dismisses it, or returns to the page.
FocusScope {
  id: root
  readonly property Tokens tokens: Tokens {}
  property var host: null
  // The height the stack may cover: the page under its header.
  property real room: 0
  readonly property var service: host ? host.service : null
  readonly property var model: host ? host.toastModel : null
  readonly property int count: model ? model.count : 0
  readonly property int maxShown: Math.max(2, Math.min(5, Math.floor(room * 0.6 / Style.space(90))))
  property bool expanded: false
  // Each row's kind, oldest first, read again whenever rows come or go.
  function rowKinds(n) {
    var kinds = []
    for (var i = 0; i < n && model; i++) kinds.push(String(model.get(i).kind))
    return kinds
  }
  readonly property var layout: StatusModel.toastLayout(rowKinds(count), maxShown)
  // How many notes wait behind "+N more" while the list is closed.
  readonly property int hiddenCount: expanded ? 0 : layout.hidden
  function shownAt(index) { return expanded || layout.shown[index] === true }
  // The row the keyboard is in, while it is in the stack.
  property string focusedKey: ""
  // Whether the person moved the keyboard here (F6, Tab), rather than clicking a toast's button or
  // the console putting it here (the bar opening an approval, or the next toast after one went).
  // Only then does focus stop a toast's clock, and only then does it move on to the next toast.
  property bool keyboardFocus: false
  // The next focus that arrives here was put here by the console; it leaves keyboardFocus as it is.
  property bool consoleAssigned: false
  onActiveFocusChanged: if (!activeFocus) { focusedKey = ""; keyboardFocus = false }
  function noteFocusArrival() {
    if (!activeFocus) return
    if (consoleAssigned) { consoleAssigned = false; return }
    var item = root.Window.activeFocusItem
    keyboardFocus = !(item && item.pointerFocused === true)
  }
  onCountChanged: if (layout.hidden === 0) expanded = false
  // Opening the list shows it from the top, where Show Fewer is; otherwise the newest stays in view.
  onExpandedChanged: scroller.keepTop = expanded

  height: Math.min(stack.implicitHeight, room)

  function slotAt(index) { return rows.itemAt(index) }
  function toastAt(index) { var slot = slotAt(index); return slot ? slot.item : null }
  // The keyboard goes to the first toast in reading order: "+N more" when it shows, else the oldest shown.
  function focusStack() {
    keyboardFocus = true
    if (moreToast.visible) return moreToast.focusFirst()
    for (var i = 0; i < count; i++) { var toast = shownAt(i) ? toastAt(i) : null; if (toast && toast.focusFirst()) return true }
    return false
  }
  // One toast by its key (the bar opening an approval): the list opens if it is behind "+N more".
  function focusKey(key, approve) {
    var at = host ? host.toastIndex(key) : -1
    if (at === -1) return false
    if (!shownAt(at)) expanded = true
    var toast = toastAt(at)
    if (!toast) return false
    keyboardFocus = false
    consoleAssigned = true
    if (approve && typeof toast.focusApprove === "function") { toast.focusApprove(); return true }
    if (toast.focusFirst()) return true
    consoleAssigned = false
    return false
  }
  // Escape from the page: the newest toast that may be dismissed goes.
  function dismissNewest() {
    for (var i = count - 1; i >= 0; i--) {
      var toast = shownAt(i) ? toastAt(i) : null
      if (toast && toast.dismissable) { toast.dismissed(); return true }
    }
    return false
  }
  function escapePressed() {
    var at = host ? host.toastIndex(focusedKey) : -1
    var slot = at !== -1 ? slotAt(at) : null
    var toast = slot && slot.activeFocus ? slot.item : null
    if (toast && typeof toast.closeDetails === "function" && toast.closeDetails()) return
    if (toast && toast.dismissable) { toast.dismissed(); return }
    if (expanded) { expanded = false; moreToast.focusFirst(); return }
    if (host) host.focusPage()
  }
  // The toast the keyboard was on went (answered, dismissed or resolved elsewhere): if the person
  // was using the keyboard here, it moves to the toast now in its place, else the one above;
  // otherwise (a click, or focus the console gave it) it goes back to the page.
  function refocus(index) {
    if (refocusKeyboard) {
      consoleAssigned = true
      for (var i = Math.min(index, count - 1); i >= 0; i--) { var toast = shownAt(i) ? toastAt(i) : null; if (toast && toast.focusFirst()) return }
      if (moreToast.visible && moreToast.focusFirst()) return
      consoleAssigned = false
    }
    if (host) host.focusPage()
  }
  // Refocusing waits for the row to go; the timer goes with the stack, so a console closing
  // with toasts up never calls into a stack that is already gone.
  property int refocusIndex: -1
  property bool refocusKeyboard: false
  Timer { id: refocusLater; interval: 0; onTriggered: root.refocus(root.refocusIndex) }

  Keys.onEscapePressed: function(event) { root.escapePressed(); event.accepted = true }

  Flickable {
    id: scroller
    anchors.fill: parent
    contentWidth: width
    contentHeight: stack.implicitHeight
    interactive: contentHeight > height
    clip: true
    boundsBehavior: Flickable.StopAtBounds
    property bool keepTop: false
    // The newest stays in view as the stack grows, unless the list was just opened; a toast the
    // keyboard is in stays in view as it grows (its details), its top first.
    function keepInView() {
      var bottom = Math.max(0, contentHeight - height)
      if (keepTop) { contentY = 0; return }
      var at = root.activeFocus && root.host ? root.host.toastIndex(root.focusedKey) : -1
      var slot = at !== -1 ? root.slotAt(at) : null
      if (!slot || !slot.visible) { contentY = bottom; return }
      var y = contentY
      if (slot.y + slot.height > y + height) y = slot.y + slot.height - height
      if (slot.y < y) y = slot.y
      contentY = Math.max(0, Math.min(bottom, y))
    }
    onContentHeightChanged: keepInView()
    onHeightChanged: keepInView()

    Column {
      id: stack
      width: root.width
      spacing: Style.space(8)

      // "+N more": the newest few show; Show All opens the whole list, Show Fewer closes it.
      Toast {
        id: moreToast
        visible: root.hiddenCount > 0 || root.expanded
        dismissable: false
        edge: Qt.alpha(Color.popups.text, 0.4)
        firstControl: moreButton
        Accessible.name: moreText.text
        Item {
          width: parent.width
          height: Math.max(moreText.implicitHeight, moreButton.implicitHeight)
          Copy {
            id: moreText
            anchors.left: parent.left
            anchors.right: moreButton.left
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            text: root.expanded ? "All " + root.count + " messages" : "+" + root.hiddenCount + " more"
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
          }
          ActionButton {
            id: moreButton
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            size: "small"
            label: root.expanded ? "Show Fewer" : "Show All"
            Accessible.name: root.expanded ? "Show only the newest messages" : "Show all " + root.count + " messages"
            onClicked: root.expanded = !root.expanded
          }
        }
      }

      Repeater {
        id: rows
        model: root.model
        onItemRemoved: function(index, item) { if (item && item.key === root.focusedKey) { root.refocusIndex = index; root.refocusKeyboard = root.keyboardFocus; refocusLater.restart() } }
        delegate: Loader {
          id: slot
          required property int index
          required property string key
          required property string kind
          required property string ref
          required property string text
          required property string tone
          required property int serial
          required property string action
          width: stack.width
          visible: root.shownAt(index)
          onActiveFocusChanged: if (activeFocus) { root.focusedKey = key; Qt.callLater(root.noteFocusArrival) }
          onLoaded: if (item && item.focusHolds !== undefined) item.focusHolds = Qt.binding(function() { return root.keyboardFocus })
          sourceComponent: kind === "approval" ? approvalToast
            : kind === "login" ? loginToast
            : kind === "login-setup" ? loginSetupToast
            : kind === "login-rejected" ? loginRejectedToast
            : kind === "question" ? questionToast
            : kind === "viewer-closed" ? viewerClosedToast
            : kind === "pair" ? pairToast
            : kind === "need" ? needToast
            : kind === "problem" ? problemToast
            : kind === "drop" ? dropToast
            : kind === "news" ? newsToast
            : kind === "theme" ? themeToast
            : kind === "away" ? awayToast
            : kind === "connect" ? connectToast
            : kind === "stopped" ? stoppedToast
            : messageToast
        }
      }
    }
  }
  // A press anywhere in the stack means the pointer, not the keyboard, is in use here. It passes on.
  MouseArea {
    anchors.fill: parent
    z: 10
    acceptedButtons: Qt.AllButtons
    onPressed: function(mouse) { root.keyboardFocus = false; mouse.accepted = false }
  }

  // ---- one component per kind. `parent` is the row's Loader, which carries the row.
  Component {
    id: approvalToast
    ApprovalCard {
      life: 15000
      dismissable: true
      onDismissed: if (root.host) root.host.dismissToast(parent.key)
      readonly property string ref: parent ? parent.ref : ""
      service: root.service
      item: {
        var list = root.service && Array.isArray(root.service.approvals) ? root.service.approvals : []
        for (var i = 0; i < list.length; i++) if (list[i].ref === ref) return list[i]
        return null
      }
    }
  }
  // An agent asking for your login to a site.
  Component {
    id: loginToast
    LoginCard {
      life: 15000
      dismissable: true
      onDismissed: if (root.host) root.host.dismissToast(parent.key)
      readonly property string refKey: parent ? parent.ref : ""
      service: root.service
      host: root.host
      item: root.service ? root.service.loginByRef(refKey) : null
      onSettingsWanted: if (root.host) root.host.showLogins()
    }
  }
  Component {
    id: loginSetupToast
    LoginSetupCard { host: root.host; service: root.service }
  }
  // A site that rejected a shared login: the agent can't sign in there, so a person can with Join.
  Component {
    id: loginRejectedToast
    Toast {
      id: rejected
      readonly property string key: parent ? parent.key : ""
      readonly property var entry: root.service && parent ? root.service.loginRejectedByKey(parent.ref) : null
      readonly property string name: entry && root.service ? root.service.computerLabelFor(entry.computer) : ""
      edge: Color.urgent
      tinted: true
      firstControl: takeButton
      dismissName: "Dismiss the note about " + (entry ? entry.site : "the site")
      Accessible.name: rejectedText.text
      onDismissed: if (root.host) root.host.dismissToast(key)
      Copy { width: parent.width; text: rejected.entry ? rejected.entry.site + " didn't accept your login on " + rejected.name : ""; font.bold: true; color: root.tokens.textTint(root.tokens.attentionColor) }
      Copy { id: rejectedText; width: parent.width; text: "It still shows its sign-in page, so the agent there can't go on. Join to sign in yourself."; font.pixelSize: Style.font.bodySmall }
      TakeControlButton {
        id: takeButton
        service: root.service
        host: root.host
        computerId: rejected.entry ? String(rejected.entry.computer) : ""
        size: "small"
        Accessible.name: "Take control of " + rejected.name + " to sign in to " + (rejected.entry ? rejected.entry.site : "")
      }
    }
  }
  Component {
    id: pairToast
    PairRequestCard {
      readonly property string ref: parent ? parent.ref : ""
      service: root.service
      request: {
        var list = root.service && Array.isArray(root.service.pairRequests) ? root.service.pairRequests : []
        for (var i = 0; i < list.length; i++) if (list[i].request_id === ref) return list[i]
        return null
      }
    }
  }
  Component {
    id: newsToast
    WhatsNew {
      readonly property string key: parent ? parent.key : ""
      service: root.service
      onDismissed: if (root.host) root.host.dismissToast(key)
    }
  }
  Component {
    id: themeToast
    ThemeResults {
      readonly property string key: parent ? parent.key : ""
      service: root.service
      onDismissed: if (root.host) root.host.dismissToast(key)
    }
  }
  Component {
    id: awayToast
    AwayTimeline {
      life: 15000
      readonly property string key: parent ? parent.key : ""
      host: root.host
      service: root.service
      onDismissed: if (root.host) root.host.dismissToast(key)
    }
  }
  // A result or an error, with one action beside it (Undo, Fix It, Add Another) when it has one.
  Component {
    id: messageToast
    Toast {
      id: message
      readonly property string key: parent ? parent.key : ""
      readonly property string words: parent ? parent.text : ""
      readonly property string actionLabel: parent ? parent.action : ""
      readonly property bool error: !!parent && parent.tone === "error"
      readonly property int serial: parent ? parent.serial : 0
      // The same message shown again starts its time over.
      onSerialChanged: restartClock()
      edge: error ? Color.urgent : Color.accent
      tinted: error
      life: error ? 0 : actionLabel !== "" ? 10000 : 5000
      firstControl: actionButton.visible ? actionButton : null
      Accessible.name: words
      onDismissed: if (root.host) root.host.dismissToast(key)
      Item {
        width: parent.width
        height: Math.max(messageText.implicitHeight, actionButton.visible ? actionButton.implicitHeight : 0)
        Copy {
          id: messageText
          anchors.left: parent.left
          anchors.right: actionButton.visible ? actionButton.left : parent.right
          anchors.rightMargin: actionButton.visible ? Style.space(8) : 0
          anchors.verticalCenter: parent.verticalCenter
          text: message.words
          color: message.error ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.foreground
          font.pixelSize: Style.font.bodySmall
          maximumLineCount: 4
          elide: Text.ElideRight
        }
        ActionButton {
          id: actionButton
          visible: message.actionLabel !== ""
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          label: message.actionLabel
          size: "small"
          onClicked: if (root.host) root.host.runToastAction(message.key)
        }
      }
    }
  }
  // An agent's question, with its answers and Dismiss.
  Component {
    id: questionToast
    QuestionCard {
      life: 15000
      dismissable: true
      onDismissed: if (root.host) root.host.dismissToast(parent.key)
      readonly property string refKey: parent ? parent.ref : ""
      service: root.service
      host: root.host
      item: {
        var list = root.service && Array.isArray(root.service.questions) ? root.service.questions : []
        for (var i = 0; i < list.length; i++) if (list[i].ref === refKey) return list[i]
        return null
      }
    }
  }
  Component {
    id: viewerClosedToast
    Toast {
      id: closedViewer
      readonly property string computerId: parent ? parent.ref : ""
      edge: Color.accent
      firstControl: handBack
      dismissable: false
      Copy { width: parent.width; text: root.service ? root.service.computerLabelFor(closedViewer.computerId) + ": you still have control" : "" }
      Flow {
        width: parent.width
        spacing: Style.space(8)
        ActionButton {
          id: handBack
          label: "Hand Back"
          role: "primary"
          size: "small"
          blocked: !root.service || root.service.mutating
          onClicked: if (!blocked) root.service.handBackFor(closedViewer.computerId)
        }
        ActionButton {
          label: "Keep Control"
          size: "small"
          onClicked: if (root.service) root.service.keepControl(closedViewer.computerId)
        }
      }
    }
  }
  // A computer whose self-repair couldn't fix something a person can, with Fix It, on every page.
  Component {
    id: needToast
    Toast {
      id: need
      readonly property string key: parent ? parent.key : ""
      readonly property string computerId: parent ? parent.ref : ""
      readonly property var entry: {
        var list = root.service && Array.isArray(root.service.needsYou) ? root.service.needsYou : []
        for (var i = 0; i < list.length; i++) if (list[i].computer_id === computerId) return list[i]
        return null
      }
      readonly property string name: entry ? (entry.label || (root.service ? root.service.computerLabelFor(computerId) : "")) : ""
      readonly property bool fixing: !!root.service && !!root.service.busy["repair:" + computerId]
      readonly property string words: entry ? entry.message + (entry.fix || entry.unsupported ? "" : " Restart it, or check it there.") : ""
      readonly property bool here: !!root.host && root.host.route === "computer" && root.host.computerId === computerId
      edge: Color.urgent
      tinted: true
      firstControl: updateHere.visible ? updateHere : fixButton.visible ? fixButton : openNeed.visible ? openNeed : null
      dismissName: "Put away the note that " + name + " needs you"
      Accessible.name: name + " needs you. " + words
      onDismissed: if (root.host) root.host.dismissToast(key)
      Copy { width: parent.width; text: need.entry && need.entry.unsupported ? "This needs a newer ibara" : need.name + " needs you"; font.bold: true; color: root.tokens.textTint(root.tokens.attentionColor); wrapMode: Text.NoWrap; elide: Text.ElideRight }
      Copy { width: parent.width; text: need.entry && need.entry.unsupported ? "Update this computer to answer " + need.name + "'s request." : need.words; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(8)
        ActionButton {
          id: fixButton
          visible: !!need.entry && need.entry.fix !== ""
          label: need.fixing ? "Fixing…" : "Fix It"
          role: "primary"
          size: "small"
          blocked: need.fixing
          tooltipText: need.entry ? StatusModel.fixDescription(need.entry.fix) : ""
          Accessible.name: "Fix It on " + need.name + ": " + (need.entry ? StatusModel.fixDescription(need.entry.fix) : "")
          onClicked: if (!blocked && root.service) root.service.repair(need.computerId, need.entry.fix)
        }
        ActionButton {
          id: updateHere
          visible: !!need.entry && need.entry.unsupported
          label: "Update ibara Here"
          size: "small"
          blocked: !root.service || !root.service.thisComputerId
          disabledReason: "Pair this computer first."
          onClicked: if (!blocked && root.host) root.host.confirmPower(root.service.thisComputerId, "update_ibara")
        }
        ActionButton {
          id: openNeed
          visible: !need.here
          label: "Open " + need.name
          size: "small"
          Accessible.name: "Open " + need.name
          onClicked: if (root.host) root.host.showComputer(need.computerId)
        }
      }
    }
  }
  // A computer offline or needing attention for a minute or more: what is wrong and what to do,
  // with Wake when ibara can wake it. Put away, it leaves the bar too until its words change.
  Component {
    id: problemToast
    Toast {
      id: problem
      readonly property string key: parent ? parent.key : ""
      readonly property string computerId: parent ? parent.ref : ""
      readonly property var entry: {
        var list = root.service && Array.isArray(root.service.problems) ? root.service.problems : []
        for (var i = 0; i < list.length; i++) if (list[i].computer_id === computerId) return list[i]
        return null
      }
      readonly property string name: entry ? (entry.label || (root.service ? root.service.computerLabelFor(computerId) : "")) : ""
      readonly property bool canWake: !!entry && entry.wake && !!root.service && root.service.canWake(computerId)
      readonly property bool waking: !!root.service && !!root.service.busy["wake:" + computerId]
      readonly property bool here: !!root.host && root.host.route === "computer" && root.host.computerId === computerId
      edge: Color.urgent
      tinted: true
      firstControl: wakeButton.visible ? wakeButton : openProblem.visible ? openProblem : null
      dismissName: "Put away the note about " + name
      Accessible.name: entry ? entry.heading + ". " + entry.body : ""
      onDismissed: if (root.host) root.host.dismissToast(key)
      Copy { width: parent.width; text: problem.entry ? problem.entry.heading : ""; font.bold: true; color: root.tokens.textTint(root.tokens.attentionColor); wrapMode: Text.NoWrap; elide: Text.ElideRight }
      Copy { width: parent.width; text: problem.entry ? problem.entry.body : ""; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(8)
        visible: wakeButton.visible || openProblem.visible
        ActionButton {
          id: wakeButton
          visible: problem.canWake
          label: problem.waking ? "Waking…" : "Wake"
          role: "primary"
          size: "small"
          blocked: problem.waking
          Accessible.name: "Wake " + problem.name
          onClicked: if (!blocked && root.service) root.service.wake(problem.computerId)
        }
        ActionButton {
          id: openProblem
          visible: !problem.here
          label: "Open " + problem.name
          size: "small"
          Accessible.name: "Open " + problem.name
          onClicked: if (root.host) root.host.showComputer(problem.computerId)
        }
      }
    }
  }
  // Files dropped on the open computer, on their way there, with how far along they are.
  Component {
    id: dropToast
    Toast {
      id: files
      readonly property string key: parent ? parent.key : ""
      readonly property string computerId: parent ? parent.ref : ""
      readonly property var drop: root.service && root.service.drops[computerId] ? root.service.drops[computerId] : null
      readonly property bool going: !!drop && drop.state !== "done"
      readonly property bool failed: !!drop && drop.failed.length > 0
      readonly property real done: !drop ? 0 : drop.state === "done" ? 1 : drop.files.length ? Math.min(1, (drop.index + 0.5) / drop.files.length) : 0
      readonly property string words: drop && root.service ? StatusModel.dropMessage(drop, root.service.computerLabelFor(computerId)) : ""
      edge: failed && !going ? Color.urgent : Color.accent
      tinted: failed && !going
      dismissable: !going
      Accessible.role: Accessible.StatusBar
      Accessible.name: words
      onDismissed: if (root.host) root.host.dismissToast(key)
      Copy { width: parent.width; text: files.words; color: root.tokens.textTint(files.failed && !files.going ? root.tokens.attentionColor : files.going ? root.tokens.workingColor : root.tokens.readyColor); font.pixelSize: Style.font.bodySmall; maximumLineCount: 3; elide: Text.ElideRight }
      Rectangle {
        parent: files
        anchors.bottom: parent.bottom
        x: 1
        width: (parent.width - 2) * files.done
        height: Math.max(2, Style.space(2))
        radius: 0
        color: files.failed ? Color.urgent : files.going ? Color.accent : files.tokens.readyColor
        Behavior on width { NumberAnimation { duration: 200 } }
      }
      readonly property Tokens tokens: Tokens {}
    }
  }
  // Until an agent has begun a task: where to connect one.
  Component {
    id: connectToast
    Toast {
      id: hint
      readonly property string key: parent ? parent.key : ""
      life: 12000
      firstControl: connectButton
      dismissName: "Dismiss the note about Connect an Agent"
      Accessible.name: hintText.text
      onDismissed: if (root.host) root.host.dismissToast(key)
      Copy { id: hintText; width: parent.width; text: "To let an AI agent use your computers, give it the prompt from Connect an Agent at the top."; font.pixelSize: Style.font.bodySmall }
      ActionButton {
        id: connectButton
        label: "Connect an Agent"
        size: "small"
        Accessible.name: "Connect an Agent: show the prompt to give an agent"
        onClicked: if (root.host) { root.host.dismissToast(hint.key); root.host.openConnect(null) }
      }
    }
  }
  // ibara isn't running on this computer: the one standing toast until it runs again, with
  // Start ibara when this desktop can start it, else (or once starting failed) the command.
  Component {
    id: stoppedToast
    Toast {
      id: stopped
      readonly property bool canStart: !!root.service && root.service.canStartIbara && root.service.startState !== "failed"
      readonly property bool starting: !!root.service && root.service.startState === "starting"
      readonly property bool failed: !!root.service && root.service.startState === "failed"
      readonly property string command: root.service ? root.service.ibaraStartCommand : ""
      readonly property string words: stopped.canStart ? "Your computers show what they were doing when it stopped, until it runs again."
        : (stopped.failed ? "ibara didn't start. " : "") + "To start it, run this in a terminal:"
      edge: Color.urgent
      tinted: true
      dismissable: false
      firstControl: startButton.visible ? startButton : copyButton
      Accessible.name: "ibara isn't running on this computer. " + words + (stopped.canStart ? "" : " " + command)
      Copy { width: parent.width; text: "ibara isn't running on this computer."; font.bold: true; color: root.tokens.textTint(root.tokens.attentionColor) }
      Copy { width: parent.width; text: stopped.words; font.pixelSize: Style.font.bodySmall }
      Row {
        visible: !stopped.canStart
        width: parent.width
        spacing: Style.space(8)
        Rectangle {
          width: parent.width - copyButton.width - Style.space(8)
          height: copyButton.height
          radius: 0
          color: Qt.alpha(Color.popups.text, 0.05)
          border.width: 1
          border.color: Qt.alpha(Color.popups.text, 0.14)
          TextEdit {
            x: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            width: parent.width - Style.space(16)
            text: stopped.command
            readOnly: true
            selectByMouse: true
            activeFocusOnTab: false
            textFormat: TextEdit.PlainText
            color: Color.popups.text
            font.family: "monospace"
            font.pixelSize: Style.font.bodySmall
            Accessible.role: Accessible.StaticText
            Accessible.name: "Command: " + text
          }
        }
        ActionButton {
          id: copyButton
          label: "Copy Command"
          size: "small"
          Accessible.name: "Copy the command that starts ibara"
          onClicked: { Quickshell.clipboardText = stopped.command; if (root.service) root.service.actionNotice = "Copied. Paste it into a terminal." }
        }
      }
      ActionButton {
        id: startButton
        visible: stopped.canStart
        label: stopped.starting ? "Starting…" : "Start ibara"
        role: "primary"
        size: "small"
        blocked: stopped.starting
        onClicked: if (!blocked && root.service) root.service.startIbara()
      }
    }
  }
}
