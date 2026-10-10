import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Tasks, procedures awaiting review and task results on this computer, with the
// selected record's detail and evidence. Reads are scoped to the open computer by Service.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  property string detailKind: "task"
  property bool showTechnicalDetail: false
  property int extendSeconds: 300
  readonly property Tokens tokens: Tokens {}
  readonly property string computerLabel: tokens.label(computer)
  readonly property var tasks: service ? service.tasks || [] : []
  readonly property var procedures: service ? service.procedures || [] : []
  readonly property var artifacts: service ? service.artifacts || [] : []
  readonly property var followed: service && service.followedTasks ? service.followedTasks[computerId] : null
  readonly property var detail: service ? service.selectedTaskDetail || ({}) : ({})
  readonly property var selectedTaskRecord: {
    if (!service || !service.selectedTaskRef) return null
    var extra = service.selectedTaskDetailRef === service.selectedTaskRef ? service.selectedTask || {} : {}
    for (var i = 0; i < tasks.length; i++)
      if (tasks[i].task_ref === service.selectedTaskRef) return Object.assign({}, tasks[i], extra)
    return extra
  }
  readonly property var selectedArtifact: {
    if (!service || !service.selectedArtifactRef) return null
    for (var i = 0; i < artifacts.length; i++) if (artifacts[i].artifact_ref === service.selectedArtifactRef) return artifacts[i]
    return null
  }
  readonly property string actionReason: !service ? "Connecting." : service.denied ? "You don't have access." : service.mutating ? "Wait for the current action to finish." : ""

  function reload() {
    if (!service || !visible || !computerId) return
    service.loadTasks()
    service.loadProcedures()
    service.loadArtifacts()
    if (detailKind === "task" && service.selectedTaskRef && !service.readPending("task")) service.inspectTask(service.selectedTaskRef)
  }
  onVisibleChanged: reload()
  onComputerIdChanged: { detailKind = "task"; showTechnicalDetail = false; reload() }

  Timer {
    interval: root.service ? root.service.openRefreshMs : 5000
    repeat: true
    running: root.visible && root.detailKind === "task" && !!root.service && !!root.service.selectedTaskRef && StatusModel.taskLive(root.selectedTaskRecord)
    onTriggered: if (!root.service.readPending("task")) root.service.inspectTask(root.service.selectedTaskRef)
  }

  // Wording lives in StatusModel; this surface owns colors and open folds.
  function taskStateColor(task) { return tokens.stateColor(StatusModel.taskTone(task)) }
  function stepColor(step) {
    return step.tone === "attention" ? tokens.attentionColor : step.tone === "paused" ? tokens.pausedColor
      : step.kind === "task" ? tokens.humanColor : step.kind === "exec" ? tokens.workingColor : tokens.accent
  }
  property var openFolds: ({})
  property bool summaryOpen: false
  property bool timelineReady: false
  property string lastNewestStepKey: ""
  ListModel { id: timelineModel }
  readonly property string foldTaskRef: computerId + ":" + (service ? service.selectedTaskRef : "")
  onFoldTaskRefChanged: {
    openFolds = ({}); summaryOpen = false; lastNewestStepKey = ""
    if (timelineReady) { timelineModel.clear(); detailScroll.contentY = 0 }
  }
  function foldOpen(group) { return group.items.some(function(s) { return !!root.openFolds[s.key] }) }
  function toggleFold(group) {
    var next = Object.assign({}, openFolds)
    var opened = !foldOpen(group)
    group.items.forEach(function(s) { next[s.key] = opened })
    openFolds = next
  }
  readonly property var timeline: StatusModel.taskRuns(detail, StatusModel.taskLive(selectedTaskRecord))
  onTimelineChanged: if (timelineReady) syncTimeline()
  Component.onCompleted: { timelineReady = true; syncTimeline() }
  function syncTimeline() {
    var newest = timeline.length ? timeline[timeline.length - 1].items.slice(-1)[0].key : ""
    // Measure before changing delegates. First open always leaves the outcome in view.
    var bottom = timelineRows.mapToItem(detailColumn, 0, timelineRows.height).y
    var follow = !!lastNewestStepKey && newest !== lastNewestStepKey &&
      detailKind === "task" && StatusModel.taskLive(selectedTaskRecord) &&
      Math.abs(detailScroll.contentY + detailScroll.height - bottom) <= Style.space(64)
    var nextFolds = {}
    for (var i = 0; i < timeline.length; i++) {
      var group = timeline[i], keys = {}
      group.items.forEach(function(s) { keys[s.key] = true })
      // Match overlapping receipts, including a fold whose oldest receipt slid out.
      var found = -1
      for (var j = i; j < timelineModel.count; j++) {
        var old = JSON.parse(timelineModel.get(j).groupJson)
        if (old.kind === group.kind && old.items.some(function(s) { return !!keys[s.key] })) { found = j; break }
      }
      var words = JSON.stringify(group)
      if (found < 0) timelineModel.insert(i, { groupJson: words })
      else {
        if (found !== i) timelineModel.move(found, i, 1)
        if (timelineModel.get(i).groupJson !== words) timelineModel.setProperty(i, "groupJson", words)
      }
      // New receipts inherit an opened fold, even after every original key expires.
      if (foldOpen(group)) group.items.forEach(function(s) { nextFolds[s.key] = true })
    }
    if (timelineModel.count > timeline.length) timelineModel.remove(timeline.length, timelineModel.count - timeline.length)
    openFolds = nextFolds
    lastNewestStepKey = newest
    if (follow) Qt.callLater(function() {
      var end = timelineRows.mapToItem(detailColumn, 0, timelineRows.height).y
      detailScroll.contentY = Math.max(0, Math.min(end - detailScroll.height, detailScroll.contentHeight - detailScroll.height))
    })
  }
  function procedureTitle(record) {
    return String((record && record.definition && record.definition.title) || "Untitled procedure")
  }
  // The version being approved: the digest the review showed.
  function procedureDigest(record) {
    return record ? String(record.source_sha256 || record.content_sha256 || "") : ""
  }
  function fileState(item) {
    if (!item) return "Unknown"
    if (item.bytes_available === false || item.delivery === "unavailable") return "Unavailable"
    if (item.delivery === "verified") return "Collected"
    if (item.bytes_available === true) return "Available"
    return "Availability unknown"
  }

  // ---- actions that change something go through the console's confirmation
  function selectTask(ref) { detailKind = "task"; showTechnicalDetail = false; service.inspectTask(ref) }
  function selectProcedure(ref) { detailKind = "procedure"; service.inspectProcedure(ref) }
  function selectArtifact(ref) { detailKind = "file"; service.selectedArtifactRef = ref }
  function confirmTask(message, label, danger, run) {
    var ref = String(service.selectedTaskRef || ""), scope = computerId
    if (!ref || !host) return
    host.askConfirm({ message: message, confirmLabel: label, danger: danger,
      run: function() { run(ref) },
      valid: function() { return root.service.selectedTaskRef === ref && String(root.service.scopedComputerId || "") === scope } })
  }
  function confirmProcedure(message, label, run) {
    var ref = String(service.selectedProcedureRef || ""), scope = computerId
    var digest = procedureDigest(service.selectedProcedure)
    if (!ref || !host) return
    host.askConfirm({ message: message.replace("%DIGEST%", digest.substring(0, 12)), confirmLabel: label,
      run: function() { run(ref, digest) },
      valid: function() {
        return root.service.selectedProcedureRef === ref && String(root.service.scopedComputerId || "") === scope &&
          (!digest || root.procedureDigest(root.service.selectedProcedure) === digest)
      } })
  }

  component RecordRow: Rectangle {
    id: recordRow
    property string title: ""
    property string meta: ""
    property color markerColor: Color.accent
    property bool current: false
    property int row: -1
    readonly property RowWindow rows: ListView.view ? ListView.view.parent as RowWindow : null
    signal picked()
    width: parent ? parent.width : 0
    height: rows ? rows.rowHeight : 0
    radius: 0
    color: current ? Qt.alpha(Color.accent, 0.14) : rowHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
    border.width: activeFocus ? Math.max(2, Style.space(2)) : current ? 1 : 0
    border.color: activeFocus ? Color.accent : Qt.alpha(Color.accent, 0.6)
    activeFocusOnTab: true
    Accessible.role: Accessible.Button
    Accessible.name: title + ", " + meta
    Keys.onReturnPressed: picked()
    Keys.onEnterPressed: picked()
    Keys.onSpacePressed: picked()
    Keys.onTabPressed: function(event) { event.accepted = recordRow.rows.step(recordRow.row, recordRow, event) }
    Keys.onBacktabPressed: function(event) { event.accepted = recordRow.rows.step(recordRow.row, recordRow, event) }
    onActiveFocusChanged: if (activeFocus) rows.focused(row, recordRow)
    HoverHandler { id: rowHover }
    MouseArea { anchors.fill: parent; onClicked: { recordRow.forceActiveFocus(); recordRow.picked() } }
    Rectangle { x: Style.space(10); anchors.verticalCenter: parent.verticalCenter; width: Style.space(8); height: width; radius: 0; color: recordRow.markerColor }
    Copy {
      id: rowTitle
      x: Style.space(28)
      y: Style.space(7)
      width: parent.width - x - Style.space(10)
      text: recordRow.title
      wrapMode: Text.NoWrap
      elide: Text.ElideRight
    }
    Copy {
      x: Style.space(28)
      anchors.top: rowTitle.bottom
      anchors.topMargin: Style.space(2)
      width: parent.width - x - Style.space(10)
      textFormat: Text.StyledText
      text: root.tokens.ink(recordRow.meta.split(" · ")[0], root.tokens.textTint(recordRow.markerColor)) + root.tokens.ink(recordRow.meta.indexOf(" · ") >= 0 ? recordRow.meta.slice(recordRow.meta.indexOf(" · ")) : "", root.tokens.dim)
      font.pixelSize: Style.font.bodySmall
      wrapMode: Text.NoWrap
      elide: Text.ElideRight
    }
  }

  component RecordList: RowWindow {
    flick: listScroll
    width: listColumn.width
    rowHeight: Style.space(48)
    spacing: Style.space(6)
  }

  component TimelineStep: Item {
    id: stepRow
    property var step: ({})
    implicitHeight: Math.max(stepText.implicitHeight, timeText.implicitHeight) + Style.space(8)
    Rectangle {
      x: Style.space(2); y: Style.space(8)
      width: Style.space(9); height: width
      color: root.stepColor(stepRow.step)
      Accessible.ignored: true
    }
    Copy {
      id: timeText
      x: Style.space(22); y: Style.space(4); width: Style.space(74)
      text: StatusModel.taskTime(stepRow.step.at)
      color: root.tokens.faint
      font.pixelSize: Style.font.bodySmall
    }
    Column {
      id: stepText
      x: Style.space(104); y: Style.space(4); width: parent.width - x
      spacing: Style.space(3)
      Flow {
        width: parent.width
        spacing: Style.space(6)
        Copy {
          text: stepRow.step.verb
          color: root.tokens.textTint(stepRow.step.kind === "task" ? root.tokens.humanColor : stepRow.step.kind === "exec" ? root.tokens.workingColor : root.tokens.accent)
          font.pixelSize: Style.font.caption
          font.capitalization: Font.AllUppercase
        }
        Copy {
          width: Math.min(implicitWidth, parent.width)
          text: stepRow.step.what
          wrapMode: Text.WrapAnywhere
        }
        Copy {
          visible: stepRow.step.tone !== "ready"
          width: Math.min(implicitWidth, parent.width)
          text: stepRow.step.result
          color: root.tokens.textTint(root.tokens.stateColor(stepRow.step.tone))
          font.pixelSize: Style.font.bodySmall
        }
      }
      Copy {
        visible: stepRow.step.error !== ""
        width: parent.width; text: stepRow.step.error
        color: root.tokens.textTint(root.tokens.attentionColor)
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WrapAnywhere
      }
      Copy {
        visible: stepRow.step.expect !== "" && stepRow.step.tone !== "ready"
        width: parent.width; text: "Expected: " + stepRow.step.expect
        color: stepRow.step.tone === "paused" ? root.tokens.textTint(root.tokens.pausedColor) : root.tokens.dim
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WrapAnywhere
      }
    }
  }

  component TimelineFold: Rectangle {
    id: foldLine
    property var group: ({})
    property bool opened: false
    signal toggled()
    implicitHeight: Math.max(foldText.implicitHeight, foldTime.implicitHeight) + Style.space(12)
    color: activeFocus ? root.tokens.raised : "transparent"
    border.width: activeFocus ? 1 : 0
    border.color: root.tokens.accent
    activeFocusOnTab: true
    Accessible.role: Accessible.Button
    Accessible.name: group.summary + (opened ? ", fold them" : ", show them")
    Accessible.focusable: true
    Accessible.onPressAction: toggled()
    Keys.onReturnPressed: toggled()
    Keys.onEnterPressed: toggled()
    Keys.onSpacePressed: toggled()
    onActiveFocusChanged: if (activeFocus) {
      var point = mapToItem(detailColumn, 0, 0)
      if (point.y < detailScroll.contentY) detailScroll.contentY = point.y
      else if (point.y + height > detailScroll.contentY + detailScroll.height) detailScroll.contentY = point.y + height - detailScroll.height
    }
    Rectangle {
      x: Style.space(2); y: Style.space(10); width: Style.space(9); height: width
      color: root.stepColor(group.items[0])
      Accessible.ignored: true
    }
    Copy {
      id: foldTime
      x: Style.space(22); y: Style.space(6); width: Style.space(74)
      text: StatusModel.taskTime(foldLine.group.items[0].at)
      color: root.tokens.faint
      font.pixelSize: Style.font.bodySmall
    }
    Copy {
      id: foldText
      x: Style.space(104); y: Style.space(6); width: parent.width - x - Style.space(4)
      text: foldLine.opened ? "▾ Fold " + foldLine.group.items.length + " Steps" : foldLine.group.summary + " ▸ Show"
      color: root.tokens.textTint(foldLine.group.items.some(function(s) { return s.tone === "paused" }) ? root.tokens.pausedColor : root.stepColor(foldLine.group.items[0]))
    }
    MouseArea {
      anchors.fill: parent
      cursorShape: Qt.PointingHandCursor
      onClicked: { foldLine.forceActiveFocus(); foldLine.toggled() }
    }
  }

  // ---- left: follow, tasks, procedures, results
  Flickable {
    id: listScroll
    anchors.left: parent.left
    anchors.top: parent.top
    anchors.bottom: parent.bottom
    width: Math.min(Style.space(440), parent.width * 0.42)
    clip: true
    contentWidth: width
    contentHeight: listColumn.implicitHeight
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Column {
      id: listColumn
      width: listScroll.width - Style.space(8)
      spacing: Style.space(6)
      Row {
        spacing: Style.space(10)
        ActionButton {
          label: root.followed ? "Stop Following" : "Follow Current Task"
          selected: !!root.followed
          blocked: !root.followed && !(root.computer && root.computer.active_task_ref)
          disabledReason: blocked ? "Nothing is running on " + root.computerLabel + " to follow." : ""
          onClicked: {
            if (blocked || !root.service) return
            if (root.followed) root.service.unfollowTask(root.computerId)
            else root.service.followTask(root.computerId)
          }
        }
        ActionButton {
          label: "Refresh"
          onClicked: root.reload()
        }
      }
      DataAge {
        at: root.service && root.service.scopedReadAt.tasks || 0
        refreshing: !!root.service && (root.service.readPending("tasks") || root.service.readPending("artifacts") || root.service.readPending("procedures"))
        nowMs: root.service ? root.service.nowMs : Date.now()
      }
      Copy {
        visible: !!root.followed
        width: parent.width
        text: "You'll get a notification when " + root.computerLabel + "'s current task is checked as finished."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
      Item { width: 1; height: Style.space(6) }

      Copy { eyebrow: true; text: "Tasks" + (root.service && root.service.tasksLoaded ? " · " + root.tasks.length : "") }
      RecordList {
        model: root.tasks
        delegate: RecordRow {
          width: listColumn.width
          row: index
          title: StatusModel.clip(StatusModel.taskTitle(modelData), 90)
          meta: StatusModel.taskState(modelData) + (StatusModel.taskAgent(modelData) ? " · " + StatusModel.taskAgent(modelData) : "")
          markerColor: root.taskStateColor(modelData)
          current: root.detailKind === "task" && !!root.service && root.service.selectedTaskRef === modelData.task_ref
          onPicked: root.selectTask(modelData.task_ref)
        }
      }
      Copy {
        visible: root.tasks.length === 0
        width: parent.width
        text: root.service && root.service.readPending("tasks") ? "Loading…" : root.service && root.service.readErrors.tasks ? StatusModel.clip(root.service.readErrors.tasks, 200) : "No tasks yet."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
      Item { width: 1; height: Style.space(8) }

      Copy { eyebrow: true; text: "Procedures awaiting review" }
      RecordList {
        model: root.procedures
        delegate: RecordRow {
          width: listColumn.width
          row: index
          title: StatusModel.clip(root.procedureTitle(modelData), 90)
          meta: root.tokens.sentence(modelData.status) || "Candidate"
          markerColor: root.tokens.pausedColor
          current: root.detailKind === "procedure" && !!root.service && root.service.selectedProcedureRef === modelData.procedure_ref
          onPicked: root.selectProcedure(modelData.procedure_ref)
        }
      }
      Copy {
        visible: root.procedures.length === 0
        width: parent.width
        text: root.service && root.service.readPending("procedures") ? "Loading…" : root.service && root.service.readErrors.procedures ? StatusModel.clip(root.service.readErrors.procedures, 200) : "Nothing to review."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
      Item { width: 1; height: Style.space(8) }

      Copy { eyebrow: true; text: "Task results" }
      RecordList {
        model: root.artifacts
        delegate: RecordRow {
          width: listColumn.width
          row: index
          title: StatusModel.clip(String(modelData.name || "Unnamed file"), 90)
          meta: root.fileState(modelData) + " · " + StatusModel.bytesLabel(modelData.size_bytes)
          markerColor: root.tokens.readyColor
          current: root.detailKind === "file" && !!root.service && root.service.selectedArtifactRef === modelData.artifact_ref
          onPicked: root.selectArtifact(modelData.artifact_ref)
        }
      }
      Copy {
        visible: root.artifacts.length === 0
        width: parent.width
        text: root.service && root.service.readPending("artifacts") ? "Loading…" : root.service && root.service.readErrors.artifacts ? StatusModel.clip(root.service.readErrors.artifacts, 200) : "No results yet."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
    }
  }

  // ---- right: the selected record
  Rectangle {
    anchors.left: listScroll.right
    anchors.leftMargin: Style.space(20)
    anchors.right: parent.right
    anchors.top: parent.top
    anchors.bottom: parent.bottom
    radius: 0
    color: root.tokens.surface
    border.width: 1
    border.color: root.tokens.rule
    Flickable {
      id: detailScroll
      anchors.fill: parent
      anchors.margins: Style.space(18)
      clip: true
      contentWidth: width
      contentHeight: detailColumn.implicitHeight
      boundsBehavior: Flickable.StopAtBounds
      flickableDirection: Flickable.VerticalFlick
      Column {
        id: detailColumn
        width: detailScroll.width
        spacing: Style.space(10)

        // Task
        Column {
          id: taskColumn
          visible: root.detailKind === "task"
          width: parent.width
          spacing: Style.space(10)
          readonly property bool hasTask: !!(root.service && root.service.selectedTaskRef)
          readonly property var checks: StatusModel.taskChecks(root.detail)
          readonly property var criteria: hasTask ? StatusModel.taskCriteria(root.detail) : []
          readonly property color outcomeColor: root.taskStateColor(root.selectedTaskRecord)
          Rectangle {
            visible: taskColumn.hasTask
            width: parent.width
            implicitHeight: bandText.implicitHeight + Style.space(24)
            color: Qt.alpha(taskColumn.outcomeColor, 0.12)
            Rectangle { width: Style.space(4); height: parent.height; color: taskColumn.outcomeColor }
            Column {
              id: bandText
              x: Style.space(14); y: Style.space(12); width: parent.width - x - Style.space(14)
              spacing: Style.space(4)
              Copy {
                width: parent.width
                text: StatusModel.taskState(root.selectedTaskRecord) + " · " + taskColumn.checks.met + " of " + taskColumn.checks.total + " checks met"
                color: root.tokens.textTint(taskColumn.outcomeColor)
                font.pixelSize: Style.font.heading
                font.bold: true
              }
              Copy {
                width: parent.width
                text: StatusModel.taskBandDetail(root.detail, root.selectedTaskRecord)
                dimmed: true
                font.pixelSize: Style.font.bodySmall
              }
            }
          }
          Copy {
            width: parent.width
            text: taskColumn.hasTask ? String((root.selectedTaskRecord || {}).goal || "Untitled task") : "Select a task"
            font.pixelSize: Style.font.heading + Style.space(2)
            font.bold: true
          }
          Rectangle {
            visible: taskColumn.hasTask && !!(root.selectedTaskRecord || {}).completion_summary
            width: parent.width
            implicitHeight: summaryText.implicitHeight + Style.space(16)
            color: Qt.alpha(root.tokens.accent, 0.05)
            Rectangle { width: Style.space(3); height: parent.height; color: root.tokens.accent }
            Column {
              id: summaryText
              x: Style.space(12); y: Style.space(8); width: parent.width - x - Style.space(12)
              spacing: Style.space(3)
              Copy { text: "The agent's summary"; dimmed: true; font.pixelSize: Style.font.caption }
              Copy {
                id: summaryCopy
                width: parent.width
                text: StatusModel.taskPlain((root.selectedTaskRecord || {}).completion_summary)
                maximumLineCount: root.summaryOpen ? 2147483647 : 3
                elide: Text.ElideRight
              }
              ActionButton {
                visible: root.summaryOpen || summaryCopy.truncated
                label: root.summaryOpen ? "Show Less" : "Show All"
                role: "quiet"; size: "small"
                onClicked: root.summaryOpen = !root.summaryOpen
              }
            }
          }
          Copy {
            visible: taskColumn.hasTask && !!(root.service && root.service.selectedTaskDetailObservedAt)
            width: parent.width
            text: root.service ? "Updated " + StatusModel.ageLabel(root.service.selectedTaskDetailObservedAt, root.service.nowMs) : ""
            dimmed: true
            font.pixelSize: Style.font.caption
          }
          Copy { visible: taskColumn.hasTask && !!(root.service && root.service.selectedTaskLoading && root.service.selectedTaskDetailRef !== root.service.selectedTaskRef); text: "Updating task details…"; dimmed: true; font.pixelSize: Style.font.bodySmall }
          Copy {
            visible: !!(root.service && root.service.readErrors.task)
            width: parent.width
            text: root.service ? StatusModel.clip(root.service.readErrors.task, 200) : ""
            color: root.tokens.textTint(root.tokens.attentionColor)
            font.pixelSize: Style.font.bodySmall
          }
          Copy { visible: taskColumn.hasTask && StatusModel.taskWarning(root.detail) !== ""; width: parent.width; text: StatusModel.taskWarning(root.detail); color: root.tokens.textTint(root.tokens.attentionColor); font.pixelSize: Style.font.bodySmall }
          Flow {
            visible: parent.hasTask
            width: parent.width
            spacing: Style.space(6)
            ActionButton {
              label: root.showTechnicalDetail ? "Hide Technical Details" : "Technical Details"
              selected: root.showTechnicalDetail
              onClicked: {
                root.showTechnicalDetail = !root.showTechnicalDetail
                if (root.service.selectedTaskDetailRef !== root.service.selectedTaskRef) root.service.inspectTask(root.service.selectedTaskRef)
              }
            }
            ActionButton {
              label: "Add 5 Minutes"
              blocked: root.actionReason !== ""
              disabledReason: root.actionReason
              onClicked: if (!blocked) root.service.extendTask(root.service.selectedTaskRef, root.extendSeconds)
            }
            ActionButton {
              label: "End Task"
              role: "danger"
              blocked: root.actionReason !== ""
              disabledReason: root.actionReason
              onClicked: if (!blocked) root.confirmTask("End this task on " + root.computerLabel + "? Other agents keep working.", "End Task", true,
                function(ref) { root.service.revokeTask(ref) })
            }
          }
          Copy {
            visible: taskColumn.hasTask && !(root.service && root.service.selectedTaskLoading) && taskColumn.criteria.length === 0 && root.timeline.length === 0 && StatusModel.taskDeliveries(root.detail).length === 0
            width: parent.width
            text: "No detailed evidence is recorded for this task."
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Copy {
            visible: taskColumn.criteria.length > 0
            eyebrow: true
            text: "Checks · " + taskColumn.checks.met + " of " + taskColumn.checks.total + " met"
            color: root.tokens.textTint(taskColumn.checks.met === taskColumn.checks.total ? root.tokens.readyColor : root.tokens.pausedColor)
          }
          Row {
            visible: taskColumn.criteria.length > 0
            width: parent.width
            height: Style.space(6)
            spacing: Style.space(2)
            Repeater {
              model: taskColumn.criteria
              delegate: Rectangle {
                width: (taskColumn.width - (taskColumn.criteria.length - 1) * Style.space(2)) / taskColumn.criteria.length
                height: Style.space(6)
                color: modelData.state === "satisfied" ? root.tokens.readyColor : root.tokens.pausedColor
                Accessible.ignored: true
              }
            }
          }
          Repeater {
            model: taskColumn.criteria
            delegate: Row {
              width: detailColumn.width
              spacing: Style.space(6)
              property var criterion: modelData
              Copy {
                width: Style.space(18)
                text: parent.criterion.state === "satisfied" ? "✓" : "○"
                color: root.tokens.textTint(parent.criterion.state === "satisfied" ? root.tokens.readyColor : root.tokens.pausedColor)
                font.bold: true
              }
              Column {
                width: parent.width - Style.space(24)
                spacing: Style.space(3)
                Copy { width: parent.width; text: String(criterion.description || criterion.id || criterion.criterion_id || "Check"); font.bold: true }
                Copy {
                  width: parent.width
                  text: (criterion.state === "satisfied" ? "Met" : "Not met yet") + (criterion.required === false ? " · optional" : " · required")
                  color: root.tokens.textTint(criterion.state === "satisfied" ? root.tokens.readyColor : root.tokens.pausedColor)
                  font.pixelSize: Style.font.bodySmall
                }
                Copy { visible: text !== ""; width: parent.width; text: StatusModel.taskClaim(criterion); dimmed: true; font.pixelSize: Style.font.bodySmall }
                Repeater {
                  model: Array.isArray(criterion.evidence) ? criterion.evidence : []
                  delegate: Copy { width: parent.width; text: StatusModel.taskPlain(modelData.summary || modelData.outcome || "Evidence available"); font.pixelSize: Style.font.bodySmall }
                }
              }
            }
          }
          Copy { visible: taskColumn.hasTask && root.timeline.length > 0; eyebrow: true; text: "What it did · oldest first" }
          Copy {
            visible: taskColumn.hasTask && !!(root.detail && (root.detail.receipt_next_cursor || root.detail.more))
            width: parent.width
            text: "Earlier steps aren't loaded."
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Item {
            visible: taskColumn.hasTask && root.timeline.length > 0
            width: parent.width
            implicitHeight: timelineRows.implicitHeight
            Rectangle {
              x: Style.space(6); y: Style.space(8); width: 1; height: Math.max(0, parent.height - Style.space(16))
              color: root.tokens.rule
              Accessible.ignored: true
            }
            Column {
              id: timelineRows
              width: parent.width
              Repeater {
                model: taskColumn.hasTask ? timelineModel : null
                delegate: Column {
                  id: runColumn
                  required property string groupJson
                  width: timelineRows.width
                  property var group: JSON.parse(groupJson)
                  readonly property bool opened: root.foldOpen(group)
                  TimelineFold {
                    visible: runColumn.group.fold
                    width: parent.width
                    group: runColumn.group
                    opened: runColumn.opened
                    onToggled: root.toggleFold(runColumn.group)
                  }
                  Repeater {
                    model: !runColumn.group.fold || runColumn.opened ? runColumn.group.items : []
                    delegate: TimelineStep { width: runColumn.width; step: modelData }
                  }
                }
              }
            }
          }
          Copy { visible: taskColumn.hasTask && StatusModel.taskDeliveries(root.detail).length > 0; eyebrow: true; text: "Required Results" }
          Repeater {
            model: taskColumn.hasTask ? StatusModel.taskDeliveries(root.detail) : []
            delegate: Column {
              width: detailColumn.width
              spacing: Style.space(3)
              property var delivery: modelData
              Copy { width: parent.width; text: String(delivery.host_id || delivery.destination_host || delivery.host || "Unknown computer") + " · " + String(delivery.destination_path || delivery.path || "Destination unavailable"); font.bold: true }
              Copy { width: parent.width; text: root.tokens.sentence(delivery.state) || "Pending"; color: root.tokens.textTint(root.tokens.stateColor(StatusModel.taskDeliveryTone(delivery))); font.pixelSize: Style.font.bodySmall }
            }
          }
          Copy {
            visible: taskColumn.hasTask && Object.keys(StatusModel.taskProjection(root.detail)).length > 0
            width: parent.width
            text: "Cleanup · " + StatusModel.taskCleanup(root.detail)
            font.pixelSize: Style.font.bodySmall
          }
          Repeater {
            model: parent.hasTask && root.showTechnicalDetail && root.service && root.service.selectedTaskDetail ? StatusModel.evidenceLines(root.service.selectedTaskDetail) : []
            delegate: Copy { width: detailColumn.width; text: String(modelData); dimmed: true; font.pixelSize: Style.font.caption; wrapMode: Text.WrapAnywhere }
          }
        }

        // Procedure
        Column {
          visible: root.detailKind === "procedure"
          width: parent.width
          spacing: Style.space(10)
          readonly property var record: root.service ? root.service.selectedProcedure : null
          Copy {
            width: parent.width
            text: parent.record ? root.procedureTitle(parent.record) : root.service && root.service.selectedProcedureLoading ? "Loading procedure…" : "Select a procedure"
            font.pixelSize: Style.font.heading + Style.space(2)
            font.bold: true
          }
          // Status, revision, applicability, evidence warnings, then the steps: one shared wording.
          Repeater {
            model: parent.record ? StatusModel.procedureLines(parent.record).slice(1) : []
            delegate: Copy {
              required property var modelData
              required property int index
              readonly property string line: String(modelData)
              readonly property string value: line.indexOf(": ") >= 0 ? line.slice(line.indexOf(": ") + 2) : ""
              width: detailColumn.width
              textFormat: Text.StyledText
              text: root.tokens.labeled(line, index === 1 ? root.tokens.faint : root.tokens.textTint(root.tokens.statusColor(value)))
              font.pixelSize: Style.font.bodySmall
            }
          }
          Copy {
            visible: !!(root.service && root.service.readErrors.procedure)
            width: parent.width
            text: root.service ? StatusModel.clip(root.service.readErrors.procedure, 200) : ""
            color: root.tokens.textTint(root.tokens.attentionColor)
            font.pixelSize: Style.font.bodySmall
          }
          Row {
            visible: !!(root.service && root.service.selectedProcedureRef)
            spacing: Style.space(6)
            ActionButton {
              readonly property bool reviewed: !!(parent.parent.record && parent.parent.record.procedure_ref === root.service.selectedProcedureRef && root.procedureDigest(parent.parent.record))
              label: "Approve This Version"
              blocked: root.actionReason !== "" || !reviewed
              disabledReason: root.actionReason || (reviewed ? "" : root.service && root.service.selectedProcedureLoading ? "ibara is still loading this procedure." : "This procedure has no version to approve.")
              onClicked: if (!blocked) root.confirmProcedure("Approve version %DIGEST% of this procedure on " + root.computerLabel + "?", "Approve",
                function(ref, digest) { root.service.approveProcedure(ref, digest) })
            }
            ActionButton {
              label: "Withdraw"
              role: "danger"
              blocked: root.actionReason !== ""
              disabledReason: root.actionReason
              onClicked: if (!blocked) root.confirmProcedure("Withdraw this procedure on " + root.computerLabel + " from approved results?", "Withdraw",
                function(ref) { root.service.quarantineProcedure(ref) })
            }
          }
        }

        // Task result
        Column {
          visible: root.detailKind === "file"
          width: parent.width
          spacing: Style.space(10)
          Copy {
            width: parent.width
            text: root.selectedArtifact ? String(root.selectedArtifact.name || "Unnamed file") : "Select a result"
            font.pixelSize: Style.font.heading + Style.space(2)
            font.bold: true
          }
          Copy {
            visible: !!root.selectedArtifact
            width: parent.width
            text: root.selectedArtifact ? root.fileState(root.selectedArtifact) + " · " + StatusModel.bytesLabel(root.selectedArtifact.size_bytes) : ""
          }
          Copy {
            visible: !!root.selectedArtifact
            width: parent.width
            text: "A saved copy may not satisfy the task's delivery check."
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          ActionButton {
            visible: !!root.selectedArtifact
            label: "Collect File"
            blocked: root.actionReason !== ""
            disabledReason: root.actionReason
            onClicked: if (!blocked) root.service.fetchArtifact(root.service.selectedArtifactRef, root.selectedArtifact.name)
          }
        }
      }
    }
  }
}
