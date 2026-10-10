import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Send a file to this computer, follow this session's transfers, and get files from
// its approved folder. Listings and transfers stay bound to the computer and folder that
// started them; Service verifies every transfer before calling it done.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  property string rootId: ""
  property string directory: "."
  readonly property Tokens tokens: Tokens {}
  readonly property string computerLabel: tokens.label(computer)
  readonly property string home: Quickshell.env("HOME") || ""
  readonly property bool ready: !!computer && computer.trust_state === "verified" && !!computer.controller_epoch && !!computer.endpoint_id &&
    computer.binding_revision !== undefined && computer.binding_revision !== null
  readonly property bool mine: !!service && service.remoteFileComputerId === computerId
  readonly property var roots: mine ? service.remoteFileRoots || [] : []
  readonly property var entries: mine && service.remoteFileEntriesLoaded ? service.remoteFileEntries || [] : []
  readonly property bool listing: mine && !!service.remoteFileListing
  readonly property bool rootsLoaded: mine && !!service.remoteFileLoaded
  readonly property string listError: mine ? String(service.remoteFileError || "") : ""
  readonly property bool revoked: listError.indexOf("Revoked") === 0
  // A folder that can't be read says why in an error toast, while this tab shows; the list
  // itself stays empty rather than carry the message.
  readonly property string shownError: visible ? listError : ""
  onShownErrorChanged: if (host) host.showToast("page-files", shownError, true)
  Component.onCompleted: if (host && shownError) host.showToast("page-files", shownError, true)
  readonly property var picked: service ? service.pickedFile : null
  readonly property var pickedFolder: service ? service.pickedFolder : null
  readonly property var transfers: service && Array.isArray(service.fileTransfers) ? service.fileTransfers : []
  readonly property string saveAsName: saveAs.text.trim()

  function shortPath(path) {
    var text = String(path || "")
    return home && text.indexOf(home) === 0 ? "~" + text.substring(home.length) : text
  }
  function rootLabel(id) { return tokens.sentence(id) || "Folder" }
  function folderParts() {
    var parts = rootId ? [rootLabel(rootId)] : []
    if (directory !== ".") parts = parts.concat(directory.split("/"))
    return parts
  }
  function folderLabel() { return folderParts().join(" › ") }
  function remotePath(name) { return directory === "." ? String(name) : directory + "/" + String(name) }
  function computerName(id) {
    var session = service && service.sessions ? service.sessions[id] : null
    return tokens.label(session)
  }
  function load() {
    if (!service || !visible || !computerId) return
    if (!mine || (!service.remoteFileLoaded && !listing)) {
      rootId = ""
      directory = "."
      service.listRemoteFileRoots(computerId)
      // Back on a computer, the service puts back the folder it last showed here.
      if (mine && service.remoteFileRootId) { rootId = String(service.remoteFileRootId); directory = String(service.remoteFileDirectory || ".") }
    } else chooseRoot()
  }
  function reload() {
    if (!service || !computerId) return
    if (rootId && mine && service.remoteFileLoaded) openFolder(rootId, directory)
    else { rootId = ""; directory = "."; service.listRemoteFileRoots(computerId) }
  }
  // Folders load on their own: the first approved folder opens once the list arrives.
  function chooseRoot() {
    if (listing || !mine || !service.remoteFileLoaded) return
    for (var i = 0; i < roots.length; i++) if (String(roots[i].root_id) === rootId) return
    if (rootId && host) host.notify("The folder you were using is no longer offered by " + computerLabel + ".", false)
    rootId = ""
    directory = "."
    if (roots.length) openFolder(String(roots[0].root_id), ".")
  }
  function openFolder(id, dir) {
    if (!service || service.humanRootId(id) !== id || service.humanRemoteDirectory(dir) !== dir) return
    rootId = id
    directory = dir
    service.listRemoteFileDirectory(computerId, id, dir)
  }
  function openEntry(entry) {
    if (!entry || entry.kind !== "directory" || !service.humanFileSegment(entry.name)) return
    openFolder(rootId, remotePath(entry.name))
  }
  function openCrumb(index) {
    if (index <= 0) { openFolder(rootId, "."); return }
    openFolder(rootId, directory.split("/").slice(0, index).join("/"))
  }
  function blockedCommon() {
    if (!service) return "Connecting."
    if (service.denied) return "You don't have access."
    if (!ready) return computer && computer.trust_state !== "verified" ? computerLabel + " isn't paired from here. Pair it again from Add Computer." : "ibara is still connecting to " + computerLabel + "."
    if (computer && computer.files_access === "denied") return "You can't move files on " + computerLabel + "."
    if (revoked) return listError
    if (service.mutating) return "Wait for the current transfer or action to finish."
    if (listing) return "Wait for the folder to load."
    return ""
  }
  function uncertainSend() {
    for (var i = 0; i < transfers.length; i++) {
      var t = transfers[i]
      if (t.direction === "send" && t.state === "failed" && t.retryable && t.computerId === computerId && t.rootId === rootId && t.remote === remotePath(saveAsName)) return true
    }
    return false
  }
  function nameTaken() {
    for (var i = 0; i < entries.length; i++) if (String(entries[i].name) === saveAsName) return true
    return false
  }
  function sendReason() {
    var common = blockedCommon()
    if (common) return common
    if (!picked) return "Choose a file first."
    if (!rootId) return computerLabel + " has no folder for files from here."
    if (!service.humanFileSegment(saveAsName)) return "Use a name without slashes that doesn't start with a dot or a dash."
    if (uncertainSend()) return "An earlier send with this name stopped part-way. Use Retry under Transfers."
    if (nameTaken()) return "A file called " + saveAsName + " is already there. Choose another name."
    return ""
  }
  // A failed start shows Service's own reason when it just gave one, else this tab's.
  function reportStart(prior, started, fallback) {
    if (!host) return
    host.notify(started || String(service.actionError || "") !== prior ? "" : fallback, true)
  }
  function send() {
    if (sendReason()) return
    var prior = String(service.actionError || "")
    reportStart(prior, service.sendPickedFile(computerId, rootId, saveAsName, directory), "The file was not sent. Check the file, the name and the folder, then try again.")
  }
  function getEntry(entry) {
    if (!entry || entry.kind === "directory" || blockedCommon() || !rootId) return
    var prior = String(service.actionError || "")
    reportStart(prior, service.getRemoteFile(computerId, rootId, remotePath(entry.name)), "The file was not fetched. Refresh the folder and try again.")
  }

  onVisibleChanged: load()
  onComputerIdChanged: { rootId = ""; directory = "."; load() }
  onReadyChanged: if (ready) load()
  // Deferred: opening a folder sets the listing flag these handlers watch.
  onRootsChanged: Qt.callLater(chooseRoot)
  onListingChanged: Qt.callLater(chooseRoot)
  onRootsLoadedChanged: Qt.callLater(chooseRoot)
  // A verified send into the open folder shows up in the list without a manual refresh.
  property string lastVerifiedSend: ""
  onTransfersChanged: {
    for (var i = 0; i < transfers.length; i++) {
      var t = transfers[i]
      if (t.direction !== "send" || t.state !== "verified" || t.computerId !== computerId) continue
      if (String(t.id) !== lastVerifiedSend) {
        lastVerifiedSend = String(t.id)
        if (visible && t.rootId === rootId) Qt.callLater(reload)
      }
      return
    }
  }
  onPickedChanged: { saveAs.text = picked ? String(picked.name || "") : ""; if (host) host.notify("", true) }

  component TableRow: Rectangle {
    id: tableRow
    property var entry: null
    property int row: -1
    readonly property bool folder: !!entry && entry.kind === "directory"
    readonly property RowWindow rows: ListView.view ? ListView.view.parent as RowWindow : null
    width: parent ? parent.width : 0
    height: rows ? rows.rowHeight : 0
    radius: 0
    color: activeFocus || rowHover.hovered ? Qt.alpha(Color.accent, 0.10) : "transparent"
    border.width: activeFocus ? Math.max(2, Style.space(2)) : 0
    border.color: Color.accent
    activeFocusOnTab: true
    Accessible.role: Accessible.Button
    Accessible.name: (folder ? "Folder " : "File ") + String(entry ? entry.name : "")
    Keys.onReturnPressed: folder ? root.openEntry(entry) : root.getEntry(entry)
    Keys.onEnterPressed: folder ? root.openEntry(entry) : root.getEntry(entry)
    Keys.onTabPressed: function(event) { event.accepted = tableRow.rows.step(tableRow.row, tableRow, event) }
    Keys.onBacktabPressed: function(event) { event.accepted = tableRow.rows.step(tableRow.row, tableRow, event) }
    onActiveFocusChanged: if (activeFocus) rows.focused(row, tableRow)
    HoverHandler { id: rowHover }
    MouseArea { anchors.fill: parent; onClicked: { tableRow.forceActiveFocus(); if (tableRow.folder) root.openEntry(tableRow.entry) } }
    Copy {
      x: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: parent.width * 0.55 - x
      text: String(tableRow.entry ? tableRow.entry.name : "") + (tableRow.folder ? "  ›" : "")
      font.bold: tableRow.folder
      wrapMode: Text.NoWrap
      elide: Text.ElideMiddle
    }
    Copy {
      x: parent.width * 0.55
      anchors.verticalCenter: parent.verticalCenter
      text: tableRow.folder || !tableRow.entry || typeof tableRow.entry.size !== "number" ? "" : StatusModel.bytesLabel(tableRow.entry.size)
      dimmed: true
    }
    Copy {
      x: parent.width * 0.72
      anchors.verticalCenter: parent.verticalCenter
      text: tableRow.entry ? root.tokens.changedLabel(tableRow.entry.modified, root.service ? root.service.nowMs : 0) : ""
      dimmed: true
    }
    ActionButton {
      id: getButton
      visible: !tableRow.folder
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      label: "Get"
      size: "small"
      role: tableRow.activeFocus || rowHover.hovered ? "primary" : "secondary"
      blocked: root.blockedCommon() !== ""
      disabledReason: root.blockedCommon()
      Accessible.name: "Get " + String(tableRow.entry ? tableRow.entry.name : "")
      onClicked: if (!blocked) root.getEntry(tableRow.entry)
      Keys.onTabPressed: function(event) { event.accepted = tableRow.rows.step(tableRow.row, getButton, event) }
      Keys.onBacktabPressed: function(event) { event.accepted = tableRow.rows.step(tableRow.row, getButton, event) }
      onActiveFocusChanged: if (activeFocus) tableRow.rows.focused(tableRow.row, getButton)
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // ---- left: send and transfers
  Flickable {
    id: leftScroll
    anchors.left: parent.left
    anchors.top: parent.top
    anchors.bottom: parent.bottom
    width: Math.min(Style.space(520), parent.width * 0.4)
    clip: true
    contentWidth: width
    contentHeight: leftColumn.implicitHeight
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Column {
      id: leftColumn
      width: leftScroll.width
      spacing: Style.space(18)
      Rectangle {
        width: parent.width
        height: sendColumn.implicitHeight + Style.space(36)
        radius: 0
        color: root.tokens.surface
        border.width: 1
        border.color: root.tokens.rule
        Column {
          id: sendColumn
          x: Style.space(18)
          y: Style.space(18)
          width: parent.width - Style.space(36)
          spacing: Style.space(12)
          Copy { width: parent.width; text: "Send a file to " + root.computerLabel; font.pixelSize: Style.font.heading; font.bold: true }
          Copy {
            visible: !!root.rootId
            width: parent.width
            text: "It arrives in " + root.computerLabel + "'s " + root.folderLabel() + " folder. Nothing there is overwritten."
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Flow {
            visible: root.roots.length > 1
            width: parent.width
            spacing: Style.space(6)
            Repeater {
              model: root.roots.length > 1 ? root.roots : []
              delegate: ActionButton {
                label: root.rootLabel(String(modelData.root_id))
                selected: root.rootId === String(modelData.root_id)
                size: "small"
                onClicked: root.openFolder(String(modelData.root_id), ".")
              }
            }
          }
          Rectangle {
            width: parent.width
            height: Style.space(56)
            radius: 0
            color: "transparent"
            border.width: 1
            border.color: root.tokens.rule
            Rectangle {
              id: fileIcon
              x: Style.space(10)
              anchors.verticalCenter: parent.verticalCenter
              width: Style.space(30)
              height: Style.space(34)
              radius: 0
              color: "transparent"
              border.width: 1
              border.color: Qt.alpha(Color.popups.text, 0.4)
              Copy {
                anchors.centerIn: parent
                text: {
                  var name = root.picked ? String(root.picked.name || "") : ""
                  var dot = name.lastIndexOf(".")
                  return dot > 0 ? name.substring(dot + 1, dot + 4).toUpperCase() : "—"
                }
                font.pixelSize: Style.font.caption
                dimmed: true
              }
            }
            Column {
              anchors.left: fileIcon.right
              anchors.leftMargin: Style.space(12)
              anchors.right: chooseFile.left
              anchors.rightMargin: Style.space(10)
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(2)
              Copy {
                width: parent.width
                text: root.picked ? String(root.picked.name || "") : "No file chosen"
                font.bold: !!root.picked
                dimmed: !root.picked
                wrapMode: Text.NoWrap
                elide: Text.ElideMiddle
              }
              Copy {
                visible: !!root.picked
                width: parent.width
                text: root.picked ? root.shortPath(root.picked.folder) + (typeof root.picked.size === "number" ? " · " + StatusModel.bytesLabel(root.picked.size) : "") : ""
                dimmed: true
                font.pixelSize: Style.font.caption
                wrapMode: Text.NoWrap
                elide: Text.ElideMiddle
              }
            }
            ActionButton {
              id: chooseFile
              anchors.right: parent.right
              anchors.rightMargin: Style.space(10)
              anchors.verticalCenter: parent.verticalCenter
              label: root.service && root.service.picking ? "Choosing…" : root.picked ? "Change…" : "Choose File…"
              blocked: !root.service || !!root.service.picking
              onClicked: if (!blocked) root.service.pickLocalFile()
            }
          }
          Row {
            width: parent.width
            spacing: Style.space(10)
            Copy { id: saveAsLabel; anchors.verticalCenter: parent.verticalCenter; text: "Save as"; dimmed: true }
            FieldInput {
              id: saveAs
              width: parent.width - saveAsLabel.width - parent.spacing
              placeholder: "File name on " + root.computerLabel
              accessibleName: "Save as"
              onAccepted: root.send()
            }
          }
          ActionButton {
            label: "Send to " + root.computerLabel
            role: "primary"
            blocked: root.sendReason() !== ""
            disabledReason: root.sendReason()
            onClicked: root.send()
          }
          Copy {
            visible: !!root.picked && root.sendReason() !== ""
            width: parent.width
            text: root.sendReason()
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
        }
      }

      Column {
        width: parent.width
        spacing: Style.space(10)
        Copy { eyebrow: true; text: "Transfers" }
        Repeater {
          model: root.transfers
          delegate: Column {
            width: leftColumn.width
            spacing: Style.space(5)
            property var t: modelData
            Item {
              width: parent.width
              height: Math.max(transferName.implicitHeight, transferState.implicitHeight)
              Copy {
                id: transferName
                anchors.left: parent.left
                anchors.right: transferState.left
                anchors.rightMargin: Style.space(10)
                text: String(t.name || "File") + (t.direction === "receive" ? " ← " : " → ") + root.computerName(t.computerId)
                wrapMode: Text.NoWrap
                elide: Text.ElideMiddle
              }
              Copy {
                id: transferState
                anchors.right: parent.right
                text: {
                  if (t.state === "verified") return "✓ " + (t.direction === "receive" ? "received" : "sent") + " · checked"
                  if (t.state === "failed") return t.direction === "receive" ? "Not received" : "Not sent"
                  var total = Number(t.total || 0), bytes = Number(t.bytes || 0)
                  if (total > 0 && bytes > 0) return Math.floor(bytes * 100 / total) + "% · " + StatusModel.bytesLabel(bytes) + " of " + StatusModel.bytesLabel(total)
                  return (t.direction === "receive" ? "Receiving" : "Sending") + (total > 0 ? " " + StatusModel.bytesLabel(total) : "") + "…"
                }
                color: root.tokens.textTint(t.state === "verified" ? root.tokens.readyColor : t.state === "failed" ? root.tokens.attentionColor : root.tokens.workingColor)
                font.pixelSize: Style.font.bodySmall
                wrapMode: Text.NoWrap
              }
            }
            // Byte counts arrive only with the receipt, so a running transfer shows movement, not a guess.
            Rectangle {
              id: track
              visible: t.state === "running"
              width: parent.width
              height: Style.space(4)
              radius: 0
              color: Qt.alpha(Color.popups.text, 0.12)
              clip: true
              Rectangle {
                id: runner
                height: parent.height
                radius: 0
                width: Number(t.total || 0) > 0 && Number(t.bytes || 0) > 0 ? parent.width * Math.min(1, Number(t.bytes) / Number(t.total)) : parent.width * 0.25
                color: Color.accent
                SequentialAnimation on x {
                  running: track.visible && !(Number(t.bytes || 0) > 0)
                  loops: Animation.Infinite
                  NumberAnimation { from: -runner.width; to: track.width; duration: 1400 }
                }
              }
            }
            Item {
              visible: t.state === "failed"
              width: parent.width
              height: visible ? Math.max(failText.implicitHeight, retry.implicitHeight) : 0
              Copy {
                id: failText
                anchors.left: parent.left
                anchors.right: retry.visible ? retry.left : parent.right
                anchors.rightMargin: Style.space(10)
                anchors.verticalCenter: parent.verticalCenter
                text: StatusModel.clip(String(t.error || "It stopped before it could be checked."), 240)
                color: root.tokens.textTint(root.tokens.attentionColor)
                font.pixelSize: Style.font.caption
              }
              ActionButton {
                id: retry
                visible: !!t.retryable
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                label: "Retry"
                size: "small"
                blocked: !root.service || !!root.service.mutating || t.computerId !== root.computerId
                disabledReason: !root.service ? "" : t.computerId !== root.computerId ? "Open " + root.computerName(t.computerId) + "'s Files tab to retry." : blocked ? "Wait for the current transfer or action to finish." : ""
                onClicked: if (!blocked) root.service.retryTransfer(t)
              }
            }
          }
        }
        Copy {
          visible: root.transfers.length === 0
          text: "No transfers yet."
          dimmed: true
          font.pixelSize: Style.font.bodySmall
        }
        ActionButton {
          label: "Notify Me When a Transfer Is Checked"
          glyph: root.service && root.service.followHumanFile ? "◉" : "○"
          role: "quiet"
          selected: !!(root.service && root.service.followHumanFile)
          size: "small"
          onClicked: if (root.service) root.service.followHumanFile = !root.service.followHumanFile
        }
      }
    }
  }

  // ---- right: the computer's folder
  Item {
    id: right
    anchors.left: leftScroll.right
    anchors.leftMargin: Style.space(24)
    anchors.right: parent.right
    anchors.top: parent.top
    anchors.bottom: parent.bottom
    Item {
      id: folderHeader
      width: parent.width
      height: Style.space(40)
      Row {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(8)
        Copy { anchors.verticalCenter: parent.verticalCenter; text: "On " + root.computerLabel; font.pixelSize: Style.font.heading; font.bold: true }
        Repeater {
          model: root.folderParts()
          delegate: Row {
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(6)
            Copy { visible: index > 0; anchors.verticalCenter: parent.verticalCenter; text: "›"; dimmed: true }
            ActionButton {
              anchors.verticalCenter: parent.verticalCenter
              label: String(modelData)
              role: "quiet"
              size: "small"
              selected: index === root.folderParts().length - 1
              onClicked: root.openCrumb(index)
            }
          }
        }
      }
      DataAge {
        anchors.right: refreshButton.left
        anchors.rightMargin: Style.space(10)
        anchors.verticalCenter: parent.verticalCenter
        at: root.mine && !root.listError && root.service ? root.service.remoteFileReadAt : 0
        refreshing: root.listing
        nowMs: root.service ? root.service.nowMs : Date.now()
      }
      ActionButton {
        id: refreshButton
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        label: "Refresh"
        blocked: !root.ready
        onClicked: if (!blocked) root.reload()
      }
    }
    Row {
      id: tableHead
      anchors.top: folderHeader.bottom
      anchors.topMargin: Style.space(10)
      width: parent.width
      height: Style.space(26)
      Copy { x: Style.space(10); width: parent.width * 0.55; eyebrow: true; text: "Name"; anchors.verticalCenter: parent.verticalCenter }
      Copy { width: parent.width * 0.17; eyebrow: true; text: "Size"; anchors.verticalCenter: parent.verticalCenter }
      Copy { eyebrow: true; text: "Changed"; anchors.verticalCenter: parent.verticalCenter }
    }
    Rectangle { anchors.top: tableHead.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
    Flickable {
      id: tableScroll
      anchors.top: tableHead.bottom
      anchors.topMargin: 1
      anchors.bottom: footer.top
      anchors.bottomMargin: Style.space(10)
      width: parent.width
      clip: true
      contentWidth: width
      contentHeight: table.implicitHeight
      boundsBehavior: Flickable.StopAtBounds
      flickableDirection: Flickable.VerticalFlick
      Column {
        id: table
        width: tableScroll.width
        RowWindow {
          flick: tableScroll
          width: table.width
          rowHeight: Style.space(40)
          model: root.entries
          delegate: TableRow { width: table.width; row: index; entry: modelData }
        }
        Copy {
          width: parent.width
          topPadding: Style.space(12)
          visible: root.entries.length === 0
          text: !root.ready ? root.blockedCommon()
            : root.listing ? "Loading…"
            : root.listError ? ""
            : root.mine && root.service.remoteFileLoaded && root.roots.length === 0 ? root.computerLabel + " has no folder shared for files."
            : root.rootId ? "This folder is empty." : ""
          color: Qt.alpha(Color.popups.text, 0.64)
        }
      }
    }
    Column {
      id: footer
      anchors.bottom: parent.bottom
      width: parent.width
      spacing: Style.space(8)
      Copy {
        width: parent.width
        text: root.pickedFolder ? "Get saves to " + root.shortPath(root.pickedFolder.path) + " and opens it when done."
          : "Get saves to " + root.shortPath(root.service ? root.service.downloadFolder : "~/Downloads") + "."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
      Row {
        spacing: Style.space(8)
        ActionButton {
          label: root.service && root.service.picking ? "Choosing…" : "Choose Folder…"
          blocked: !root.service || !!root.service.picking
          onClicked: if (!blocked) root.service.pickLocalFolder()
        }
        ActionButton {
          visible: !!root.pickedFolder
          label: "Use " + root.shortPath(root.service ? root.service.downloadFolder : "~/Downloads")
          onClicked: root.service.clearPickedFolder()
        }
      }
    }
  }
}
