import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// The open computer's workspaces, each with its windows, so a person can close a window an agent
// left behind, or move it to another workspace, without an agent. A window an agent is using
// stays as it is until its task stops. Read when the tab opens, every 5 s while it shows and
// after each action. No pictures of windows and no rearranging inside a workspace.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  readonly property Tokens tokens: Tokens {}
  readonly property string computerLabel: tokens.label(computer)
  readonly property var workspaces: service && service.computerWindows ? service.computerWindows.workspaces || [] : []
  readonly property var agent: service && service.computerWindows && service.computerWindows.agent ? service.computerWindows.agent : null
  readonly property bool loaded: !!service && service.windowsLoaded
  // A failed read keeps showing while the next one runs, so the 5 s reads don't blink it.
  readonly property string readError: service && service.readErrors.windows ? String(service.readErrors.windows) : ""
  property string lastError: ""
  onReadErrorChanged: if (readError) lastError = readError
  readonly property string shownError: readError || (loaded ? "" : lastError)
  // A computer on an ibara from before window management answers the read this way.
  readonly property bool tooOld: /Unknown operator operation/.test(shownError)
  readonly property string actionReason: !service ? "Connecting." : service.denied ? "You don't have access." : service.mutating ? "Wait for the current action to finish." : ""
  // What the last Close or Move To… left beside a window, by address: { text, busy }.
  property var notes: ({})
  // The keyboard's place, by window and button, so a list read again puts it back.
  property string focusAddress: ""
  property string focusPart: ""
  property var rowItems: ({})
  // A confirmation from this tab or a Move To… list is open: reading the list again would rebuild
  // the rows and close it, so the reads wait (Service.windowsHeld) and run once it closes.
  property string menuAddress: ""
  function inside(item) {
    for (var at = item; at; at = at.parent) if (at === root) return true
    return false
  }
  readonly property bool confirming: !!host && !!host.confirmation && inside(host.confirmation.anchor)
  readonly property bool holding: visible && (confirming || menuAddress !== "")
  onHoldingChanged: {
    if (service) service.windowsHeld = holding
    if (!holding) reload()
  }
  Component.onDestruction: if (service && holding) service.windowsHeld = false

  function reload() {
    if (!service || !visible || !computerId) return
    if (!service.readPending("windows")) service.loadWindows()
  }
  onVisibleChanged: reload()
  onComputerIdChanged: { notes = ({}); lastError = ""; focusAddress = ""; focusPart = ""; menuAddress = ""; reload() }
  onWorkspacesChanged: Qt.callLater(root.keepFocus)
  Timer {
    interval: 5000
    repeat: true
    running: root.visible && !!root.service && root.service.consoleOpen && root.computerId !== "" && !root.tooOld && !root.holding
    onTriggered: root.reload()
  }

  function workspaceTitle(space) {
    var name = String(space.name || space.id)
    if (space.special !== true) return "Workspace " + name
    var rest = name.replace(/^special:?/, "")
    return rest === "scratchpad" ? "Scratchpad" : rest ? "Special: " + rest : "Special"
  }
  function windowTitle(win) { return String(win.title || win["class"] || "Untitled window") }
  // Core says whether a window runs in a terminal, from its process: a TUI started with its own app id counts.
  function isTerminal(win) { return win.terminal === true }
  function windowPresent(address) {
    for (var i = 0; i < workspaces.length; i++)
      for (var j = 0; j < workspaces[i].windows.length; j++) if (workspaces[i].windows[j].address === address) return true
    return false
  }
  function moveItems(space) {
    var items = []
    for (var n = 1; n <= 10; n++) if (space.special === true || Number(space.id) !== n) items.push({ id: String(n), label: "Workspace " + n })
    return items
  }
  function setNote(address, note) {
    var next = Object.assign({}, notes)
    if (note) next[address] = note
    else delete next[address]
    notes = next
  }
  function closeWindow(win) {
    var address = String(win.address), pid = win.pid, scope = computerId
    var run = function() { root.setNote(address, null); root.service.closeWindow(address, pid) }
    if (!isTerminal(win) || !host) { run(); return }
    host.askConfirm({ message: "Anything running in it stops.", confirmLabel: "Close", danger: true, run: run,
      valid: function() { return String(root.service.scopedComputerId || "") === scope && root.windowPresent(address) } })
  }
  function moveWindow(win, workspace) {
    setNote(String(win.address), null)
    service.moveWindow(String(win.address), win.pid, workspace)
  }
  // Ending the task that uses a window asks first, as End Task does on the Activity tab.
  function stopTask(address, taskRef) {
    var scope = computerId
    if (!host || !taskRef) return
    host.askConfirm({ message: "End this task on " + computerLabel + "? Other agents keep working.", confirmLabel: "Stop Task", danger: true,
      run: function() { root.setNote(address, null); root.service.revokeTask(taskRef) },
      valid: function() { return String(root.service.scopedComputerId || "") === scope } })
  }
  // A row read again is built anew; if the keyboard was on it, it goes back to the same window
  // and button, or to Refresh when that window has gone. Elsewhere on the page, it stays put.
  function keepFocus() {
    if (!focusAddress || !visible) return
    var focused = host ? host.focusedItem : null
    for (var item = root; focused && item; item = item.parent) if (item === focused) { focused = null; break }
    if (focused) return
    var row = rowItems[focusAddress]
    if (row) row.takeFocus(focusPart)
    else { focusAddress = ""; refreshButton.forceActiveFocus() }
  }
  function noteFocus(address, part) { focusAddress = address; focusPart = part }

  Connections {
    target: root.service
    ignoreUnknownSignals: true
    function onWindowActionSettled(computerId, address, outcome) {
      if (computerId !== root.computerId) return
      if (outcome === "open") root.setNote(address, { text: "Still open. It may be asking to save; look at the Screen tab.", busy: false })
      else if (outcome === "busy") root.setNote(address, { text: "An agent is using this window. Stop its task first.", busy: true })
      else if (outcome === "pending-close" || outcome === "pending-move")
        root.setNote(address, { text: "Waiting for approval. After approval, choose " + (outcome === "pending-close" ? "Close" : "Move To…") + " again.", busy: false })
      else root.setNote(address, null)
    }
  }

  Flickable {
    id: scroll
    anchors.fill: parent
    clip: true
    contentWidth: width
    contentHeight: column.implicitHeight
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Column {
      id: column
      width: Math.min(scroll.width, root.tokens.columnMax)
      spacing: Style.space(6)
      Row {
        spacing: Style.space(10)
        ActionButton {
          id: refreshButton
          label: "Refresh"
          onClicked: root.reload()
          onActiveFocusChanged: if (activeFocus) root.noteFocus("", "")
        }
        DataAge {
          anchors.verticalCenter: parent.verticalCenter
          at: root.service && root.service.scopedReadAt.windows || 0
          refreshing: !!root.service && root.service.readPending("windows")
          nowMs: root.service ? root.service.nowMs : Date.now()
        }
      }
      Copy {
        visible: root.workspaces.length === 0
        width: parent.width
        text: root.tooOld ? "Update ibara on this computer to manage its windows."
          : root.shownError ? StatusModel.clip(root.shownError, 200)
          : root.service && (root.service.readPending("windows") || !root.loaded) ? "Loading…" : "No windows."
        color: root.tokens.textTint(root.tooOld ? root.tokens.pausedColor : root.shownError ? root.tokens.attentionColor : root.tokens.dim)
        font.pixelSize: Style.font.bodySmall
      }

      Repeater {
        model: root.workspaces
        delegate: Column {
          id: section
          readonly property var space: modelData
          width: column.width
          spacing: Style.space(2)
          Item { width: 1; height: Style.space(8) }
          Row {
            spacing: Style.space(10)
            Copy { text: root.workspaceTitle(section.space); font.pixelSize: Style.font.title; font.bold: true }
            Copy {
              visible: section.space.active === true
              anchors.verticalCenter: parent.verticalCenter
              text: "On Screen"
              color: root.tokens.textTint(root.tokens.accent)
              font.pixelSize: Style.font.bodySmall
            }
          }
          Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
          Copy {
            visible: section.space.windows.length === 0
            text: "No windows"
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }

          Repeater {
            model: section.space.windows
            delegate: Rectangle {
              id: row
              readonly property var win: modelData
              readonly property string address: String(win.address || "")
              readonly property string app: String(win["class"] || "App")
              readonly property string title: String(win.title || "")
              readonly property var task: win.task && typeof win.task === "object" ? win.task : null
              readonly property bool inUse: !!task && task.running === true
              readonly property var note: root.notes[address] || null
              // Stop Task beside "An agent is using this window": the task on it, else the one in control.
              readonly property string stopRef: note && note.busy ? String(task && task.task_ref ? task.task_ref : root.agent && root.agent.task_ref ? root.agent.task_ref : "") : ""
              readonly property string reason: inUse ? "An agent is using this window. Stop its task first." : root.actionReason
              readonly property bool focusInside: closeButton.activeFocus || moveMenu.button.activeFocus || stopButton.activeFocus
              function takeFocus(part) {
                if (part === "move") moveMenu.focusTrigger()
                else if (part === "stop" && stopButton.visible) stopButton.forceActiveFocus()
                else closeButton.forceActiveFocus()
              }
              Component.onCompleted: root.rowItems[address] = row
              Component.onDestruction: if (root.rowItems[address] === row) delete root.rowItems[address]
              width: section.width
              height: Math.max(Style.space(48), info.implicitHeight + Style.space(14))
              radius: 0
              color: focusInside ? Qt.alpha(Color.accent, 0.08) : rowHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
              Accessible.role: Accessible.StaticText
              Accessible.name: app + (title ? ", " + title : "") + (inUse ? ", in use by an agent" : task ? ", left by an agent" : "")
              HoverHandler { id: rowHover }

              Rectangle {
                x: Style.space(10)
                anchors.verticalCenter: parent.verticalCenter
                width: Style.space(8)
                height: width
                radius: 0
                color: row.inUse ? root.tokens.workingColor : row.task ? root.tokens.pausedColor : root.tokens.faint
              }
              Column {
                id: info
                x: Style.space(28)
                anchors.verticalCenter: parent.verticalCenter
                width: buttons.x - x - Style.space(10)
                spacing: Style.space(2)
                Item {
                  width: parent.width
                  height: appText.implicitHeight
                  Copy {
                    id: appText
                    width: Math.min(implicitWidth, parent.width * 0.4)
                    text: row.app
                    font.bold: true
                    wrapMode: Text.NoWrap
                    elide: Text.ElideRight
                  }
                  Copy {
                    anchors.left: appText.right
                    anchors.leftMargin: Style.space(8)
                    anchors.right: parent.right
                    text: row.title
                    wrapMode: Text.NoWrap
                    elide: Text.ElideRight
                  }
                }
                Item {
                  visible: !!row.task
                  width: parent.width
                  height: badge.implicitHeight
                  Copy {
                    id: badge
                    text: row.inUse ? "In Use by an Agent" : "Left by an Agent"
                    color: root.tokens.textTint(row.inUse ? root.tokens.workingColor : root.tokens.pausedColor)
                    font.pixelSize: Style.font.bodySmall
                    font.bold: true
                    wrapMode: Text.NoWrap
                  }
                  Copy {
                    visible: !row.inUse
                    anchors.left: badge.right
                    anchors.leftMargin: Style.space(8)
                    anchors.right: parent.right
                    text: row.task ? String(row.task.goal || "") : ""
                    color: root.tokens.foreground
                    font.pixelSize: Style.font.bodySmall
                    wrapMode: Text.NoWrap
                    elide: Text.ElideRight
                  }
                }
                Copy {
                  visible: !!row.note
                  width: parent.width
                  text: row.note ? row.note.text : ""
                  color: root.tokens.textTint(row.note && row.note.busy ? root.tokens.attentionColor : root.tokens.pausedColor)
                  font.pixelSize: Style.font.bodySmall
                }
              }
              Row {
                id: buttons
                anchors.right: parent.right
                anchors.rightMargin: Style.space(8)
                anchors.verticalCenter: parent.verticalCenter
                spacing: Style.space(6)
                ActionButton {
                  id: stopButton
                  visible: row.stopRef !== ""
                  size: "small"
                  role: "danger"
                  label: "Stop Task"
                  blocked: root.actionReason !== ""
                  disabledReason: root.actionReason
                  Accessible.name: "Stop the task using " + root.windowTitle(row.win)
                  onActiveFocusChanged: if (activeFocus) root.noteFocus(row.address, "stop")
                  onClicked: if (!blocked) root.stopTask(row.address, row.stopRef)
                }
                ActionButton {
                  id: closeButton
                  size: "small"
                  label: "Close"
                  blocked: row.reason !== ""
                  disabledReason: row.reason
                  tooltipText: disabledReason || (root.isTerminal(row.win) ? "Closes this terminal; anything running in it stops" : "Asks " + row.app + " to close this window")
                  Accessible.name: "Close " + root.windowTitle(row.win)
                  onActiveFocusChanged: if (activeFocus) root.noteFocus(row.address, "close")
                  onClicked: if (!blocked) root.closeWindow(row.win)
                }
                ActionMenu {
                  id: moveMenu
                  size: "small"
                  role: "secondary"
                  label: "Move To…"
                  accessibleName: "Move " + root.windowTitle(row.win) + " to another workspace"
                  tooltipText: "Moves it without changing what's on screen there"
                  blocked: row.reason !== ""
                  disabledReason: row.reason
                  items: root.moveItems(section.space)
                  onTriggered: id => root.moveWindow(row.win, Number(id))
                  onOpenedChanged: {
                    if (opened) root.menuAddress = row.address
                    else if (root.menuAddress === row.address) root.menuAddress = ""
                  }
                  Connections {
                    target: moveMenu.button
                    function onActiveFocusChanged() { if (moveMenu.button.activeFocus) root.noteFocus(row.address, "move") }
                  }
                }
              }
            }
          }
        }
      }
      Item { width: 1; height: Style.space(8) }
    }
  }
}
