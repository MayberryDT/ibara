import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// The computer's live screen at selected quality, as large as the tab allows in the screen's own
// shape, and a compact rail of one-line sections: Now, Who can use it, Recent files and, with two
// or more displays, Display. On a wide window the rail stands at the right and the picture takes
// the rest of the width and the full height; on a narrow one the rail sits under the picture.
// Each section is one summary line until opened, and one opens at a time: a click, Enter or Space
// opens or closes it, Up and Down move between them. An open section shows at most eight lines;
// Show All opens the tab that has everything. Nothing in the rail scrolls.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  // The window is wide enough for the rail beside the picture (ComputerView decides).
  property bool railBeside: true
  // The one open section: "now", "access", "files", "display", or "" for none.
  property string openSection: ""
  readonly property Tokens tokens: Tokens {}
  readonly property int lineCap: 8
  readonly property string fleetState: tokens.stateOf(computer)
  readonly property var task: computer && computer.active_task && typeof computer.active_task === "object" ? computer.active_task : null
  readonly property string taskRef: task ? String(task.task_ref || "") : ""
  readonly property double nowMs: service ? service.nowMs : Date.now()
  readonly property var accessRows: service ? StatusModel.whoCanUse(service.accessTable, computer ? computer.operator_principal : "") : []
  readonly property var recentFiles: {
    var rows = service && Array.isArray(service.fileTransfers) ? service.fileTransfers : []
    return rows.filter(function(t) { return t.computerId === root.computerId })
  }
  // The live task's steps, newest first, once its detail is read: opening Now reads it.
  readonly property var steps: service && taskRef && service.selectedTaskDetailRef === taskRef ? StatusModel.listOf(service.selectedTaskDetail, "receipts") : []
  readonly property bool stepsLoading: !!service && !!taskRef && service.selectedTaskRef === taskRef && !!service.selectedTaskLoading
  readonly property var outputs: computer && Array.isArray(computer.outputs) ? computer.outputs : []
  // Two or more displays and none chosen: the Display section is the place to choose.
  readonly property bool choosingDisplay: outputs.length > 1 && !(computer && computer.display_id)
  readonly property var sectionOrder: outputs.length > 1 ? ["now", "access", "files", "display"] : ["now", "access", "files"]

  // What Now says on its line: the task, its agent and how long it has run (or its state when it
  // stopped); with no task, who holds the computer, else that nothing is running.
  readonly property string idleLine: fleetState === "offline" ? "Not answering"
    : tokens.actor(computer) !== "" ? tokens.activity(computer, nowMs) : "Nothing running"
  readonly property string taskAge: {
    var started = task ? StatusModel.isoMs(String(task.started_at || "")) : NaN
    return isFinite(started) ? StatusModel.durationLabel(nowMs - started) : ""
  }
  readonly property string nowSummary: {
    if (!task) return idleLine
    var parts = [StatusModel.clip(String(task.title || "Untitled task"), 120)]
    if (task.principal) parts.push(String(task.principal))
    var state = String(task.state || "")
    if (state && ["active", "running"].indexOf(state) === -1) parts.push(tokens.sentence(state).toLowerCase())
    else if (taskAge) parts.push(taskAge)
    return parts.join(" · ")
  }
  readonly property string nowSummaryMarkup: {
    if (!task) return tokens.ink(idleLine, tokens.textTint(tokens.stateColor(fleetState)))
    var parts = [tokens.ink(StatusModel.clip(String(task.title || "Untitled task"), 120), tokens.foreground)]
    if (task.principal) parts.push(tokens.ink(task.principal, tokens.foreground))
    var state = String(task.state || "")
    if (state && ["active", "running"].indexOf(state) === -1) parts.push(tokens.ink(tokens.sentence(state).toLowerCase(), tokens.textTint(tokens.statusColor(state))))
    else if (taskAge) parts.push(tokens.ink(taskAge, tokens.dim))
    return parts.join(tokens.ink(" · ", tokens.dim))
  }
  readonly property string accessStatus: service && service.readErrors.access ? StatusModel.clip(service.readErrors.access, 160)
    : !service || !service.accessTable || service.readPending("access") ? "Checking…" : "No one else"
  readonly property string filesSummary: {
    if (!recentFiles.length) return "None this session"
    var newest = recentFiles[0]
    return recentFiles.length + " · " + String(newest.name || "File") + (newest.state === "failed" ? " · failed" : newest.state === "verified" ? "" : " · in progress")
  }
  readonly property string displaySummary: {
    if (choosingDisplay) return "Choose one"
    for (var i = 0; i < outputs.length; i++)
      if (computer && outputs[i].display_id === computer.display_id) return String(outputs[i].label || outputs[i].display_id)
    return ""
  }

  // The screen's shape: the shown picture's, 16:9 until one shows.
  readonly property real aspect: preview.frameAspect > 0 ? preview.frameAspect : 16 / 9
  readonly property real gap: Style.space(20)
  readonly property real railWidth: Style.space(300)
  readonly property real stageWidth: Math.floor(Math.max(0, railBeside ? Math.min(width - railWidth - gap, height * aspect)
    : Math.min(width, (height - rail.height - gap) * aspect)))

  function reload() {
    if (!service || !visible || !computerId) return
    service.loadAccess()
    if (openSection === "now") readSteps()
  }
  function readSteps() { if (service && visible && taskRef) service.inspectTask(taskRef) }
  function toggle(key) {
    openSection = openSection === key ? "" : key
    if (openSection === "now") readSteps()
  }
  function sectionItem(key) { return key === "now" ? nowSection : key === "access" ? accessSection : key === "files" ? filesSection : displaySection }
  function moveFocus(key, delta) {
    var next = sectionOrder.indexOf(key) + delta
    if (next >= 0 && next < sectionOrder.length) sectionItem(sectionOrder[next]).focusHeader()
  }
  // One step on one line: its time, what it did, and a word when it failed or needs review.
  function stepProblem(step) {
    if (!step) return ""
    if (step.job_state === "unknown") return "needs review"
    if (["failed", "cancelled", "canceled", "interrupted", "timed_out"].indexOf(step.job_state) !== -1) return "failed"
    if (step.job_state === "running" || (step.execution === "running" && step.job_state !== "completed")) return "in progress"
    if (step.job_state === "completed" && step.execution === "running") return ""
    if (step.dependency_state === "unresolved" || step.requires_reconciliation || step.execution === "unknown" || step.effect === "unknown") return "needs review"
    return step.error ? "failed" : ""
  }
  function stepLine(step) {
    var ms = StatusModel.timeMs(step && step.created_at)
    var clock = isFinite(ms) ? tokens.clockLabel(new Date(ms).toISOString()) : ""
    var what = StatusModel.clip(String(step.summary || tokens.sentence(step.tool) || "Step"), 160)
    var problem = stepProblem(step)
    return (clock ? clock + "  " : "") + what + (problem ? " · " + problem : "")
  }
  function fileLine(file) {
    return String(file.name || "File") + (file.direction === "receive" ? "  ← received" : "  → sent") +
      (file.state === "verified" ? " · checked" : file.state === "failed" ? " · failed" : " · in progress")
  }
  function ruleWords(rule) { return rule === "allow" ? "allowed" : rule === "ask" ? "asks first" : "denied" }
  // Nerd Font glyphs (Omarchy's monospace font): Material Design eye, folder, mouse, the agent
  // robot the fleet uses, and a shield with a key.
  function markGlyph(key) {
    return key === "watch" ? "\u{F0208}" : key === "files" ? "\u{F024B}" : key === "control" ? "\u{F037D}" : key === "agents" ? "\u{F16A3}" : "\u{F0BC4}"
  }

  onVisibleChanged: reload()
  onComputerIdChanged: reload()
  onTaskRefChanged: if (openSection === "now") readSteps()
  onChoosingDisplayChanged: if (choosingDisplay) openSection = "display"
  Component.onCompleted: if (choosingDisplay) openSection = "display"
  // While Now is open on a live task, its steps are read again now and then.
  Timer {
    interval: 15000
    repeat: true
    running: root.visible && root.openSection === "now" && root.taskRef !== ""
    onTriggered: root.readSteps()
  }

  // One rail section: a one-line header that opens and closes, and its body while open.
  component RailSection: Column {
    id: section
    property string key: ""
    property string title: ""
    property string summary: ""
    property string summaryMarkup: root.tokens.escapeText(summary)
    property bool summaryUrgent: false
    property alias body: bodyLoader.sourceComponent
    readonly property bool open: root.openSection === key
    function focusHeader() { header.forceActiveFocus() }
    width: parent ? parent.width : 0
    Rectangle {
      id: header
      width: parent.width
      height: Style.space(28)
      radius: 0
      activeFocusOnTab: true
      color: headerHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
      border.width: activeFocus ? Math.max(2, Style.space(2)) : 0
      border.color: Color.accent
      Accessible.role: Accessible.Button
      Accessible.name: section.title
      Accessible.description: section.summary
      Accessible.checkable: true
      Accessible.checked: section.open
      Accessible.onPressAction: root.toggle(section.key)
      Keys.onReturnPressed: root.toggle(section.key)
      Keys.onEnterPressed: root.toggle(section.key)
      Keys.onSpacePressed: root.toggle(section.key)
      Keys.onUpPressed: root.moveFocus(section.key, -1)
      Keys.onDownPressed: root.moveFocus(section.key, 1)
      Text {
        id: chevron
        x: Style.space(4)
        anchors.verticalCenter: parent.verticalCenter
        width: Style.space(14)
        text: section.open ? "\u{F0140}" : "\u{F0142}"
        color: Qt.alpha(Color.popups.text, 0.64)
        font.family: Style.font.family
        font.pixelSize: Style.font.body
        Accessible.ignored: true
      }
      Copy {
        id: titleText
        anchors.left: chevron.right
        anchors.leftMargin: Style.space(4)
        anchors.verticalCenter: parent.verticalCenter
        text: section.title
        font.bold: true
        wrapMode: Text.NoWrap
      }
      Copy {
        id: summaryText
        anchors.left: titleText.right
        anchors.leftMargin: Style.space(10)
        anchors.right: parent.right
        anchors.rightMargin: Style.space(6)
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.StyledText
        text: section.summaryMarkup
        Accessible.name: section.summary
        dimmed: !section.summaryUrgent
        color: root.tokens.textTint(section.summaryUrgent ? root.tokens.attentionColor : section.key === "now" ? root.tokens.stateColor(root.fleetState) : root.tokens.dim)
        wrapMode: Text.NoWrap
        elide: Text.ElideRight
      }
      HoverHandler { id: headerHover }
      MouseArea {
        anchors.fill: parent
        cursorShape: Qt.PointingHandCursor
        onClicked: { header.forceActiveFocus(); root.toggle(section.key) }
      }
      // A line cut short reads whole under the pointer.
      Loader {
        active: headerHover.hovered && summaryText.truncated
        sourceComponent: PanelToolTip { parent: header; visible: true; text: section.summary }
      }
    }
    Loader {
      id: bodyLoader
      active: section.open
      visible: active
      x: Style.space(22)
      width: parent.width - x - Style.space(6)
    }
    Item { width: 1; height: section.open ? Style.space(10) : 0 }
    Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // Show All: the tab that has the whole list.
  component ShowAll: ActionButton {
    property string tab: ""
    property string tabLabel: ""
    label: "Show All"
    role: "quiet"
    size: "small"
    tooltipText: "Open the " + tabLabel + " tab"
    Accessible.name: "Show all on the " + tabLabel + " tab"
    onClicked: if (root.host) root.host.setTab(tab)
  }

  // A permission as a short mark: bright when allowed, with a "?" when it asks first, faint
  // when denied, so the marks line up in columns. The words are in its tooltip and its name.
  component PermissionMark: Item {
    id: markItem
    property var mark: ({})
    readonly property string words: String(mark.label) + ": " + root.ruleWords(mark.rule)
    width: Style.space(20)
    height: Style.space(18)
    Accessible.role: Accessible.StaticText
    Accessible.name: words
    Text {
      anchors.centerIn: parent
      text: root.markGlyph(markItem.mark.key)
      color: markItem.mark.rule === "deny" ? root.tokens.faint : root.tokens.textTint(root.tokens.ruleColor(markItem.mark.rule))
      font.family: Style.font.family
      font.pixelSize: Style.font.body + Style.space(1)
      Accessible.ignored: true
    }
    Text {
      visible: markItem.mark.rule === "ask"
      anchors.right: parent.right
      anchors.top: parent.top
      anchors.topMargin: -Style.space(2)
      text: "?"
      color: root.tokens.textTint(root.tokens.pausedColor)
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      font.bold: true
      Accessible.ignored: true
    }
    HoverHandler { id: markHover }
    Loader {
      active: markHover.hovered
      sourceComponent: PanelToolTip { parent: markItem; visible: true; text: markItem.words }
    }
  }

  Rectangle {
    id: stage
    width: root.stageWidth
    height: Math.round(width / root.aspect)
    radius: 0
    color: Qt.alpha(Color.popups.text, 0.08)
    clip: true
    SteadyPreview {
      id: preview
      anchors.fill: parent
      transformOrigin: Item.Center
      incoming: root.tokens.frameSource(root.computer, root.service && root.service.denied)
      drop: root.tokens.dropFrame(root.computer)
      onShownSizeSettled: (width, height) => { if (root.service) root.service.notePreviewSize("selected", width, height) }
      LiveVideo {
        service: root.service
        computerId: root.computerId
        preview: preview
        shown: !!root.service && root.service.consoleOpen && root.service.watchVisible && root.service.selectedComputerId === root.computerId
      }
    }
    PictureMoments {
      id: moments
      anchors.fill: parent
      service: root.service
      computerId: root.computerId
      tokens: root.tokens
      preview: preview
      fleetState: root.fleetState
    }
    Copy {
      anchors.centerIn: parent
      width: parent.width - Style.space(40)
      visible: !preview.hasFrame && !moments.arriving
      horizontalAlignment: Text.AlignHCenter
      text: root.computer && root.computer.frame_error ? (root.choosingDisplay ? "Choose a display under Display, " + (root.railBeside ? "on the right." : "below.") : String(root.computer.frame_error)) : root.fleetState === "offline" ? "No picture. This computer is not answering." : "Waiting for a picture"
      dimmed: true
    }
    Rectangle {
      x: Style.space(10)
      y: Style.space(10)
      width: tagRow.implicitWidth + Style.space(16)
      height: tagRow.implicitHeight + Style.space(8)
      radius: 0
      color: Qt.alpha(Color.popups.background, 0.88)
      border.width: 1
      border.color: root.tokens.rule
      Row {
        id: tagRow
        anchors.centerIn: parent
        spacing: Style.space(7)
        StateMarker { tokens: root.tokens; fleetState: root.fleetState; anchors.verticalCenter: parent.verticalCenter }
        Copy {
          anchors.verticalCenter: parent.verticalCenter
          text: (root.service ? root.service.turnWords(root.computer.computer_id) : "") || root.tokens.stateLabel(root.fleetState, root.computer)
          color: root.tokens.textTint(root.tokens.stateColor(root.fleetState))
          font.pixelSize: Style.font.bodySmall
        }
      }
    }
  }
  // A person's ring: when someone takes control it closes in once around the picture in the
  // person color; when control is handed back it lets go, opening outward past the picture's
  // edge as it thins and fades. Outside the picture's clip, so the opening shows.
  Rectangle {
    id: controlRing
    readonly property bool person: root.fleetState === "human"
    property real spread: 0
    x: stage.x - spread
    y: stage.y - spread
    width: stage.width + spread * 2
    height: stage.height + spread * 2
    color: "transparent"
    radius: 0
    border.color: root.tokens.stateColor("human")
    border.width: Style.space(4)
    opacity: 0
    function settle() { ringIn.stop(); ringOut.stop(); spread = 0; border.width = Style.space(4); opacity = person ? 1 : 0 }
    Component.onCompleted: settle()
    onPersonChanged: {
      if (person) { ringOut.stop(); spread = 0; opacity = 1; ringIn.restart() }
      else if (opacity > 0) { ringIn.stop(); ringOut.restart() }
    }
    // Another computer in the same place: its ring as it stands, after any change this caused.
    Connections { target: root; function onComputerIdChanged() { Qt.callLater(controlRing.settle) } }
    NumberAnimation { id: ringIn; target: controlRing; property: "border.width"; from: Style.space(40); to: Style.space(4); duration: 450; easing.type: Easing.OutCubic }
    ParallelAnimation {
      id: ringOut
      NumberAnimation { target: controlRing; property: "spread"; from: 0; to: Style.space(18); duration: 700; easing.type: Easing.OutCubic }
      NumberAnimation { target: controlRing; property: "border.width"; to: Style.space(1); duration: 700; easing.type: Easing.OutCubic }
      NumberAnimation { target: controlRing; property: "opacity"; to: 0; duration: 700; easing.type: Easing.InQuad }
    }
    Accessible.ignored: true
  }

  Column {
    id: rail
    x: root.railBeside ? root.width - width : 0
    y: root.railBeside ? 0 : stage.height + root.gap
    width: root.railBeside ? root.railWidth : root.width
    Copy {
      readonly property string reason: root.service && root.service.sessions[root.computerId]
        ? String(root.service.sessions[root.computerId].fallback_reason || "") : ""
      readonly property string engine: root.service && root.service.sessions[root.computerId]
        ? String(root.service.sessions[root.computerId].screen_engine || "") : ""
      visible: engine !== ""
      width: parent.width
      height: visible ? implicitHeight + Style.space(10) : 0
      text: engine === "ibara" ? "Join uses ibara’s stream." :
        (engine === "unavailable" ? "Screen sharing is unavailable." : "Join uses Sunshine.") + (reason ? " " + reason : "")
      color: root.tokens.textTint(root.tokens.attentionColor)
      wrapMode: Text.Wrap
      maximumLineCount: 3
      elide: Text.ElideRight
    }
    Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }

    RailSection {
      id: nowSection
      key: "now"
      title: "Now"
      summary: root.nowSummary
      summaryMarkup: root.nowSummaryMarkup
      body: Component {
        Column {
          spacing: Style.space(4)
          Copy {
            width: parent.width
            text: root.task ? StatusModel.clip(String(root.task.title || "Untitled task"), 240) : root.idleLine
            color: root.task ? root.tokens.foreground : root.tokens.textTint(root.tokens.stateColor(root.fleetState))
            font.bold: !!root.task
            maximumLineCount: 3
            elide: Text.ElideRight
          }
          Copy {
            visible: !!root.task
            width: parent.width
            text: {
              if (!root.task) return ""
              var parts = []
              var who = root.task.principal ? "agent from " + String(root.task.principal) : root.tokens.actor(root.computer)
              if (who) parts.push(root.tokens.ink(who, root.tokens.foreground))
              var started = root.tokens.clockLabel(root.task.started_at)
              if (started) parts.push(root.tokens.ink("started " + started, root.tokens.dim))
              if (root.taskAge) parts.push(root.tokens.ink(root.taskAge, root.tokens.dim))
              return parts.join(root.tokens.ink(" · ", root.tokens.dim))
            }
            textFormat: Text.StyledText
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
          }
          Copy { visible: !!root.task; topPadding: Style.space(4); eyebrow: true; text: "Latest steps" }
          Repeater {
            model: root.steps.slice(0, root.lineCap)
            delegate: Copy {
              required property var modelData
              width: parent.width
              textFormat: Text.StyledText
              text: root.tokens.stepMarkup(modelData, true)
              Accessible.name: root.stepLine(modelData)
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
          }
          Copy {
            visible: !!root.task && root.steps.length === 0
            width: parent.width
            text: root.stepsLoading ? "Checking…" : root.service && root.service.readErrors.task ? StatusModel.clip(root.service.readErrors.task, 160) : "No steps recorded yet"
            color: root.service && root.service.readErrors.task ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.dim
            font.pixelSize: Style.font.bodySmall
          }
          ShowAll { tab: "activity"; tabLabel: "Activity" }
        }
      }
    }

    RailSection {
      id: accessSection
      key: "access"
      title: "Who can use it"
      summary: root.accessRows.length ? StatusModel.peopleAndAgents(root.accessRows) : root.accessStatus
      summaryUrgent: !root.accessRows.length && !!root.service && !!root.service.readErrors.access
      body: Component {
        Column {
          id: whoColumn
          readonly property real marksWidth: Style.space(20) * 5
          readonly property real nameWidth: Math.min(width - marksWidth - Style.space(10), Style.space(200))
          spacing: Style.space(2)
          Repeater {
            model: root.accessRows.slice(0, root.lineCap)
            delegate: Item {
              id: whoRow
              required property var modelData
              width: whoColumn.width
              height: Style.space(20)
              Accessible.role: Accessible.StaticText
              Accessible.name: modelData.label + ": " + modelData.marks.map(function(m) { return m.label + " " + root.ruleWords(m.rule) }).join(", ")
              Copy {
                width: whoColumn.nameWidth
                anchors.verticalCenter: parent.verticalCenter
                text: whoRow.modelData.label
                font.pixelSize: Style.font.bodySmall
                font.bold: whoRow.modelData.you
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
              }
              Row {
                x: whoColumn.nameWidth + Style.space(10)
                anchors.verticalCenter: parent.verticalCenter
                Repeater {
                  model: whoRow.modelData.marks
                  delegate: PermissionMark {
                    required property var modelData
                    mark: modelData
                  }
                }
              }
            }
          }
          Copy {
            visible: root.accessRows.length === 0
            width: parent.width
            text: root.accessStatus
            color: root.service && root.service.readErrors.access ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.dim
            font.pixelSize: Style.font.bodySmall
          }
          Copy {
            visible: root.accessRows.length > root.lineCap
            text: (root.accessRows.length - root.lineCap) + " more"
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Row {
            spacing: Style.space(10)
            ShowAll { tab: "access"; tabLabel: "Access" }
            DataAge {
              anchors.verticalCenter: parent.verticalCenter
              at: root.service && root.service.scopedReadAt.access || 0
              refreshing: !!root.service && root.service.readPending("access")
              nowMs: root.nowMs
            }
          }
        }
      }
    }

    RailSection {
      id: filesSection
      key: "files"
      title: "Recent files"
      summary: root.filesSummary
      summaryUrgent: root.recentFiles.length > 0 && root.recentFiles[0].state === "failed"
      body: Component {
        Column {
          spacing: Style.space(3)
          Repeater {
            model: root.recentFiles.slice(0, root.lineCap)
            delegate: Copy {
              required property var modelData
              width: parent.width
              text: root.fileLine(modelData)
              color: root.tokens.textTint(modelData.state === "failed" ? root.tokens.attentionColor : modelData.state === "verified" ? root.tokens.readyColor : root.tokens.workingColor)
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideMiddle
            }
          }
          Copy {
            visible: root.recentFiles.length > root.lineCap
            text: (root.recentFiles.length - root.lineCap) + " more"
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          ShowAll { tab: "files"; tabLabel: "Files" }
        }
      }
    }

    RailSection {
      id: displaySection
      visible: root.outputs.length > 1
      key: "display"
      title: "Display"
      summary: root.displaySummary
      summaryUrgent: root.choosingDisplay
      body: Component {
        Flow {
          spacing: Style.space(6)
          Repeater {
            model: root.outputs.length > 1 ? root.outputs : []
            delegate: ActionButton {
              required property var modelData
              label: String(modelData.label || modelData.display_id)
              size: "small"
              selected: !!root.computer && root.computer.display_id === modelData.display_id
              onClicked: if (root.service) root.service.setComputerDisplay(root.computerId, String(modelData.display_id))
            }
          }
        }
      }
    }
  }
}
