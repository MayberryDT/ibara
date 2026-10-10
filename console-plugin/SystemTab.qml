import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Health, logs and upkeep for the open computer, with power kept apart. What can't be undone
// from here (restart, shut down, sleep, lock, update, Remove Computer) is confirmed by name, right
// beside the button, and runs only if the same computer is still open.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  readonly property Tokens tokens: Tokens {}
  readonly property string computerLabel: tokens.label(computer)
  readonly property var health: service ? service.health || ({}) : ({})
  readonly property var healthLines: StatusModel.healthLines(health)
  readonly property bool offline: tokens.stateOf(computer) === "offline"
  readonly property string actionReason: host ? host.powerReason(computer) : "Connecting."
  readonly property string updateReason: host ? host.updateReason(computer, "update_ibara") : "Connecting."
  readonly property var lastRepair: health.repair && health.repair.last && typeof health.repair.last === "object" ? health.repair.last : null
  // Omarchy's update there: running, or how the last one ended, when, and why a failed one failed.
  readonly property var omarchyUpdate: computer && computer.omarchy_update ? computer.omarchy_update : null
  readonly property string omarchyUpdateLine: {
    var u = omarchyUpdate
    if (!u) return ""
    if (u.state === "running") return tokens.ink("Updating Omarchy…", tokens.textTint(tokens.workingColor))
    var when = u.finished_at > 0 ? tokens.ink(" · " + tokens.changedLabel(new Date(u.finished_at).toISOString(), service ? service.nowMs : Date.now()), tokens.dim) : ""
    if (u.state === "failed") return tokens.ink("Omarchy's last update failed", tokens.textTint(tokens.attentionColor)) + when + "." +
      (u.message ? "<br>" + tokens.ink(u.message, tokens.textTint(tokens.attentionColor)) : "") + "<br>Choose Update Omarchy to try again."
    return tokens.ink("Omarchy updated", tokens.textTint(tokens.readyColor)) + when + (u.restart_needed ? tokens.ink(" · restart to finish", tokens.textTint(tokens.pausedColor)) : "")
  }

  readonly property var ibaraUpdate: computer && computer.ibara_update ? computer.ibara_update : null
  readonly property var latest: service ? service.latestRelease : null
  property bool updateNotesOpen: false
  readonly property var releaseNotes: service && service.behind(computer) ? StatusModel.releaseNotes(latest, computer.version) : []
  function reload() {
    if (!service || !visible || !computerId) return
    service.loadHealth()
    if (!service.tailnet) service.loadTailnet()
  }
  onVisibleChanged: reload()
  onComputerIdChanged: reload()

  component Group: Rectangle {
    default property alias content: groupColumn.data
    property string title: ""
    property color accentColor: root.tokens.rule
    width: parent ? parent.width : 0
    height: groupColumn.implicitHeight + Style.space(32)
    radius: 0
    color: root.tokens.surface
    border.width: 1
    border.color: accentColor
    Column {
      id: groupColumn
      x: Style.space(16)
      y: Style.space(16)
      width: parent.width - Style.space(32)
      spacing: Style.space(10)
      Copy { text: parent.parent.title; font.pixelSize: Style.font.title; font.bold: true }
    }
  }

  // Wide, health and upkeep on the left, sharing and power on the right; narrower, one column.
  readonly property bool wide: width >= tokens.wideAt
  readonly property real columnWidth: wide ? Math.min(tokens.columnMax, Math.floor((width - tokens.columnGap) / 2)) : Math.min(width, tokens.columnMax)

  Flickable {
    id: scroll
    anchors.fill: parent
    clip: true
    contentWidth: width
    contentHeight: root.wide ? Math.max(column.implicitHeight, side.implicitHeight) : side.y + side.implicitHeight
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Column {
      id: column
      width: root.columnWidth
      spacing: Style.space(16)

      Group {
        title: "Health and upkeep"
        Repeater {
          model: root.healthLines
          delegate: Copy {
            required property var modelData
            readonly property string line: String(modelData)
            width: parent ? parent.width : 0
            textFormat: Text.StyledText
            text: line.indexOf("Busy: ") === 0 ? root.tokens.labeled(line, root.tokens.textTint(root.tokens.statusColor(line.slice(6))))
              : line.indexOf("Memory ") === 0 || line.indexOf("Disk ") === 0 ? root.tokens.ink(line.slice(0, line.indexOf(" ") + 1), root.tokens.dim) + root.tokens.ink(line.slice(line.indexOf(" ") + 1), root.tokens.foreground)
              : root.tokens.ink(line, root.tokens.dim)
            font.pixelSize: Style.font.bodySmall
          }
        }
        Copy {
          visible: root.healthLines.length === 0
          text: root.service && root.service.readPending("health") ? "Checking health…" : root.service && root.service.readErrors.health ? StatusModel.clip(root.service.readErrors.health, 200) : "No health report yet. Choose Refresh Health."
          color: root.service && root.service.readErrors.health ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.dim
          font.pixelSize: Style.font.bodySmall
        }
        Copy {
          id: lastRepairLine
          visible: !!root.lastRepair
          width: parent.width
          textFormat: Text.StyledText
          text: root.lastRepair ? root.tokens.ink("Last repair: ", root.tokens.dim) + root.tokens.ink(StatusModel.clip(root.lastRepair.summary, 160), root.tokens.foreground) + (root.lastRepair.at ? root.tokens.ink(" · " + root.tokens.changedLabel(new Date(StatusModel.timeMs(root.lastRepair.at)).toISOString(), root.service ? root.service.nowMs : Date.now()), root.tokens.dim) : "") : ""
          font.pixelSize: Style.font.bodySmall
        }
        Copy {
          visible: text !== ""
          width: parent.width
          textFormat: Text.StyledText
          text: root.omarchyUpdateLine
          color: root.tokens.textTint(root.tokens.statusColor(root.omarchyUpdate ? root.omarchyUpdate.state : ""))
          font.pixelSize: lastRepairLine.font.pixelSize
        }
        DataAge {
          visible: at > 0
          at: root.service && root.service.scopedReadAt.health || 0
          refreshing: !!root.service && root.service.readPending("health")
          nowMs: root.service ? root.service.nowMs : Date.now()
        }
        Copy {
          width: parent.width
          text: "ibara " + (root.computer && root.computer.version || "version pending")
          font.pixelSize: Style.font.bodySmall
        }
        Copy {
          visible: !!root.ibaraUpdate
          width: parent.width
          text: !root.ibaraUpdate ? "" : root.ibaraUpdate.state === "running" ? "Updating ibara…"
            : root.ibaraUpdate.state === "failed" ? "Update failed: " + root.ibaraUpdate.message
            : "ibara " + root.ibaraUpdate.version + " installed" + (root.ibaraUpdate.shell === "deferred_locked" ? ". Its bar restarts after unlock." : ".")
          color: root.ibaraUpdate && root.ibaraUpdate.state === "failed" ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.foreground
          font.pixelSize: Style.font.bodySmall
        }
        Copy {
          visible: !!root.service && root.service.staleConsole && root.computerId === root.service.thisComputerId
          width: parent.width
          text: "The bar is still using ibara " + (root.service ? root.service.loadedPluginVersion : "") + ". Restart it to load the installed version."
          font.pixelSize: Style.font.bodySmall
          dimmed: true
        }
        ActionButton {
          visible: !!root.service && root.service.staleConsole && root.computerId === root.service.thisComputerId
          label: "Restart Bar"
          blocked: !!root.computer && root.computer.locked
          disabledReason: "Unlock this computer first."
          onClicked: if (!blocked) root.service.restartBar()
        }
        Repeater {
          model: root.updateNotesOpen ? root.releaseNotes : root.releaseNotes.slice(0, 3)
          delegate: Copy { required property var modelData; width: parent.width; text: String(modelData); font.pixelSize: Style.font.bodySmall; dimmed: true }
        }
        ActionButton {
          visible: root.releaseNotes.length > 3
          label: root.updateNotesOpen ? "Hide Release Notes" : "More Release Notes"
          onClicked: root.updateNotesOpen = !root.updateNotesOpen
        }
        Flow {
          width: parent.width
          spacing: Style.space(8)
          ActionButton { label: "Check for Updates"; onClicked: if (root.service) root.service.loadUpdateCheck(true) }
          ActionButton { label: "Refresh Health"; onClicked: root.service.loadHealth() }
          ActionButton {
            label: "Show ibara Log"
            selected: root.service && root.service.logLines.length > 0 && root.service.logsWhich === "ibara"
            onClicked: root.service.loadLogs("ibara")
          }
          ActionButton {
            label: "Show Screen Sharing Log"
            selected: root.service && root.service.logLines.length > 0 && root.service.logsWhich === "viewer"
            onClicked: root.service.loadLogs("viewer")
          }
          ActionButton {
            label: "Open Terminal"
            tooltipText: "A terminal on this desktop, signed in to " + root.computerLabel
            blocked: !root.service || root.service.mutating
            disabledReason: blocked && root.service ? "Wait for the current action to finish." : ""
            onClicked: if (!blocked) root.service.openTerminalFor(root.computerId)
          }
          ActionButton {
            label: "Lock Screen"
            blocked: root.actionReason !== ""
            disabledReason: root.actionReason
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "lock")
          }
          ActionButton {
            label: root.service && root.service.behind(root.computer) ? "Update to " + root.latest.version : "Update ibara"
            tooltipText: "Installs ibara's latest signed release on " + root.computerLabel + ". Nobody needs to be there."
            blocked: root.updateReason !== ""
            disabledReason: root.updateReason
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "update_ibara")
          }
          ActionButton {
            label: "Update Omarchy"
            tooltipText: "Runs Omarchy's system update on " + root.computerLabel + " with no questions. Nobody needs to be there."
            blocked: (root.host ? root.host.updateReason(root.computer, "update_omarchy") : "Connecting.") !== ""
            disabledReason: root.host ? root.host.updateReason(root.computer, "update_omarchy") : "Connecting."
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "update_omarchy")
          }
        }
        Copy {
          visible: !!root.service && root.service.readErrors.logs !== undefined
          width: parent.width
          text: root.service && root.service.readErrors.logs ? StatusModel.clip(root.service.readErrors.logs, 200) : ""
          color: root.tokens.textTint(root.tokens.attentionColor)
          font.pixelSize: Style.font.bodySmall
        }
        Rectangle {
          visible: !!(root.service && root.service.logLines.length)
          width: parent.width
          height: Math.min(Style.space(260), logsText.implicitHeight + Style.space(20))
          radius: 0
          color: Qt.alpha(Color.popups.text, 0.04)
          border.width: 1
          border.color: root.tokens.rule
          Flickable {
            anchors.fill: parent
            anchors.margins: Style.space(10)
            clip: true
            contentWidth: width
            contentHeight: logsText.implicitHeight
            contentY: Math.max(0, contentHeight - height)
            boundsBehavior: Flickable.StopAtBounds
            flickableDirection: Flickable.VerticalFlick
            Copy {
              id: logsText
              width: parent.width
              text: root.service ? root.service.logLines.join("\n") : ""
              font.family: "monospace"
              font.pixelSize: Style.font.caption
              wrapMode: Text.WrapAnywhere
            }
          }
        }
      }
    }

    Column {
      id: side
      x: root.wide ? root.columnWidth + root.tokens.columnGap : 0
      y: root.wide ? 0 : column.implicitHeight + Style.space(16)
      width: root.columnWidth
      spacing: Style.space(16)

      Group {
        title: "Share with a friend"
        visible: !!root.service && root.service.thisComputerId !== "" && root.computerId === root.service.thisComputerId
        Copy {
          width: parent.width
          text: "Let a friend use this computer from theirs, with what you choose and for as long as you choose."
          dimmed: true
          font.pixelSize: Style.font.bodySmall
        }
        ActionButton { label: "Share This Computer"; onClicked: if (root.host) root.host.showShare() }
      }

      Group {
        title: "Power"
        accentColor: Qt.alpha(Color.urgent, 0.5)
        Copy {
          width: parent.width
          text: root.offline
            ? (root.computer && root.computer.wake ? root.computerLabel + " is off or asleep. Wake sends it the signal to turn on" + (root.computer.wake.kind === "wifi" ? " over Wi-Fi" : "") + "." : root.computerLabel + " isn't answering, and ibara has no way to wake it from here.")
            : "Restart, Shut Down and Sleep stop " + root.computerLabel + "'s agents until it's back." + (root.computer && root.computer.wake ? " Wake turns it on again from here." : "")
          dimmed: true
          font.pixelSize: Style.font.bodySmall
        }
        Copy {
          visible: !!root.computer && (root.computer.disk_password === true || root.health.disk_password === true)
          width: parent.width
          text: root.computerLabel + "'s disk asks for its password when it starts, so after a restart someone must type it there before ibara can reach it again."
          color: root.tokens.textTint(root.tokens.attentionColor)
          font.pixelSize: Style.font.bodySmall
        }
        Row {
          spacing: Style.space(8)
          ActionButton {
            visible: root.offline && !!root.computer && !!root.computer.wake
            label: root.service && root.service.busy["wake:" + root.computerId] ? "Waking…" : "Wake"
            role: "primary"
            blocked: !!root.service && !!root.service.busy["wake:" + root.computerId]
            onClicked: if (!blocked) root.service.wake(root.computerId)
          }
          ActionButton {
            label: "Restart"
            role: "danger"
            blocked: root.actionReason !== ""
            disabledReason: root.actionReason
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "restart")
          }
          ActionButton {
            label: "Shut Down"
            role: "danger"
            blocked: root.actionReason !== ""
            disabledReason: root.actionReason
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "shutdown")
          }
          ActionButton {
            label: "Sleep"
            role: "danger"
            blocked: root.actionReason !== ""
            disabledReason: root.actionReason
            onClicked: if (!blocked && root.host) root.host.confirmPower(root.computerId, "sleep")
          }
        }
      }

      Group {
        title: "Remove from your fleet"
        Copy {
          width: parent.width
          text: "Takes " + root.computerLabel + " off this computer's fleet, as when it was reinstalled and no longer answers as itself. Add Computer adds it again."
          dimmed: true
          font.pixelSize: Style.font.bodySmall
        }
        ActionButton {
          id: removeButton
          label: "Remove Computer"
          role: "danger"
          blocked: !root.service || root.service.mutating
          disabledReason: blocked && root.service ? "Wait for the current action to finish." : ""
          onClicked: if (!blocked && root.host) root.host.confirmRemove(root.computerId, removeButton)
        }
      }
    }
  }
}
