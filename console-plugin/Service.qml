import QtQuick
import Quickshell
import Quickshell.Io
import "StatusModel.js" as StatusModel
import "ServiceBridge.js" as ServiceBridge

Item {
  id: root
  readonly property string notificationTraceCandidate: "follow-notify-attribution-20260924-r1"
  readonly property string notificationTraceRootId: Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 14)
  property double notificationTraceRootOrdinal: 0
  property string omarchyPath: ""
  property var shell: null
  readonly property bool standalone: !!(shell && shell.standalone)
  property var manifest: null
  property var settings: ({})
  property var computers: []
  property var sessions: ({})
  property var followedTasks: ({})
  function notificationTracePrefix(event) {
    return "[ibara-follow-trace] candidate=" + notificationTraceCandidate + "-root-" + notificationTraceRootOrdinal +
      " root=" + notificationTraceRootId + " event=" + event
  }
  function notificationTraceToken(value) {
    var text = String(value || "")
    return /^[A-Za-z0-9_.:-]{1,128}$/.test(text) ? text : "invalid"
  }
  function notificationTraceState(value) {
    return ["completed", "partial", "cancelled", "blocked", "unverified"].indexOf(value) !== -1 ? value : "invalid"
  }
  function notificationTraceNotice(value) {
    return value === "verified-completion" ? value : "invalid"
  }
  function logDroppedFollowNotice(notice, reason) {
    if (!notice || !notice.trace) return
    var allowedReasons = ["target-binding-or-policy", "target-denied", "target-epoch-changed", "protected-views-cleared"]
    var safeReason = allowedReasons.indexOf(reason) !== -1 ? reason : "unknown"
    console.info(root.notificationTracePrefix("notice-dropped") +
      " computer=" + root.notificationTraceToken(notice.trace.computerId) +
      " task_ref=" + root.notificationTraceToken(notice.trace.taskRef) +
      " state=" + root.notificationTraceState(notice.trace.state) +
      " verified=" + (notice.trace.verified === true ? "true" : "false") +
      " notice=" + root.notificationTraceNotice(notice.trace.notice) + " reason=" + safeReason)
  }
  function followTask(computerId) {
    var session = sessions[String(computerId || "")]
    var ref = session ? String(session.active_task_ref || "") : ""
    if (!session || session.trust_state !== "verified" || !session.controller_epoch ||
        !/^[A-Za-z0-9_.:-]{1,128}$/.test(ref) || denied) return false
    var next = Object.assign({}, followedTasks)
    next[computerId] = { computerId: computerId, taskRef: ref, endpointId: session.endpoint_id,
      bindingRevision: session.binding_revision, authorizationGeneration: session.authorization_generation,
      epoch: session.controller_epoch }
    followedTasks = next
    pollFollowedTasks()
    return true
  }
  function unfollowTask(computerId) {
    var next = Object.assign({}, followedTasks)
    delete next[computerId]
    followedTasks = next
  }
  function sameFollowIdentity(follow, snapshot) {
    return !!follow && !!snapshot && follow.computerId === snapshot.computerId &&
      follow.taskRef === snapshot.taskRef && follow.endpointId === snapshot.endpointId &&
      follow.bindingRevision === snapshot.bindingRevision &&
      follow.authorizationGeneration === snapshot.authorizationGeneration && follow.epoch === snapshot.epoch
  }
  function sameTargetIdentity(target, snapshot) {
    return !!target && !!snapshot && target.computerId === snapshot.computerId &&
      target.endpointId === snapshot.endpointId && target.bindingRevision === snapshot.bindingRevision &&
      target.authorizationGeneration === snapshot.authorizationGeneration && target.epoch === snapshot.epoch
  }
  function sessionStatusSnapshot(computerId) {
    var id = String(computerId || ""), session = sessions[id], follow = followedTasks[id]
    if (!session) return null
    return {
      computerId: id, endpointId: session.endpoint_id, bindingRevision: session.binding_revision,
      authorizationGeneration: session.authorization_generation, epoch: session.controller_epoch,
      followSnapshot: follow ? Object.assign({}, follow) : null
    }
  }
  function sameSessionStatusRequest(request) {
    var snapshot = request.sessionSnapshot, current = sessions[request.ref], follow = followedTasks[request.ref]
    if (!snapshot || !current || snapshot.computerId !== request.ref || current.computer_id !== request.ref ||
        current.trust_state !== "verified" || current.endpoint_id !== snapshot.endpointId ||
        current.binding_revision !== snapshot.bindingRevision ||
        current.authorization_generation !== snapshot.authorizationGeneration || current.controller_epoch !== snapshot.epoch)
      return false
    return snapshot.followSnapshot ? sameFollowIdentity(follow, snapshot.followSnapshot) : !follow
  }
  function sameFollowStatusRequest(request) {
    var snapshot = request.followSnapshot, current = sessions[request.ref]
    return sameFollowIdentity(followedTasks[request.ref], snapshot) && snapshot.computerId === request.ref && !!current &&
      current.computer_id === request.ref && current.trust_state === "verified" &&
      current.endpoint_id === snapshot.endpointId && current.binding_revision === snapshot.bindingRevision &&
      current.authorization_generation === snapshot.authorizationGeneration && current.controller_epoch === snapshot.epoch
  }
  function clearDeniedTarget(snapshot, followSnapshot) {
    if (!snapshot) return
    if (followSnapshot && sameFollowIdentity(followedTasks[snapshot.computerId], followSnapshot))
      unfollowTask(snapshot.computerId)
    for (var i = 0; i < pendingNotices.length; i++)
      if (sameTargetIdentity(pendingNotices[i], snapshot)) logDroppedFollowNotice(pendingNotices[i], "target-denied")
    pendingNotices = pendingNotices.filter(function(notice) { return !sameTargetIdentity(notice, snapshot) })
  }
  function pollFollowedTasks() {
    for (var id in followedTasks) {
      var follow = followedTasks[id], session = sessions[id]
      if (!session || session.trust_state !== "verified" || session.endpoint_id !== follow.endpointId ||
          session.binding_revision !== follow.bindingRevision || session.authorization_generation !== follow.authorizationGeneration ||
          session.controller_epoch !== follow.epoch) { unfollowTask(id); continue }
      var kind = "follow-task:" + id
      if (!readPending(kind)) requestRead(kind, ["operator-task-status", "--computer", id, "--epoch", follow.epoch, "--task", follow.taskRef], id, follow)
    }
  }
  function clearTargetEpoch(computerId, oldEpoch) {
    unfollowTask(computerId)
    for (var i = 0; i < pendingNotices.length; i++)
      if (pendingNotices[i].computerId === computerId) logDroppedFollowNotice(pendingNotices[i], "target-epoch-changed")
    pendingNotices = pendingNotices.filter(function(notice) { return notice.computerId !== computerId })
    var pin = humanFileTransfer
    if (pin && pin.computerId === computerId && pin.epoch === oldEpoch && pin.phase === "running")
      publishHumanFile(copyHumanPin(pin, "partial", "Not confirmed. ibara restarted on " + computerLabelFor(computerId) + " before this transfer finished. Look for the file in the Files tab before sending it again.", pin.jobId))
  }
  // Every session publication goes through here, so each computer carries its fleet state, actor
  // and activity line, and what waits for your answer there (`waiting`, from fleet-attention), and
  // sessions and computers always hold the same objects.
  function publishSessions(next, rows) {
    var decorated = ({}), waiting = waitingByComputer()
    for (var id in next) {
      var s = next[id]
      decorated[id] = s && typeof s === "object" ? StatusModel.decorateSession(Object.assign({}, s, { waiting: waiting[id] || null })) : s
    }
    sessions = decorated
    computers = (rows || computers).map(function(item) { return item && decorated[item.computer_id] }).filter(function(item) { return !!item })
  }
  function waitingByComputer() {
    var out = ({})
    // A login request waits for your approval like any other.
    var asks = approvals.concat(logins).filter(function(item) { return root.requestNeedsAlert(item) })
    for (var i = 0; i < asks.length; i++) { var a = asks[i].computer_id; out[a] = out[a] || { approvals: 0, questions: 0 }; out[a].approvals += 1 }
    for (var j = 0; j < questions.length; j++) { if (!requestNeedsAlert(questions[j])) continue; var q = questions[j].computer_id; out[q] = out[q] || { approvals: 0, questions: 0 }; out[q].questions += 1 }
    return out
  }
  // An approval or a question that comes or goes changes its computer's state everywhere.
  function republishWaiting() {
    var waiting = waitingByComputer()
    for (var id in sessions) if (!sameValue(sessions[id] && sessions[id].waiting || null, waiting[id] || null)) { publishSessions(sessions); return }
  }
  onApprovalsChanged: Qt.callLater(root.republishWaiting)
  onQuestionsChanged: Qt.callLater(root.republishWaiting)
  readonly property var fleetCounts: StatusModel.fleetCounts(computers)
  // The live task a computer reports to an observing operator: title and start, its latest step in
  // plain words and its latest click (a fraction of the screen), bound to its task_ref.
  function sanitizedActiveTask(value, ref) {
    if (!value || typeof value !== "object" || Array.isArray(value)) return null
    var taskRef = String(value.task_ref || ""), started = String(value.started_at || "")
    if (!/^[A-Za-z0-9_.:-]{1,128}$/.test(taskRef) || taskRef !== String(ref || "")) return null
    var step = value.last_step && typeof value.last_step === "object" ? value.last_step : null
    var point = value.last_point && typeof value.last_point === "object" ? value.last_point : null
    var px = point && typeof point.x === "number" ? point.x : NaN, py = point && typeof point.y === "number" ? point.y : NaN
    return { task_ref: taskRef, title: StatusModel.clip(value.title, 160),
      principal: /^[A-Za-z0-9_.:-]{1,64}$/.test(String(value.principal || "")) ? String(value.principal) : "",
      state: /^[a-z_]{1,32}$/.test(String(value.state || "")) ? String(value.state) : "",
      connection: ["connected", "disconnected"].indexOf(String(value.connection || "")) >= 0 ? String(value.connection) : "",
      active_jobs: typeof value.active_jobs === "number" && isFinite(value.active_jobs) ? Math.max(0, Math.floor(value.active_jobs)) : 0,
      uncertain_jobs: typeof value.uncertain_jobs === "number" && isFinite(value.uncertain_jobs) ? Math.max(0, Math.floor(value.uncertain_jobs)) : 0,
      started_at: /^\d{4}-\d\d-\d\dT/.test(started) && isFinite(StatusModel.isoMs(started)) ? started : "",
      last_step: step && typeof step.summary === "string" && step.summary.trim() ? { summary: StatusModel.clip(step.summary.trim(), 80), at: String(step.at || "") } : null,
      last_point: point && px >= 0 && px <= 1 && py >= 0 && py <= 1 && typeof point.at === "string" && point.at ? { x: px, y: py, at: point.at } : null }
  }
  // The most recent finished task on a computer, or null.
  function sanitizedLastTask(value) {
    if (!value || typeof value !== "object" || Array.isArray(value)) return null
    var ref = String(value.ref || "")
    if (!/^[A-Za-z0-9_.:-]{1,128}$/.test(ref) || ["done", "failed", "cancelled", "blocked"].indexOf(value.outcome) === -1) return null
    return { ref: ref, title: StatusModel.clip(value.title, 160), outcome: value.outcome, finished_at: String(value.finished_at || "") }
  }
  // Moments the console shows on a computer's picture: its agent clicked at (x, y), a fraction of
  // the screen; its task finished done. Omarchy's update there failing is a toast. Each fires once,
  // when a later status read differs from the one before, so opening the console never replays an
  // old click, Done or failure.
  signal agentClicked(string computerId, real x, real y)
  signal taskDone(string computerId)
  // An agent began its first task through this computer: its name and the computer it uses ("" when unknown).
  signal agentConnected(string computerId, string name)
  function noteStatusMoments(computerId, before, after) {
    noteIbaraUpdate(computerId, before, after)
    if (!before || before.last_task === undefined) return
    var oldPoint = before.active_task && before.active_task.last_point, newPoint = after.active_task && after.active_task.last_point
    if (newPoint && (!oldPoint || oldPoint.at !== newPoint.at)) agentClicked(computerId, newPoint.x, newPoint.y)
    var oldRef = before.last_task ? before.last_task.ref : ""
    if (after.last_task && after.last_task.ref !== oldRef && after.last_task.outcome === "done") taskDone(computerId)
    var oldUpdate = before.omarchy_update, newUpdate = after.omarchy_update
    if (newUpdate && newUpdate.state === "failed" && !(oldUpdate && oldUpdate.state === "failed" && oldUpdate.finished_at === newUpdate.finished_at))
      powerAnswered(computerId, "update_omarchy", "Omarchy's update on " + computerLabelFor(computerId) + " failed. See its System tab.", true)
  }
  // The first agent to begin a task here: a toast names it and the computer it is using, whose
  // card lights up once. The computer is the one whose running task started last.
  function announceFirstAgent() {
    var best = null
    for (var i = 0; i < computers.length; i++) {
      var task = computers[i] && computers[i].active_task
      if (task && (!best || String(task.started_at) > String(best.active_task.started_at))) best = computers[i]
    }
    var principal = best ? String(best.active_task.principal || "") : ""
    var name = principal.indexOf("@") > 0 ? principal : principal ? "An agent from " + principal : "An agent"
    var computerId = best ? String(best.computer_id) : ""
    actionNotice = name + " connected." + (computerId ? " It can use " + computerLabelFor(computerId) + "." : "")
    agentConnected(computerId, name)
  }

  // Console routes. The properties let a console created after the request open in the right place.
  property string requestedRoute: "fleet"
  property string requestedComputerId: ""
  property int routeSerial: 0
  signal openComputerRequested(string computerId)
  signal openFleetRequested()
  signal openAddRequested()
  signal openShareRequested()
  function requestRoute(route, computerId) {
    requestedRoute = route
    requestedComputerId = computerId
    routeSerial += 1
    dismissNotice()
  }
  function openComputer(computerId) {
    var id = String(computerId || "")
    if (!selectComputer(id)) return false
    if (!warmComputer(id)) whenConnected(id, function() { root.warmComputer(id) })
    scopeComputer(id)
    requestRoute("computer", id)
    openComputerRequested(id)
    return true
  }
  function openFleet() {
    clearSelectedComputer()
    scopeComputer("")
    requestRoute("fleet", "")
    openFleetRequested()
  }
  function openAdd() {
    clearSelectedComputer()
    scopeComputer("")
    requestRoute("add", "")
    openAddRequested()
  }
  // Share This Computer: invites for friends, made on this computer's own console.
  function openShare() {
    clearSelectedComputer()
    scopeComputer("")
    requestRoute("share", "")
    openShareRequested()
  }
  function dismissNotice() {
    actionNotice = ""
    actionError = ""
  }

  // Reads and actions for one computer go over its pairing route (`--computer ID --epoch E`).
  // Switching scope drops every list that belonged to the last one, then shows what the new
  // computer's tabs last read (see scopedCache) while its tabs read again.
  property string scopedComputerId: ""
  property int scopeGeneration: 0
  readonly property var scopedReadKinds: ["tasks", "task", "artifacts", "procedures", "procedure", "health", "logs", "access", "computer-settings", "windows"]
  function scopeComputer(computerId) {
    var id = String(computerId || "")
    if (id && !sessions[id]) return false
    if (id === scopedComputerId) return true
    scopedComputerId = id
    scopeGeneration += 1
    readQueue = readQueue.filter(function(request) { return !request.scoped })
    var errors = Object.assign({}, readErrors)
    for (var i = 0; i < scopedReadKinds.length; i++) delete errors[scopedReadKinds[i]]
    readErrors = errors
    for (var k = 0; k < cachedReadKinds.length; k++) setScopedValue(cachedReadKinds[k], emptyScopedValue(cachedReadKinds[k]))
    selectedTask = null; selectedTaskDetail = null; selectedTaskDetailRef = ""; selectedTaskDetailObservedAt = ""
    selectedTaskRef = ""; selectedTaskLoading = false
    selectedProcedure = null; selectedProcedureRef = ""; selectedProcedureLoading = false
    selectedArtifactRef = ""
    scopedReadAt = ({})
    if (id) restoreScoped(id)
    return true
  }

  // ---- each computer's last tab data. Coming back to a computer shows what its tabs last read
  // at once, with its age (scopedReadAt), while the tabs read again in the background. Memory only.
  // An entry holds only while the computer keeps the endpoint, binding revision and authorization
  // generation it was read under. A refusal or failed read drops it, a computer that leaves the
  // directory is dropped, and clearProtectedViews drops them all.
  readonly property var cachedReadKinds: ["tasks", "artifacts", "procedures", "health", "logs", "access", "computer-settings", "windows"]
  property var scopedCache: ({})
  property var scopedReadAt: ({})
  function cacheIdentity(computerId) {
    var s = sessions[String(computerId || "")]
    return s && s.trust_state === "verified" && s.endpoint_id ? [s.endpoint_id, s.binding_revision, s.authorization_generation].join("|") : ""
  }
  function emptyScopedValue(kind) {
    return kind === "health" ? ({}) : kind === "access" ? null : kind === "windows" ? ({ workspaces: [], agent: null }) : []
  }
  function scopedValue(kind) {
    return kind === "tasks" ? tasks : kind === "artifacts" ? artifacts : kind === "procedures" ? procedures
      : kind === "health" ? health : kind === "logs" ? logLines : kind === "access" ? accessTable
      : kind === "windows" ? computerWindows : computerSettings
  }
  // A value put back marks its list loaded; an empty value marks it not loaded yet.
  function setScopedValue(kind, value, loaded) {
    if (kind === "tasks") { tasks = value; tasksLoaded = loaded === true }
    else if (kind === "artifacts") { artifacts = value; artifactsLoaded = loaded === true }
    else if (kind === "procedures") { procedures = value; proceduresLoaded = loaded === true }
    else if (kind === "health") health = value
    else if (kind === "logs") logLines = value
    else if (kind === "access") accessTable = value
    else if (kind === "computer-settings") computerSettings = value
    else if (kind === "windows") { computerWindows = value; windowsLoaded = loaded === true }
  }
  function rememberScoped(kind) {
    var id = scopedComputerId, identity = cacheIdentity(id)
    if (!id || !identity || cachedReadKinds.indexOf(kind) === -1) return
    var at = Date.now()
    var entry = scopedCache[id] && scopedCache[id].identity === identity ? scopedCache[id] : { identity: identity, kinds: ({}) }
    var kinds = Object.assign({}, entry.kinds)
    kinds[kind] = { value: scopedValue(kind), at: at }
    var cache = Object.assign({}, scopedCache)
    cache[id] = { identity: identity, kinds: kinds }
    scopedCache = cache
    var ages = Object.assign({}, scopedReadAt)
    ages[kind] = at
    scopedReadAt = ages
  }
  function restoreScoped(computerId) {
    var entry = scopedCache[computerId]
    if (!entry) return
    if (entry.identity !== cacheIdentity(computerId)) { forgetComputerCaches(computerId); return }
    var ages = ({})
    for (var kind in entry.kinds) {
      setScopedValue(kind, entry.kinds[kind].value, true)
      ages[kind] = entry.kinds[kind].at
    }
    scopedReadAt = ages
  }
  // A read that failed shows its error, never data from before it.
  function forgetScopedKind(computerId, kind) {
    if (computerId === scopedComputerId) {
      setScopedValue(kind, emptyScopedValue(kind))
      if (scopedReadAt[kind]) { var ages = Object.assign({}, scopedReadAt); delete ages[kind]; scopedReadAt = ages }
    }
    var entry = scopedCache[computerId]
    if (!entry || !entry.kinds[kind]) return
    var kinds = Object.assign({}, entry.kinds)
    delete kinds[kind]
    var cache = Object.assign({}, scopedCache)
    cache[computerId] = { identity: entry.identity, kinds: kinds }
    scopedCache = cache
  }
  function forgetComputerCaches(computerId) {
    var id = String(computerId || "")
    if (scopedCache[id]) { var cache = Object.assign({}, scopedCache); delete cache[id]; scopedCache = cache }
    if (remoteFileCache[id]) { var files = Object.assign({}, remoteFileCache); delete files[id]; remoteFileCache = files }
  }
  // Entries for computers no longer in the directory, or read under another identity, go.
  function pruneComputerCaches() {
    var id
    for (id in scopedCache) if (scopedCache[id].identity !== cacheIdentity(id)) forgetComputerCaches(id)
    for (id in remoteFileCache) if (remoteFileCache[id].identity !== fileCacheIdentity(id)) forgetComputerCaches(id)
  }
  // A computer's command line over its pairing route, or null while ibara has no connection to it yet.
  function computerArgs(command, computerId, rest) {
    var id = String(computerId || ""), s = sessions[id]
    if (!s || s.trust_state !== "verified" || !s.controller_epoch) return null
    return [String(command), "--computer", id, "--epoch", String(s.controller_epoch)].concat(rest || [])
  }
  // A read for the computer in scope. One asked for before its connection is up waits for it.
  function requestScopedRead(kind, command, rest, ref) {
    var id = scopedComputerId, generation = scopeGeneration
    if (!id) return
    var args = computerArgs(command, id, rest)
    if (args) { requestRead(kind, args, ref, null, { id: id, generation: generation }); return }
    whenConnected(id, function() {
      if (root.scopedComputerId === id && root.scopeGeneration === generation) root.requestScopedRead(kind, command, rest, ref)
    }, function(message) {
      if (root.scopedComputerId !== id || root.scopeGeneration !== generation) return
      var errors = Object.assign({}, root.readErrors)
      errors[kind] = message
      root.readErrors = errors
    })
  }
  // A scoped reply must come from the scope that asked and name that computer and its current connection.
  function scopedReplyCurrent(scope, parsed) {
    var session = sessions[scope.id], data = parsed.data || ({})
    return !!scope && scope.generation === scopeGeneration && scope.id === scopedComputerId && !!session &&
      data.computer_id === scope.id && !!session.controller_epoch && data.controller_epoch === session.controller_epoch
  }

  // Local file choice runs the desktop chooser through ibarad, outside Quickshell and off
  // the action lane. Choosing nothing leaves everything as it was.
  property bool picking: false
  property string pickKind: ""
  property string pickRequestId: ""
  property int pickSequence: 0
  property var pickedFile: null
  property var pickedFolder: null
  // Where files from other computers are saved: the console's Download folder setting, else ~/Downloads.
  readonly property string downloadFolder: humanFolder(String(consoleSetting("download_folder", "") || "")) || humanFolder(String(Quickshell.env("HOME") || "") + "/Downloads")
  function pickLocal(kind) {
    if (picking) return false
    pickSequence += 1
    pickKind = kind
    pickRequestId = "pick-" + pickSequence
    picking = true
    daemonSend(pickRequestId, [kind], { kind: "pick" })
    return true
  }
  function pickLocalFile() { return pickLocal("pick-file") }
  function pickLocalFolder() { return pickLocal("pick-folder") }
  function clearPickedFolder() { pickedFolder = null }
  function consumePick(text) {
    picking = false
    try {
      var parsed = StatusModel.parseEnvelope(text)
      if (parsed.request_id !== pickRequestId) return
      if (parsed.error) { actionError = String(parsed.error.message || parsed.error.code || "The file chooser did not open."); return }
      var data = parsed.data || ({})
      if (data.picked !== true) return
      var chosen = String(data.path || "")
      if (!humanAbsolutePath(chosen) || humanBaseName(chosen) !== String(data.name || "")) throw new Error("The file chooser returned an unusable path.")
      var parent = chosen.substring(0, chosen.lastIndexOf("/")) || "/"
      if (pickKind === "pick-folder") { pickedFolder = { path: chosen, name: String(data.name), folder: parent }; return }
      if (typeof data.size !== "number" || !isFinite(data.size) || data.size < 0) throw new Error("The file chooser returned no file size.")
      pickedFile = { path: chosen, name: String(data.name), folder: parent, size: data.size }
    } catch (error) {
      actionError = StatusModel.clip(error, 200)
    }
  }

  // This session's transfers, newest first. Each follows one human-file pin from start to receipt.
  property var fileTransfers: []
  property int transferSequence: 0
  function syncTransfer(pin) {
    if (!pin || !pin.transferId) return
    var phase = String(pin.phase || "")
    var state = phase === "running" ? "running" : (phase === "verified" || phase === "verified-collected") ? "verified" : "failed"
    var receipt = pin.receipt || null
    var entry = {
      id: pin.transferId, computerId: pin.computerId, direction: pin.direction, name: humanBaseName(pin.remote),
      rootId: pin.rootId, remote: pin.remote, local: pin.local, state: state,
      bytes: receipt ? receipt.size : 0, total: receipt ? receipt.size : Number(pin.total || 0),
      jobId: pin.jobId || "", error: state === "failed" ? String(pin.notice || "") : "",
      // Retry never repeats an uncertain upload: it resumes the retained job, or sends again only
      // when nothing started. Downloads never overwrite, so they may always run again.
      retryable: state === "failed" && phase !== "revoked" &&
        (pin.direction === "receive" || phase === "failed" || (phase === "partial" && !!pin.jobId)),
      openWhenDone: pin.openWhenDone === true
    }
    var list = fileTransfers.filter(function(item) { return item.id !== entry.id })
    fileTransfers = [entry].concat(list).slice(0, 20)
  }
  function transferTotal(kind, local, remote) {
    if (kind !== "human-file-receive") return pickedFile && pickedFile.path === local ? pickedFile.size : 0
    var name = humanBaseName(remote)
    for (var i = 0; i < remoteFileEntries.length; i++)
      if (remoteFileEntries[i].name === name && typeof remoteFileEntries[i].size === "number") return remoteFileEntries[i].size
    return 0
  }
  function sendPickedFile(computerId, rootId, saveAsName, remoteDirectory) {
    var file = pickedFile
    if (!file) { actionError = "Choose a file first."; return false }
    var name = String(saveAsName || file.name)
    if (!humanFileSegment(name)) { actionError = "Choose another name: it cannot start with a dot or a dash, or contain a slash."; return false }
    var folder = humanRemoteDirectory(remoteDirectory || ".")
    if (folder === "") { actionError = "That folder is not one this computer allows."; return false }
    return beginHumanFile("human-file-send", computerId, rootId, folder === "." ? name : folder + "/" + name, file.path, "", "", false)
  }
  function getRemoteFile(computerId, rootId, remotePath) {
    var chosen = !!pickedFolder
    var local = humanReceiveLocal(chosen ? pickedFolder.path : downloadFolder, humanBaseName(remotePath))
    if (!local) { actionError = "The download folder is not usable. Choose another folder."; return false }
    return beginHumanFile("human-file-receive", computerId, rootId, remotePath, local, "", "", chosen)
  }
  function retryTransfer(t) {
    var id = String(t && typeof t === "object" ? t.id : t || ""), entry = null
    for (var i = 0; i < fileTransfers.length; i++) if (fileTransfers[i].id === id) entry = fileTransfers[i]
    if (!entry || entry.state !== "failed" || !entry.retryable) return false
    if (entry.computerId !== remoteFileComputerId) { actionError = "Open this computer's Files tab to retry."; return false }
    if (entry.direction === "send" && entry.jobId)
      return beginHumanFile("human-file-resume", entry.computerId, entry.rootId, entry.remote, entry.local, entry.jobId, entry.id, false)
    return beginHumanFile(entry.direction === "receive" ? "human-file-receive" : "human-file-send", entry.computerId, entry.rootId, entry.remote, entry.local, "", entry.id, entry.openWhenDone)
  }
  property var remoteFileRoots: []
  property var remoteFileEntries: []
  property string remoteFileComputerId: ""
  property string remoteFileRootId: ""
  property string remoteFileDirectory: "."
  property int remoteFileGeneration: 0
  property bool remoteFileListing: false
  property bool remoteFileLoaded: false
  property bool remoteFileEntriesLoaded: false
  property string remoteFileError: ""
  property var humanFileTransfer: null
  property bool followHumanFile: false
  property var pendingControl: null
  // Listings and transfers stay bound to the computer, folder and names that started them.
  function humanFileSegment(value) {
    var text = String(value || "")
    if (!text || text.length > 255) return false
    if (text.charCodeAt(0) === 46 || text.charCodeAt(0) === 45) return false
    for (var i = 0; i < text.length; i++) {
      var code = text.charCodeAt(i)
      if (code < 32 || code === 47 || code === 92) return false
    }
    return true
  }
  function humanRootId(value) {
    var text = String(value || "")
    return /^[a-z][a-z0-9_-]{0,63}$/.test(text) ? text : ""
  }
  function humanJobId(value) {
    var text = String(value || "")
    return /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$/.test(text) ? text : ""
  }
  function humanRemoteDirectory(value) {
    var text = String(value || "")
    if (text === ".") return "."
    if (!text || text.length > 512 || text.charAt(0) === "/") return ""
    var parts = text.split("/")
    for (var i = 0; i < parts.length; i++) if (!humanFileSegment(parts[i])) return ""
    return text
  }
  function humanRemotePath(value) {
    var text = String(value || "")
    if (!text || text.length > 1024 || text.charAt(0) === "/") return ""
    var slash = text.lastIndexOf("/")
    var directory = slash < 0 ? "." : text.substring(0, slash)
    var name = slash < 0 ? text : text.substring(slash + 1)
    if (humanRemoteDirectory(directory) === "" || !humanFileSegment(name)) return ""
    return directory === "." ? name : directory + "/" + name
  }
  function humanAbsolutePath(value) {
    var text = String(value || "")
    if (text.length < 2 || text.length > 4096 || text.charAt(0) !== "/" || text.indexOf("//") >= 0) return false
    var parts = text.split("/")
    for (var i = 1; i < parts.length; i++) {
      if (!parts[i] || parts[i] === "." || parts[i] === "..") return false
      for (var n = 0; n < parts[i].length; n++) if (parts[i].charCodeAt(n) < 32 || parts[i].charCodeAt(n) === 92) return false
    }
    return true
  }
  function humanFolder(value) {
    var text = String(value || "")
    if (text.length > 1 && text.charAt(text.length - 1) === "/") text = text.substring(0, text.length - 1)
    return humanAbsolutePath(text) ? text : ""
  }
  function humanBaseName(value) {
    var parts = String(value || "").split("/")
    return parts.length ? parts[parts.length - 1] : ""
  }
  function humanReceiveLocal(folder, name) {
    var parent = humanFolder(folder)
    if (!parent || !humanFileSegment(name)) return ""
    var local = parent + "/" + name
    return humanAbsolutePath(local) && humanBaseName(local) === name ? local : ""
  }
  function remoteFileKind(op, computerId, generation) {
    return "operator-files-" + op + ":" + computerId + "#gen=" + generation
  }
  function remoteFileKindOp(kind) {
    var text = String(kind || "")
    if (text.indexOf("operator-files-roots:") === 0) return "roots"
    if (text.indexOf("operator-files-list:") === 0) return "list"
    return ""
  }
  function remoteFileKindGeneration(kind) {
    var match = String(kind || "").match(/#gen=(\d+)$/)
    return match ? parseInt(match[1], 10) : -1
  }
  function remoteFileResponseCurrent(kind, ref) {
    return remoteFileKindGeneration(kind) === remoteFileGeneration && String(ref || "") === remoteFileComputerId
  }
  function cancelQueuedFileReads() {
    var queue = []
    for (var i = 0; i < readQueue.length; i++) {
      if (String(readQueue[i].kind).indexOf("operator-files-") !== 0) queue.push(readQueue[i])
    }
    readQueue = queue
  }
  function setStableFileError(op, computerId, message) {
    var errors = Object.assign({}, readErrors)
    var key = "operator-files-" + op + ":" + computerId
    if (message) errors[key] = message
    else delete errors[key]
    readErrors = errors
  }
  function clearRemoteFiles() {
    remoteFileGeneration += 1
    remoteFileRoots = []
    remoteFileEntries = []
    remoteFileRootId = ""
    remoteFileDirectory = "."
    remoteFileListing = false
    remoteFileLoaded = false
    remoteFileEntriesLoaded = false
    remoteFileError = ""
    remoteFileReadAt = 0
    remoteFileCache = ({})
  }
  // The Files tab's last listing per computer, as scopedCache keeps the other tabs': shown at once
  // when you come back, then read again. It also holds only for the controller epoch it was read in.
  property var remoteFileCache: ({})
  property double remoteFileReadAt: 0
  function fileCacheIdentity(computerId) {
    var s = sessions[String(computerId || "")]
    return cacheIdentity(computerId) && s.controller_epoch ? cacheIdentity(computerId) + "|" + s.controller_epoch : ""
  }
  function rememberRemoteFiles(computerId) {
    var id = String(computerId || ""), identity = fileCacheIdentity(id)
    if (!identity || id !== remoteFileComputerId) return
    var cache = Object.assign({}, remoteFileCache)
    cache[id] = { identity: identity, roots: remoteFileRoots, rootId: remoteFileRootId, directory: remoteFileDirectory,
      entries: remoteFileEntriesLoaded ? remoteFileEntries : [], entriesLoaded: remoteFileEntriesLoaded, at: Date.now() }
    remoteFileCache = cache
    remoteFileReadAt = cache[id].at
  }
  function queueRemoteFileRead(op, computerId, args) {
    remoteFileListing = true
    cancelQueuedFileReads()
    var kind = remoteFileKind(op, computerId, remoteFileGeneration)
    requestRead(kind, args, computerId)
    if (!readPending(kind)) {
      remoteFileListing = false
      remoteFileError = "Failed. The file list could not be requested."
      setStableFileError(op, computerId, remoteFileError)
    }
  }
  function listRemoteFileRoots(computerId) {
    remoteFileGeneration += 1
    remoteFileComputerId = String(computerId || "")
    remoteFileRootId = ""
    remoteFileDirectory = "."
    remoteFileError = ""
    remoteFileLoaded = false
    remoteFileEntriesLoaded = false
    remoteFileReadAt = 0
    setStableFileError("roots", remoteFileComputerId, "")
    setStableFileError("list", remoteFileComputerId, "")
    var session = sessions[computerId]
    if (denied || !session || !session.controller_epoch || session.trust_state !== "verified" || !session.endpoint_id || session.binding_revision === undefined || session.binding_revision === null) {
      remoteFileListing = false
      remoteFileRoots = []
      remoteFileEntries = []
      return
    }
    // Back on a computer: its last listing shows at once while the folder it shows is read again.
    var cached = remoteFileCache[remoteFileComputerId]
    if (cached && cached.identity === fileCacheIdentity(remoteFileComputerId)) {
      remoteFileRoots = cached.roots
      remoteFileLoaded = true
      remoteFileRootId = cached.rootId
      remoteFileDirectory = cached.directory
      remoteFileEntries = cached.entries
      remoteFileEntriesLoaded = cached.entriesLoaded
      remoteFileReadAt = cached.at
      if (cached.rootId) {
        queueRemoteFileRead("list", computerId, ["operator-files", "--computer", computerId, "--epoch", session.controller_epoch, "--op", "files_list", "--root", cached.rootId, "--directory", cached.directory])
        return
      }
    } else if (cached) {
      forgetComputerCaches(remoteFileComputerId)
    }
    remoteFileListing = true
    if (!remoteFileLoaded) { remoteFileRoots = []; remoteFileEntries = [] }
    queueRemoteFileRead("roots", computerId, ["operator-files", "--computer", computerId, "--epoch", session.controller_epoch, "--op", "files_roots"])
  }
  function listRemoteFileDirectory(computerId, rootId, directory) {
    var rootName = humanRootId(rootId)
    var folder = humanRemoteDirectory(directory)
    var session = sessions[computerId]
    if (!rootName || folder === "" || String(computerId) !== remoteFileComputerId || denied || !session || !session.controller_epoch || session.trust_state !== "verified") return
    remoteFileGeneration += 1
    remoteFileListing = true
    // Reading the folder already shown again keeps its entries on screen until the new ones arrive.
    if (rootName !== remoteFileRootId || folder !== remoteFileDirectory) {
      remoteFileEntries = []
      remoteFileEntriesLoaded = false
      remoteFileReadAt = 0
    }
    remoteFileRootId = rootName
    remoteFileDirectory = folder
    remoteFileError = ""
    setStableFileError("list", computerId, "")
    queueRemoteFileRead("list", computerId, ["operator-files", "--computer", computerId, "--epoch", session.controller_epoch, "--op", "files_list", "--root", rootName, "--directory", folder])
  }
  function humanFileCommand(kind) {
    if (kind === "human-file-receive") return "operator-file-receive"
    if (kind === "human-file-resume") return "operator-file-resume"
    return "operator-file-send"
  }
  function beginHumanFile(kind, computerId, rootId, remote, local, jobId, transferId, openWhenDone) {
    if (mutating || denied) return false
    if (remoteFileComputerId === String(computerId) && remoteFileError.indexOf("Revoked") === 0) return false
    var session = sessions[computerId]
    var rootName = humanRootId(rootId)
    var remotePath = humanRemotePath(remote)
    var localPath = humanAbsolutePath(local) ? String(local) : ""
    var retained = humanJobId(jobId)
    if (!session || String(computerId) !== remoteFileComputerId || !session.controller_epoch || session.trust_state !== "verified" || !session.endpoint_id || session.binding_revision === undefined || session.binding_revision === null) return false
    if (!rootName || !remotePath || !localPath) return false
    if (kind === "human-file-resume" && !retained) return false
    if (kind === "human-file-receive" && humanBaseName(localPath) !== humanBaseName(remotePath)) return false
    var current = humanFileTransfer
    if (kind === "human-file-send" && current && current.phase === "partial" && current.computerId === String(computerId) && current.rootId === rootName && current.remote === remotePath) return false
    if (kind === "human-file-resume" && current && current.jobId === retained && (current.computerId !== String(computerId) || current.rootId !== rootName || current.remote !== remotePath || current.local !== localPath)) return false
    var pin = {
      phase: "running",
      notice: "In progress. Transferring " + remotePath + " for computer " + computerId + ", approved folder " + rootName + ", local " + localPath + ". This has not finished.",
      computerId: String(computerId), rootId: rootName, remote: remotePath, local: localPath,
      jobId: kind === "human-file-resume" ? retained : "",
      epoch: String(session.controller_epoch), endpointId: String(session.endpoint_id),
      bindingRevision: session.binding_revision, authorizationGeneration: session.authorization_generation,
      follow: followHumanFile,
      transferId: String(transferId || "") || "transfer-" + (++transferSequence),
      direction: kind === "human-file-receive" ? "receive" : "send",
      total: transferTotal(kind, localPath, remotePath), openWhenDone: openWhenDone === true
    }
    var args = [humanFileCommand(kind), "--computer", pin.computerId, "--epoch", pin.epoch, "--root", pin.rootId, "--remote", pin.remote, "--local", pin.local]
    if (kind === "human-file-resume") args.push("--job", retained)
    humanFileTransfer = pin
    startHelper(kind, args)
    if (pendingMutation !== kind) {
      publishHumanFile(copyHumanPin(pin, "failed", "Failed. The transfer did not start. Nothing was verified.", pin.jobId))
      return false
    }
    syncTransfer(pin)
    return true
  }
  function argAfter(args, name) {
    var at = args.indexOf(name)
    return at >= 0 && at + 1 < args.length ? String(args[at + 1]) : ""
  }
  function remoteFileRevoked(parsed) {
    var error = parsed && parsed.error ? parsed.error : ({})
    var code = String(error.code || "")
    var message = String(error.message || "")
    return !!parsed && (parsed.connection === "unauthorized" ||
      ["UNAUTHORIZED", "PERMISSION_DENIED", "AUTH_REQUIRED", "GRANT_REVOKED", "IDENTITY_MISMATCH"].indexOf(code) !== -1 ||
      /revok|no longer approved|grant unavailable|identity changed/i.test(message))
  }
  function humanFileReceipt(data) {
    var size = data ? data.size_bytes : null
    var sha = String(data && data.sha256 || "")
    if (typeof size !== "number" || !isFinite(size) || Math.floor(size) !== size || size < 0 || size > 500 * 1000 * 1000) return null
    if (!/^[a-f0-9]{64}$/.test(sha)) return null
    return { size: size, sha: sha }
  }
  function humanJobFromMessage(message) {
    var match = String(message || "").match(/Upload ([A-Za-z0-9][A-Za-z0-9_.:-]{0,127}) is partial or uncertain/)
    return match && humanJobId(match[1]) ? match[1] : ""
  }
  function computerLabelFor(computerId) {
    var session = sessions[String(computerId || "")]
    return session && session.label ? String(session.label) : "this computer"
  }
  // A computer that reports no display has nothing to preview yet; ibara keeps asking.
  readonly property string noDisplayNotice: "This computer has no display yet. ibara shows its screen once one appears."
  // A followed task's name for a notification: its title when the computer still reports it.
  function followedTaskName(session, taskRef) {
    var task = session && session.active_task
    return task && task.task_ref === taskRef && task.title ? "\u201c" + String(task.title) + "\u201d" : "The task you follow"
  }
  // A failure message in plain English, naming the computer it came from.
  function plainError(message, computerId, retrying) {
    var session = sessions[String(computerId || "")]
    return StatusModel.plainError(message, session && session.label ? String(session.label) : "", retrying === true)
  }
  function publishHumanFile(next) {
    next.notice = StatusModel.clip(next.notice, 800)
    humanFileTransfer = next
    syncTransfer(next)
  }
  function copyHumanPin(pin, phase, notice, jobId) {
    return {
      phase: phase, notice: notice, jobId: jobId || "",
      computerId: pin.computerId, rootId: pin.rootId, remote: pin.remote, local: pin.local,
      epoch: pin.epoch, endpointId: pin.endpointId, bindingRevision: pin.bindingRevision,
      authorizationGeneration: pin.authorizationGeneration, follow: pin.follow === true,
      transferId: pin.transferId || "", direction: pin.direction || "", total: Number(pin.total || 0), openWhenDone: pin.openWhenDone === true
    }
  }
  function settleHumanFile(kind, parsed, outcome) {
    var pin = humanFileTransfer
    if (!pin || pin.phase !== "running" || String(kind).indexOf("human-file-") !== 0) return
    if (outcome === "unmatched" || outcome === "stale-access" || outcome === "unreadable") {
      publishHumanFile(copyHumanPin(pin, "partial", "Not confirmed. ibara couldn't check that " + pin.remote + " reached " + computerLabelFor(pin.computerId) + ". Look for it in the Files tab before sending it again." + (pin.jobId ? " Use Retry to finish it." : ""), pin.jobId))
      return
    }
    if (!parsed || parsed.error || outcome === "error") {
      var message = String(parsed && parsed.error ? (parsed.error.message || parsed.error.code || "The transfer failed.") : "The transfer failed.")
      var phase = remoteFileRevoked(parsed) ? "revoked" : (parsed && parsed.error && (parsed.error.retry_safe === false || /partial|uncertain|reply was lost|before repeating|do not repeat|inspect the target|inspect its status/i.test(message)) ? "partial" : "failed")
      var mentioned = humanJobFromMessage(message)
      var job = mentioned && (!pin.jobId || mentioned === pin.jobId) ? mentioned : (phase === "partial" && pin.jobId && !mentioned ? pin.jobId : "")
      // ibarad names the job in its own words; the job travels in the entry, and Retry uses it.
      message = message.replace(/^Upload [A-Za-z0-9][A-Za-z0-9_.:-]{0,127} is partial or uncertain\. Inspect its status; resume explicitly only after verifying original source and destination\. ?/, "")
      var notice = "Failed. Nothing was sent or saved. " + message
      if (phase === "revoked") notice = "Your access to " + computerLabelFor(pin.computerId) + " changed, so ibara stopped checking this transfer. The Access tab shows who can use " + computerLabelFor(pin.computerId) + "."
      else if (phase === "partial") notice = "Not confirmed. " + message + " Look for " + pin.remote + " in the Files tab before sending it again." + (job ? " Use Retry to finish it." : "")
      publishHumanFile(copyHumanPin(pin, phase, notice, job))
      return
    }
    var data = parsed.data || ({})
    var receipt = humanFileReceipt(data)
    var expected = kind === "human-file-receive" ? "verified-collected" : "verified"
    var jobId = humanJobId(data.job_id)
    var aligned = data.computer_id === pin.computerId && data.root_id === pin.rootId && data.remote_path === pin.remote &&
      String(data.endpoint_id || "") === String(pin.endpointId) && String(data.binding_revision) === String(pin.bindingRevision) &&
      (!data.controller_epoch || String(data.controller_epoch) === String(pin.epoch)) &&
      (kind !== "human-file-receive" || data.local_path === pin.local) &&
      (kind !== "human-file-resume" || jobId === pin.jobId) && !!pin.endpointId
    if (data.state === expected && receipt && jobId && aligned) {
      var settled = copyHumanPin(pin, expected, (expected === "verified" ? "Verified. Sent " : "Verified. Received ") + pin.remote + " for computer " + pin.computerId + ", folder " + pin.rootId + ", local " + (data.local_path || pin.local) + ". " + receipt.size + " bytes, sha256 " + receipt.sha + ". Job " + jobId + ".", jobId)
      settled.receipt = receipt
      publishHumanFile(settled)
      // A download to a folder the user chose opens that folder once its bytes are verified.
      if (expected === "verified-collected" && pin.openWhenDone && !openFolderProcess.running) {
        openFolderProcess.command = ["xdg-open", pin.local.substring(0, pin.local.lastIndexOf("/")) || "/"]
        openFolderProcess.running = true
      }
      var current = sessions[pin.computerId]
      if (pin.follow && !denied && current && current.connection !== "unauthorized" && current.trust_state === "verified" &&
          current.endpoint_id === pin.endpointId && current.binding_revision === pin.bindingRevision &&
          current.authorization_generation === pin.authorizationGeneration)
        queueNotice({ title: String(current.label || "ibara"),
          body: (expected === "verified" ? "Sent " + pin.remote + " to " : "Received " + pin.remote + " from ") + String(current.label || "the computer") + ". ibara checked it.",
          computerId: pin.computerId, endpointId: pin.endpointId, bindingRevision: pin.bindingRevision,
          authorizationGeneration: pin.authorizationGeneration, epoch: pin.epoch })
    } else
      publishHumanFile(copyHumanPin(pin, "partial", "Not confirmed. What " + computerLabelFor(pin.computerId) + " reported for " + pin.rootId + "/" + pin.remote + " did not match what was sent. Look for it in the Files tab before sending it again.", pin.jobId))
  }
  function sanitizedFileRoots(value) {
    if (!Array.isArray(value)) return null
    var roots = []
    var seen = ({})
    for (var i = 0; i < value.length && roots.length < 100; i++) {
      var id = value[i] && humanRootId(value[i].root_id)
      if (!id || seen[id]) continue
      seen[id] = true
      roots.push({ root_id: id })
    }
    return value.length && !roots.length ? null : roots
  }
  function sanitizedFileEntries(value) {
    if (!Array.isArray(value)) return null
    var entries = []
    for (var i = 0; i < value.length && entries.length < 100; i++) {
      var item = value[i]
      if (!item || (item.kind !== "file" && item.kind !== "directory") || !humanFileSegment(item.name)) continue
      // Size and change time only when the controller reports them; nothing is guessed.
      var size = typeof item.size === "number" && isFinite(item.size) && item.size >= 0 && Math.floor(item.size) === item.size ? item.size : null
      var modified = typeof item.modified === "string" && item.modified.length <= 40 && isFinite(StatusModel.isoMs(item.modified)) ? item.modified : ""
      entries.push({ name: String(item.name), kind: item.kind, size: size, modified: modified })
    }
    return value.length && !entries.length ? null : entries
  }
  function discardRemoteFileRead(kind, ref, notice) {
    if (!remoteFileResponseCurrent(kind, ref)) return
    remoteFileListing = false
    remoteFileError = StatusModel.clip(notice || "Failed. This file list no longer matches the current request. Refresh the folder and try again.", 300)
    setStableFileError(remoteFileKindOp(kind) || "list", ref, remoteFileError)
    remoteFileEntries = []
    remoteFileEntriesLoaded = false
    if (remoteFileKindOp(kind) === "roots") {
      remoteFileRoots = []
      remoteFileLoaded = false
    }
  }
  function fileSessionMatches(request, parsed, result) {
    var session = sessions[request.ref]
    var data = parsed.data || ({})
    if (!session || !result || !remoteFileResponseCurrent(request.kind, request.ref)) return false
    return data.computer_id === request.ref && argAfter(request.args, "--computer") === String(request.ref) &&
      argAfter(request.args, "--epoch") === String(session.controller_epoch) &&
      String(data.endpoint_id) === String(session.endpoint_id) && String(data.binding_revision) === String(session.binding_revision) &&
      String(data.controller_epoch) === String(session.controller_epoch) &&
      String(result.endpoint_id) === String(session.endpoint_id) && String(result.controller_epoch) === String(session.controller_epoch) &&
      String(result.authorization_generation) === String(session.authorization_generation)
  }
  function applyRemoteFileRead(request, parsed) {
    if (!remoteFileResponseCurrent(request.kind, request.ref)) return
    var op = remoteFileKindOp(request.kind)
    var generation = remoteFileGeneration
    try {
    if (parsed.error) {
      var detail = String(parsed.error.message || parsed.error.code || "Remote files are unavailable.")
      var revoked = remoteFileRevoked(parsed)
      remoteFileError = StatusModel.clip((revoked ? "Revoked. Access no longer matches this computer. " : "Failed. ") + detail + (revoked ? " The list was cleared." : ""), 300)
      setStableFileError(op || "list", request.ref, remoteFileError)
      remoteFileEntries = []
      remoteFileEntriesLoaded = false
      if (revoked || op === "roots") {
        remoteFileRoots = []
        remoteFileLoaded = false
      }
      if (revoked) forgetComputerCaches(request.ref)
      remoteFileReadAt = 0
      return
    }
    var result = (parsed.data || ({})).result || ({})
    if (!fileSessionMatches(request, parsed, result)) {
      remoteFileError = "Failed. The file list no longer matches the selected computer. Refresh the folder and try again."
      setStableFileError(op || "list", request.ref, remoteFileError)
      remoteFileEntries = []
      remoteFileEntriesLoaded = false
      if (op === "roots") { remoteFileRoots = []; remoteFileLoaded = false }
      return
    }
    if (op === "roots") {
      var roots = sanitizedFileRoots(result.roots)
      if (!roots) {
        remoteFileRoots = []
        remoteFileEntries = []
        remoteFileLoaded = false
        remoteFileEntriesLoaded = false
        remoteFileError = "Failed. The list of shared folders was unreadable. Choose Refresh to try again."
        setStableFileError("roots", request.ref, remoteFileError)
        return
      }
      remoteFileRoots = roots
      remoteFileLoaded = true
      remoteFileError = ""
      setStableFileError("roots", request.ref, "")
      rememberRemoteFiles(request.ref)
      return
    }
    var requestedRoot = argAfter(request.args, "--root")
    var requestedDirectory = argAfter(request.args, "--directory")
    if (requestedRoot !== remoteFileRootId || requestedDirectory !== remoteFileDirectory || result.root_id !== remoteFileRootId) {
      remoteFileEntries = []
      remoteFileEntriesLoaded = false
      remoteFileError = "Failed. The listing that arrived was for another folder. Choose Refresh to load this one."
      setStableFileError("list", request.ref, remoteFileError)
      return
    }
    var entries = sanitizedFileEntries(result.entries)
    if (!entries) {
      remoteFileEntries = []
      remoteFileEntriesLoaded = false
      remoteFileError = "Failed. The folder listing was unreadable. Choose Refresh to try again."
      setStableFileError("list", request.ref, remoteFileError)
      return
    }
    remoteFileEntries = entries
    remoteFileEntriesLoaded = true
    remoteFileError = ""
    setStableFileError("list", request.ref, "")
    rememberRemoteFiles(request.ref)
    } finally {
      if (generation === remoteFileGeneration) remoteFileListing = false
    }
  }
  property string selectedComputerId: ""
  // The console publishes whether the open computer's Screen tab is showing; only then does
  // the selected computer preview at selected quality.
  property bool watchVisible: false
  onWatchVisibleChanged: if (!watchVisible) previewQueue = previewQueue.filter(function(item) { return item.quality !== "selected" })
  property var tasks: []
  property var artifacts: []
  property var procedures: []
  property bool tasksLoaded: false
  property bool artifactsLoaded: false
  property bool proceduresLoaded: false
  // The scoped computer's workspaces, each with its windows, and the task holding control there
  // (the `windows` result: {workspaces, agent}).
  property var computerWindows: ({ workspaces: [], agent: null })
  property bool windowsLoaded: false
  // The window a Close or Move To… on the action lane is for: {computerId, address, title}.
  property var windowAction: null
  // A Close or Move To… finished for one window: "closed", "open" (it may be asking to save),
  // "moved", "busy" (an agent is using it), "gone", "pending-close" or "pending-move" (waiting for
  // approval) or "failed".
  signal windowActionSettled(string computerId, string address, string outcome)
  // While the Windows tab has a confirmation or a Move To… list open, a list read in the background
  // is dropped, not shown: rebuilding the rows would close them. The tab reads again after.
  property bool windowsHeld: false
  property var accessTable: null
  // The scoped computer's health report (operator-health result) and its last log lines.
  property var health: ({})
  property var logLines: []
  property string logsWhich: "ibara"
  // The scoped computer's own settings (operator-settings), as sections.
  property var computerSettings: []
  property string selectedTaskRef: ""
  property string selectedArtifactRef: ""
  property string selectedProcedureRef: ""
  property var selectedTask: null
  property var selectedTaskDetail: null
  property string selectedTaskDetailRef: ""
  property string selectedTaskDetailObservedAt: ""
  property bool selectedTaskLoading: false
  property var selectedProcedure: null
  property bool selectedProcedureLoading: false
  property string connectionState: "loading"
  property bool panelOpen: false
  property bool quickOpen: false
  property bool consoleOpen: false
  property bool consoleFocused: false
  readonly property bool requestsInConsole: consoleOpen && consoleFocused
  // Inline plugin settings use Omarchy's existing persisted settings store.
  readonly property var requestSettingDefinitions: [
    { key: "request_popups", title: "Request Pop-ups", help: "Show request pop-ups with answer buttons.", fallback: true },
    { key: "popup_approvals", title: "Approvals", help: "Show approval requests.", fallback: true },
    { key: "popup_logins", title: "Logins", help: "Show login requests.", fallback: true },
    { key: "popup_questions", title: "Questions", help: "Show agents' questions.", fallback: true },
    { key: "popup_dnd", title: "Show During Do Not Disturb", help: "Let request pop-ups appear during Do Not Disturb.", fallback: false },
    { key: "popup_desktop_notifications", title: "Also Send Desktop Notifications", help: "Send requests to desktop notifications too.", fallback: false }
  ]
  readonly property var requestSettingsSections: [{ id: "request_popups", title: "Request Pop-ups", settings: requestSettingDefinitions.map(function(d) {
    return { key: d.key, title: d.title, help: d.help, type: "bool", scope: "console", value: root.requestBool(d.key), "default": d.fallback }
  }) }]
  function isRequestSetting(key) { return requestSettingDefinitions.some(function(d) { return d.key === key }) }
  function requestBool(key) {
    for (var i = 0; i < requestSettingDefinitions.length; i++) if (requestSettingDefinitions[i].key === key) {
      var v = setting(key, requestSettingDefinitions[i].fallback)
      return v === true || v === "true"
    }
    return false
  }
  function changeRequestSetting(key, value, undoing) {
    var definition = requestSettingDefinitions.filter(function(d) { return d.key === key })[0]
    if (!definition || !shell) return false
    var before = requestBool(key), on = value === true || value === "true"
    if (before === on) return true
    var next = Object.assign({}, settings)
    next[key] = on
    if (!shell.updateEntryInline("io.zet.ibara", next)) {
      setSettingError("", key, "ibara couldn't save this setting.")
      return false
    }
    applySettings(next)
    setSettingError("", key, "")
    if (!undoing) settingsApplied("", [{ key: key, title: definition.title, before: String(before), after: String(on) }], definition.title + " is now " + (on ? "On." : "Off."))
    return true
  }
  function resetRequestSettings() {
    if (!shell) return false
    var next = Object.assign({}, settings), changes = []
    requestSettingDefinitions.forEach(function(d) {
      var before = root.requestBool(d.key)
      if (before !== d.fallback) changes.push({ key: d.key, title: d.title, before: String(before), after: String(d.fallback) })
      next[d.key] = d.fallback
    })
    if (!changes.length) return true
    if (!shell.updateEntryInline("io.zet.ibara", next)) { actionError = "ibara couldn't reset Request Pop-ups."; return false }
    applySettings(next)
    settingsApplied("", changes, "Request Pop-ups settings are back to their defaults.")
    return true
  }
  property var hiddenRequestPopups: ({})
  property bool requestDismissalsReady: false
  onRequestDismissalsReadyChanged: Qt.callLater(root.republishWaiting)
  onHiddenRequestPopupsChanged: Qt.callLater(root.republishWaiting)
  FileView {
    id: requestDismissals
    path: standalone ? (Quickshell.env("XDG_STATE_HOME") || Quickshell.env("HOME") + "/.local/state") + "/ibara/request-dismissals.json" : Quickshell.env("HOME") + "/.local/state/omarchy/ibara-request-dismissals.json"
    atomicWrites: true
    printErrors: false
    onLoaded: {
      if (root.requestDismissalsReady) return
      try {
        var saved = JSON.parse(text())
        if (saved && typeof saved === "object" && !Array.isArray(saved)) root.hiddenRequestPopups = saved
      } catch (e) {}
      root.requestDismissalsReady = true
    }
    onLoadFailed: root.requestDismissalsReady = true
  }
  function requestNeedsAlert(item) {
    if (!requestDismissalsReady || !item) return false
    var hidden = hiddenRequestPopups[item.ref]
    return (!hidden || hidden.signature !== requestAlertSignature(item)) && (item.kind !== "login" || loginNeedsNotification(item))
  }
  function requestAlertSignature(item) {
    var assistance = item && item.login && item.login.assistance
    // A new signup proposal is a new decision. Poll/retry revisions are not.
    return assistance && assistance.signup && assistance.signup.state === "proposed" ? JSON.stringify(assistance.signup) : "request"
  }
  function hideRequestPopup(ref) {
    var next = Object.assign({}, hiddenRequestPopups)
    var item = requestItems.filter(function(item) { return item.ref === ref })[0]
    next[ref] = { at: Date.now(), signature: requestAlertSignature(item) }
    var refs = Object.keys(next).sort(function(a,b) { return Number(next[b].at) - Number(next[a].at) })
    refs.slice(512).forEach(function(key) { delete next[key] })
    hiddenRequestPopups = next
    requestDismissals.setText(JSON.stringify(next))
    closeStaleApprovalNotices()
  }
  function openRequest(item) {
    if (shell && item) shell.summon("io.zet.ibara", JSON.stringify({ route: "computer", computerId: item.computer_id, focus: "request", ref: item.ref }))
  }
  function openRequestSettings() { if (shell) shell.summon("io.zet.ibara", JSON.stringify({ route: "logins" })) }
  readonly property var requestItems: approvals.concat(logins, questions).sort(function(a, b) { return a.at - b.at || a.ref.localeCompare(b.ref) })
  readonly property var popupRequests: requestDismissalsReady && !serviceStopped && requestBool("request_popups") && !requestsInConsole && dndReady && (!requestDnd || requestBool("popup_dnd"))
    ? requestItems.filter(function(item) {
        return root.requestNeedsAlert(item) && root.requestKindEnabled(item.kind) && (item.kind !== "login" || (root.loginNeedsNotification(item) && (item.login.assistance || !root.loginSourceElsewhere(item))))
      }) : []
  function requestKindEnabled(kind) { return requestBool(kind === "approval" ? "popup_approvals" : kind === "login" ? "popup_logins" : "popup_questions") }
  // Ordinary bar-widget plugins do not receive the first-party service proxy on stock
  // Omarchy. Read its small persisted DND setting, also shared by notification clones.
  readonly property var notificationService: shell && typeof shell.firstPartyServiceFor === "function" ? shell.firstPartyServiceFor("omarchy.notifications") : null
  property bool persistedDnd: false
  property bool dndRead: false
  readonly property bool dndReady: !!notificationService || dndRead
  readonly property bool requestDnd: notificationService ? notificationService.doNotDisturb : persistedDnd
  function readRequestDnd() { if (!notificationService && !requestDndReader.running) requestDndReader.running = true }
  Process {
    id: requestDndReader
    command: root.standalone ? ["gsettings", "get", "org.gnome.desktop.notifications", "show-banners"] : ["head", "-c", "8193", "--", Quickshell.env("HOME") + "/.local/state/omarchy/notifications.json"]
    stdout: StdioCollector {
      onStreamFinished: {
        try { root.persistedDnd = root.standalone ? text.trim() !== "true" : text.length > 8192 || JSON.parse(text).dnd === true }
        catch (error) { root.persistedDnd = text.trim() !== "" }
        root.dndRead = true
      }
    }
    onExited: { root.dndRead = true; Qt.callLater(root.syncRequestNotices) }
  }
  Timer {
    interval: 1000
    repeat: true
    running: root.requestItems.length > 0 && !root.notificationService
    triggeredOnStart: true
    onTriggered: root.readRequestDnd()
  }
  Loader {
    active: !root.standalone
    source: active ? Qt.resolvedUrl("RequestPopups.qml") : ""
    onLoaded: item.service = root
  }
  Loader {
    active: !root.standalone
    source: active ? Qt.resolvedUrl("HandBackPills.qml") : ""
    onLoaded: item.service = root
  }
  onRequestsInConsoleChanged: Qt.callLater(root.syncRequestNotices)
  property bool hadRequestItems: false
  onRequestItemsChanged: {
    // DND may have changed while idle. Read it before presenting new requests.
    if (!notificationService && !hadRequestItems && requestItems.length) { dndRead = false; readRequestDnd() }
    hadRequestItems = requestItems.length > 0
    Qt.callLater(root.syncRequestNotices)
  }
  onRequestDndChanged: Qt.callLater(root.syncRequestNotices)
  onSettingsChanged: Qt.callLater(root.syncRequestNotices)
  property bool mutating: false
  readonly property bool reading: activeReads.some(function(request) { return !!request })
  // Four read lanes: a 15-computer fleet bootstraps and polls without starving what the person asked for.
  property var activeReads: [null, null, null, null]
  property var readQueue: []
  property var readErrors: ({})
  property int readSequence: 0
  property int actionSequence: 0
  property int generation: 0
  property int failureCount: 0
  property string lastError: ""
  property string actionError: ""
  property string pendingMutation: ""
  property string actionRequestId: ""
  property string actionNotice: ""
  // ---- first run: the tailnet, adding computers, requests to use this one, and agents.
  // Where ibara's releases are published: the same value as IBARA_BASE_URL in the core's
  // packaging/release.env (packaging/release.sh refuses a release when they differ): the
  // downloads of the newest GitHub release of MayberryDT/ibara.
  readonly property string ibaraBaseUrl: "https://github.com/MayberryDT/ibara/releases/latest/download"
  // The one place the console names ibara's install command.
  readonly property string ibaraInstallCommand: ibaraBaseUrl ? "curl -fsSL " + ibaraBaseUrl + "/install | sh" : ""
  // This plugin can arrive before the rest of ibara (from Omarchy's plugin marketplace).
  // "package": the ibara package is not installed; "setup": it is, but `ibara setup` has not
  // finished; "": ibara is here (or its background service simply isn't answering).
  property string ibaraMissing: ""
  function checkInstalled() { if (!installCheck.running) installCheck.running = true }
  // Install ibara, or finish setting it up, in a terminal on this desktop; the console
  // connects by itself once ibara's service answers (setup also restarts the shell once).
  function installIbara() {
    var command = ibaraMissing === "setup" ? "ibara setup" : ibaraInstallCommand
    if (!command) return false
    if (standalone) { shell.launchTerminal(command); return true }
    installLauncher.command = ["omarchy-launch-floating-terminal-with-presentation", command]
    installLauncher.running = true
    return true
  }
  // ibara is installed and set up here, but its background service isn't answering. Nothing is
  // wrong with the computers, so each keeps its last known state; the console says so in one
  // toast (Start ibara, when this desktop's service manager knows ibara's service, else the one
  // command that starts it) and greys the wall. Set once the install check confirms ibara is here
  // while the connection is down; cleared the moment it connects.
  property bool daemonDown: false
  property bool serviceStopped: false
  property bool canStartIbara: false
  // "", "starting", or "failed": Start ibara ran, and ibara still isn't answering.
  property string startState: ""
  readonly property string ibaraStartCommand: "ibara setup"
  onServiceStoppedChanged: if (serviceStopped && !ibaraUnitCheck.running) ibaraUnitCheck.running = true
  function startIbara() {
    if (!canStartIbara || startState === "starting" || ibaraStarter.running) return false
    startState = "starting"
    ibaraStarter.running = true
    return true
  }
  property var latestRelease: null
  property string loadedPluginVersion: ""
  readonly property bool staleConsole: !!(loadedPluginVersion && latestRelease && latestRelease.installed_version && StatusModel.versionCompare(loadedPluginVersion, latestRelease.installed_version) < 0)
  FileView {
    path: Qt.resolvedUrl("build.json")
    onLoaded: { try { root.loadedPluginVersion = String(JSON.parse(text()).version || "") } catch (e) {} }
  }
  function restartBar() {
    if (standalone) { actionNotice = "Close the Console and reopen ibara to load the installed version."; return }
    Quickshell.execDetached(["omarchy", "restart", "shell"])
  }
  readonly property string updateAvailable: latestRelease && StatusModel.releaseBehind(latestRelease.console_version, latestRelease.version) ? String(latestRelease.version) : ""
  function behind(c) { return !!(c && latestRelease && StatusModel.releaseBehind(c.version, latestRelease.version)) }
  function loadUpdateCheck(refresh) { if (!busy["update-check"]) { setBusy("update-check", "reading"); sideSend(refresh ? ["update-check", "--refresh"] : ["update-check"], { busyKey: "update-check" }) } }
  property var seenIbaraUpdates: ({})
  function noteIbaraUpdate(id, before, after) {
    var u = after && after.ibara_update
    if (!u || u.state === "running") return
    var key = String(u.started_at) + ":" + u.state + ":" + String(u.shell)
    var pending = updateAllRun && updateAllRun.action === "update_ibara" && updateAllRun.waiting.indexOf(id) !== -1
    if (!consoleOpen && !pending) return
    if (seenIbaraUpdates[id] === key) return
    var recent = u.finished_at > Date.now() - 300000
    var next = Object.assign({}, seenIbaraUpdates); next[id] = key; seenIbaraUpdates = next
    if (!recent && !pending) return
    if (pending && updateAllRun.baseline[id] === key) return
    var text = u.state === "failed" ? "Update failed on " + computerLabelFor(id) + ": " + (u.message || "The update stopped before it finished.")
      : "ibara " + u.version + " on " + computerLabelFor(id) + (u.shell === "deferred_locked" ? ". Its bar restarts after unlock." : "")
    powerSettled(id, "update_ibara", text, u.state === "failed")
  }
  Timer { interval: 60000; running: true; repeat: true; triggeredOnStart: true; onTriggered: root.loadUpdateCheck(false) }
  // ---- What's New: the notes of the ibara update installed last, shown once.
  property var whatsNew: null
  function loadWhatsNew() { if (!busy["whats-new"]) { setBusy("whats-new", "reading"); sideSend(["whats-new"], { busyKey: "whats-new" }) } }
  function dismissWhatsNew() { whatsNew = null; sideSend(["whats-new-seen"], {}) }
  // ---- Start without the disk password (`unattended-boot`): this computer's own switch on the
  // Settings page. Changing it needs root and the disk password, so it runs `ibara unattended-boot`
  // in a terminal (sudo asks once), and the switch follows once the change has happened.
  property var unattendedBootSections: []
  // The value asked for in the terminal, until the answer shows it (or ten minutes pass).
  property var unattendedBootWanted: null
  property double unattendedBootAskedAt: 0
  function loadUnattendedBoot() { if (!busy["unattended-boot"]) { setBusy("unattended-boot", "reading"); sideSend(["unattended-boot"], { busyKey: "unattended-boot" }) } }
  function setUnattendedBoot(on) {
    var setting = StatusModel.findSetting(unattendedBootSections, "unattended_boot")
    if (!setting || unattendedBootWanted !== null) return false
    var command = "ibara unattended-boot " + (on ? "enable" : "disable")
    if (on && !setting.available) { setSettingError("", "unattended_boot", "This computer can't start without its disk password; the line above says why."); return false }
    setSettingError("", "unattended_boot", "")
    unattendedBootWanted = on
    unattendedBootAskedAt = Date.now()
    setBusy("setting::unattended_boot", "terminal")
    unattendedLauncher.command = ["omarchy-launch-floating-terminal-with-presentation", command]
    unattendedLauncher.running = true
    return true
  }
  function settleUnattendedBoot(message) {
    unattendedBootWanted = null
    setBusy("setting::unattended_boot", "")
    if (message) setSettingError("", "unattended_boot", message)
  }
  // Lock the screen at sign-in: a file in this account's Omarchy hooks, so the console changes it
  // at once (`unattended-boot-lock on|off`) and the answer is the new state of both switches.
  function setSignInLock(on) {
    var setting = StatusModel.findSetting(unattendedBootSections, "lock_at_sign_in")
    if (!setting || busy["setting::lock_at_sign_in"]) return false
    setSettingError("", "lock_at_sign_in", "")
    setBusy("setting::lock_at_sign_in", "saving")
    sideSend(["unattended-boot-lock", on ? "on" : "off"], { busyKey: "setting::lock_at_sign_in" })
    return true
  }
  // This Computer back to its defaults: starting without the password goes first, since the lock
  // stays while it is on.
  function resetThisComputer() {
    var boot = StatusModel.findSetting(unattendedBootSections, "unattended_boot"), lock = StatusModel.findSetting(unattendedBootSections, "lock_at_sign_in")
    if (boot && boot.value) return setUnattendedBoot(false)
    return lock && lock.value ? setSignInLock(false) : false
  }
  // Omarchy's own Tailscale install, the one its menu runs (Install › Service › Tailscale).
  readonly property string tailscaleInstallCommand: "omarchy-install-service-tailscale"
  // The directory has answered at least once, so an empty fleet is really empty.
  property bool directoryLoaded: false
  // The last tailnet read (StatusModel.tailnetView), null until one arrives.
  property var tailnet: null
  // Adds started here, by tailnet node: { node, requestId, code, mode, state, message, computerId,
  // label, startedAt }. state: starting, waiting, canceling, paired, declined, expired, failed.
  property var pairings: ({})
  property var pairStatusAsked: ({})
  readonly property bool pairingActive: {
    for (var node in pairings) if (["starting", "waiting", "canceling"].indexOf(pairings[node].state) !== -1) return true
    return false
  }
  signal pairingAdded(string node, string label)
  // Computers that joined the fleet after the console's first read of it, by id: when (ms). The
  // first picture of one in view plays its arrival once, within two minutes (takeArrival).
  property var arrivals: ({})
  function takeArrival(computerId) {
    var at = arrivals[computerId]
    if (!at) return false
    var next = Object.assign({}, arrivals)
    delete next[computerId]
    arrivals = next
    return Date.now() - at < 120000
  }
  // Requests from other computers to use this one, waiting for a person here. One just accepted
  // stays for 2.5 s marked `accepted`, so its card can show the match, whatever a read says.
  property var pairRequests: []
  property var pairAnswers: ({})
  property var notifiedPairRequests: ({})
  function keepAccepted(list) {
    var held = pairRequests.filter(function(request) { return request.accepted })
    if (!held.length) return list
    var ids = held.map(function(request) { return request.request_id })
    return list.filter(function(request) { return ids.indexOf(request.request_id) === -1 }).concat(held)
  }
  Timer {
    id: acceptedLinger
    interval: 2500
    onTriggered: root.pairRequests = root.pairRequests.filter(function(request) { return !request.accepted })
  }
  // The prompt that connects an agent (from ibara, `connect-prompt`), and whether any agent has
  // begun a task through this computer yet.
  property string connectPrompt: ""
  property bool firstTaskDone: false
  property int sideSequence: 0
  readonly property bool tailscaleSigningIn: tailscaleUp.running
  property int accessGeneration: 0
  property int actionAccessGeneration: 0
  property var actionScope: null
  property double nowMs: Date.now()
  property var pendingNotices: []

  // Desktop notices go through the console's Notifications setting.
  function queueNotice(notice) {
    if (!consoleBool("notifications", true)) return
    pendingNotices = pendingNotices.concat([notice])
    drainNotices()
  }
  function drainNotices() {
    if (notifyProcess.running || !pendingNotices.length) return
    var queue = pendingNotices.slice()
    var notice = queue.shift()
    pendingNotices = queue
    if (notice.computerId) {
      var current = sessions[notice.computerId]
      if (denied || !current || current.connection === "unauthorized" || current.trust_state !== "verified" || current.endpoint_id !== notice.endpointId ||
          current.binding_revision !== notice.bindingRevision || current.authorization_generation !== notice.authorizationGeneration ||
          !notice.epoch || current.controller_epoch !== notice.epoch) {
        logDroppedFollowNotice(notice, "target-binding-or-policy")
        Qt.callLater(root.drainNotices)
        return
      }
    }
    if (notice.trace)
      console.info(root.notificationTracePrefix("notice-dispatch") +
        " computer=" + root.notificationTraceToken(notice.trace.computerId) +
        " task_ref=" + root.notificationTraceToken(notice.trace.taskRef) +
        " state=" + root.notificationTraceState(notice.trace.state) + " verified=" + (notice.trace.verified === true ? "true" : "false") +
        " notice=" + root.notificationTraceNotice(notice.trace.notice))
    notifyProcess.command = ["notify-send", "-a", "ibara", notice.title, notice.body]
    notifyProcess.running = true
  }

  // ibarad (the operator role) answers every console command over one Unix socket. A request
  // carries the command's arguments and an id; the reply is a version-2 envelope that every
  // handler below parses with StatusModel.parseEnvelope. Previews come back as events.
  readonly property string daemonSocketPath: {
    var dir = String(Quickshell.env("XDG_RUNTIME_DIR") || "")
    return dir ? dir + "/ibara/ibarad.sock" : ""
  }
  readonly property string daemonAbsentMessage: "ibara isn't running on this computer."
  // The live connection. Quickshell 0.3 keeps a failed socket and never retries it, so each
  // attempt uses a fresh Socket from daemonSocketComponent.
  property var daemonSocket: null
  property bool daemonConnecting: false
  property int daemonRetryMs: 500
  // Requests awaiting an answer, by id: which handler takes it and whether it was written.
  property var daemonPending: ({})
  // Lines written once the connection opens, and failures handed to their handlers later.
  property var daemonOutbox: []
  property var daemonFailures: []
  readonly property string pluginId: "io.zet.ibara"
  readonly property int openRefreshMs: intSetting("openRefreshSec", 5, 2, 60) * 1000
  readonly property int closedRefreshMs: intSetting("closedRefreshSec", 15, 5, 120) * 1000
  // The controller reports holds_control only to the verified holder. Trust it only for the
  // epoch that reported it and while the session is ready; a restart, rebinding or denial clears it.
  function holdsControlOn(computerId) {
    var s = sessions[String(computerId || "")]
    return !!s && s.holds_control === true && (s.screen_engine !== "ibara" || s.turn === "person") && s.connection === "ready" && !!s.controller_epoch && s.holds_control_epoch === s.controller_epoch
  }
  function turnWords(computerId) {
    var s = sessions[String(computerId || "")]
    if (!s || !s.turn) return ""
    if (s.turn === "person") return holdsControlOn(computerId) ? "Your Turn" : String(s.turn_holder || "A person") + "’s Turn"
    var state = StatusModel.computerState(s)
    return state === "working" ? "Agent working" : state === "ready" ? "Ready" : state === "paused" ? "Agents paused" : ""
  }
  readonly property var heldComputerIds: Object.keys(sessions).filter(function(id) { return root.holdsControlOn(id) }).sort()
  readonly property bool denied: connectionState === "unauthorized"

  function localPath(url) {
    var value = String(url || "")
    if (value.indexOf("file://") === 0) value = value.substring(7)
    try { return decodeURIComponent(value) } catch (error) { return value }
  }

  function setting(name, fallback) {
    var value = settings ? settings[name] : undefined
    return value === undefined || value === null ? fallback : value
  }

  function saveFleetOrder(mode, ids) {
    if ((mode !== "name" && mode !== "attention") || !Array.isArray(ids) || !shell) return false
    if (setting("fleet_sort", "name") === mode && JSON.stringify(setting("fleet_order", [])) === JSON.stringify(ids)) return true
    var next = Object.assign({}, settings, { fleet_sort: mode, fleet_order: ids })
    if (!shell.updateEntryInline("io.zet.ibara", next)) {
      actionError = "ibara couldn't save the computer order."
      return false
    }
    // This changes presentation only; do not tear down preview lanes.
    settings = next
    return true
  }

  function intSetting(name, fallback, minimum, maximum) {
    var number = parseInt(String(setting(name, fallback)), 10)
    if (!isFinite(number)) number = fallback
    return Math.max(minimum, Math.min(maximum, number))
  }

  function applySettings(value) {
    settings = value || ({})
    releasePreviewLanes()
    schedulePoll(panelOpen ? openRefreshMs : closedRefreshMs)
  }

  function setPanelOpen(value) {
    setSurfaceOpen("quick", value)
  }

  function setSurfaceOpen(surface, value) {
    if (surface === "quick") quickOpen = !!value
    else if (surface === "console") consoleOpen = !!value
    var wasOpen = panelOpen
    panelOpen = quickOpen || consoleOpen
    if (panelOpen && !wasOpen) refresh()
    else schedulePoll(panelOpen ? openRefreshMs : closedRefreshMs)
    if (panelOpen) { Qt.callLater(root.refreshComputerSessions); loadAttention() }
    else closePreviews()
    // Opening the console asks what happened while you were away and reads its settings again.
    if (surface === "console" && consoleOpen) { loadAway(); loadConsoleSettings(); loadWhatsNew(); loadLoginSettings(); loadUpdateCheck(false) }
    if (surface === "console" && consoleOpen && scopedComputerId) {
      if (!tasksLoaded && !readPending("tasks")) loadTasks()
      if (!artifactsLoaded && !readPending("artifacts")) loadArtifacts()
    }
  }

  // The request for one command line: its command, then its arguments as strings.
  function daemonRequest(id, args) {
    return { id: String(id), command: String(args[0] || ""), args: args.slice(1).map(function(value) { return String(value) }) }
  }
  // route.kind names the handler: action, read (with its lane), pick, preview or side.
  function daemonSend(id, args, route) {
    var request = daemonRequest(id, args)
    var connected = !!daemonSocket && daemonSocket.connected
    var pending = Object.assign({}, daemonPending)
    pending[request.id] = Object.assign({}, route, { command: request.command, sent: connected })
    daemonPending = pending
    var line = JSON.stringify(request) + "\n"
    if (connected) {
      daemonSocket.write(line)
      daemonSocket.flush()
      return
    }
    daemonOutbox = daemonOutbox.concat([{ id: request.id, line: line }])
    connectDaemon()
  }
  function forgetDaemonRequest(id) {
    if (!daemonPending[id]) return
    var pending = Object.assign({}, daemonPending)
    delete pending[id]
    daemonPending = pending
  }
  function connectDaemon() {
    if (daemonConnecting || (daemonSocket && daemonSocket.connected)) return
    daemonReconnect.stop()
    if (!daemonSocketPath) { failDaemonRequests(); return }
    if (daemonSocket) daemonSocket.destroy()
    daemonConnecting = true
    daemonSocket = daemonSocketComponent.createObject(root)
    daemonSocket.connected = true
  }
  function scheduleDaemonReconnect() {
    daemonReconnect.interval = daemonRetryMs
    daemonReconnect.restart()
    daemonRetryMs = Math.min(30000, daemonRetryMs * 2)
  }
  function daemonConnectionChanged(socket) {
    if (socket !== daemonSocket) return
    daemonConnecting = false
    if (!socket.connected) {
      failDaemonRequests()
      scheduleDaemonReconnect()
      noteDaemonDown()
      return
    }
    daemonRetryMs = 500
    var outbox = daemonOutbox, pending = Object.assign({}, daemonPending)
    daemonOutbox = []
    for (var i = 0; i < outbox.length; i++) {
      if (!pending[outbox[i].id]) continue
      pending[outbox[i].id] = Object.assign({}, pending[outbox[i].id], { sent: true })
      socket.write(outbox[i].line)
    }
    daemonPending = pending
    socket.flush()
    // Back after it stopped: read everything again now rather than at the next backed-off poll.
    var wasDown = daemonDown
    daemonDown = false
    serviceStopped = false
    startState = ""
    if (wasDown) Qt.callLater(function() { root.failureCount = 0; root.refresh(); root.loadAttention() })
  }
  function daemonConnectFailed(socket) {
    if (socket !== daemonSocket || socket.connected) return
    daemonConnecting = false
    failDaemonRequests()
    scheduleDaemonReconnect()
    noteDaemonDown()
  }
  // Nothing answers on ibara's socket: the install check says whether ibara is missing or stopped.
  function noteDaemonDown() {
    daemonDown = true
    if (!serviceStopped) checkInstalled()
  }
  // Every waiting request fails with a plain reason, as a lost reply always did. Handlers
  // run later, never inside the call that sent the request.
  function failDaemonRequests() {
    var pending = daemonPending, failures = daemonFailures.slice()
    daemonPending = ({})
    daemonOutbox = []
    for (var id in pending) failures.push({ id: id, route: pending[id] })
    daemonFailures = failures
    if (failures.length) Qt.callLater(root.flushDaemonFailures)
  }
  function flushDaemonFailures() {
    var failures = daemonFailures
    daemonFailures = []
    for (var i = 0; i < failures.length; i++)
      deliverDaemon(failures[i].route, failures[i].id, daemonFailure(failures[i].id, failures[i].route))
  }
  // A request that was never written did nothing; one written before the connection closed
  // has an unknown outcome, so an action is not safe to repeat.
  function daemonFailure(id, route) {
    var sent = route.sent === true
    var message = sent ? "ibara lost its connection to its background service before an answer came." : daemonAbsentMessage
    if (route.kind === "preview") message += " ibara will try again."
    else if (route.kind === "action") message += sent ? " It can't tell whether this finished; the computer's header and Activity tab show what changed." : " Nothing was done."
    return { version: 2, request_id: id, command: route.command, target: "ibara",
      observed_at: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"), connection: sent ? "offline" : "missing-dependency",
      desktop: null, owner: {}, capabilities: {}, stale: false, data: null,
      error: { code: "DAEMON_UNAVAILABLE", message: message, retry_safe: !(sent && route.kind === "action") } }
  }
  function deliverDaemon(route, id, value) {
    if (route.kind === "warm") return
    if (route.kind === "action") consumeAction(value)
    else if (route.kind === "read") consumeRead(route.lane, value)
    else if (route.kind === "pick") consumePick(value)
    else if (route.kind === "side") consumeSide(route, value)
    else if (route.kind === "preview") {
      for (var lane = 0; lane < previewActive.length; lane++)
        if (previewActive[lane] && previewActive[lane].request_id === id) { consumePreview(lane, value); return }
    }
  }
  // One line from ibarad: a reply {id, envelope} or a preview event {event, computer_id, data}.
  function consumeDaemonLine(line) {
    var message
    try { message = JSON.parse(line) } catch (error) { return }
    if (!message || typeof message !== "object") return
    var preview = message.event === "preview"
    var envelope = preview ? message.data : message.envelope
    var id = preview ? String(envelope && envelope.request_id || "") : String(message.id || "")
    var route = daemonPending[id]
    // A picture that lands after the console closed was written anyway; ibarad deletes it
    // with every other file no session still shows.
    if (!route) { if (preview && !panelOpen) releasePreviewFiles(); return }
    forgetDaemonRequest(id)
    // An oversized answer fails as it always did: its text is over the handler's parse limit.
    var limit = (route.kind === "preview" ? 2 * 1024 * 1024 : 512 * 1024) + 1024
    deliverDaemon(route, id, String(line).length > limit ? line : envelope)
  }

  // One action at a time on the action lane. `scope` ({id, generation}) marks an action for one
  // computer: its answer reports only on that computer.
  function startHelper(kind, args, scope) {
    if (mutating) return
    mutating = true
    pendingMutation = kind
    actionErrorFix = null
    actionError = ""
    actionNotice = ""
    actionSequence += 1
    actionRequestId = "mut-" + generation + "-" + actionSequence + "-" + kind
    actionAccessGeneration = accessGeneration
    actionScope = scope || null
    daemonSend(actionRequestId, args, { kind: "action" })
  }
  // An action for one computer over its pairing route, on the action lane.
  function startComputerAction(kind, command, computerId, rest) {
    var id = String(computerId || ""), args = computerArgs(command, id, rest)
    if (mutating) { actionError = "Wait for the current action to finish."; return false }
    if (!args) { actionError = "ibara is still connecting to " + computerLabelFor(id) + ". Try again in a moment."; return false }
    startHelper(kind, args, { id: id, generation: scopeGeneration })
    return pendingMutation === kind
  }

  function readPending(kind) {
    for (var active = 0; active < activeReads.length; active++)
      if (activeReads[active] && activeReads[active].kind === kind) return true
    for (var i = 0; i < readQueue.length; i++) if (readQueue[i].kind === kind) return true
    return false
  }

  function requestRead(kind, args, ref, identitySnapshot, scope) {
    if (denied) return
    var errors = Object.assign({}, readErrors)
    // Keep a task refresh error readable until the next successful answer.
    if (kind !== "task" || String(ref || "") !== selectedTaskDetailRef) delete errors[kind]
    readErrors = errors
    var queue = readQueue.slice()
    for (var i = queue.length - 1; i >= 0; i--) if (queue[i].kind === kind) queue.splice(i, 1)
    var request = { kind: kind, args: args, ref: String(ref || ""), access: accessGeneration, scope: scope || null }
    request.scoped = !!request.scope
    if (String(kind).indexOf("follow-task:") === 0 && identitySnapshot)
      request.followSnapshot = Object.assign({}, identitySnapshot)
    if (String(kind).indexOf("session-status:") === 0 && identitySnapshot) {
      request.sessionSnapshot = Object.assign({}, identitySnapshot, {
        followSnapshot: identitySnapshot.followSnapshot ? Object.assign({}, identitySnapshot.followSnapshot) : null
      })
    }
    // What the person asked for goes ahead of background session polling.
    var background = /^(session-status|session-bootstrap|follow-task):/
    var at = queue.length
    if (!background.test(String(kind))) while (at > 0 && background.test(String(queue[at - 1].kind))) at--
    queue.splice(at, 0, request)
    readQueue = queue
    drainReads()
  }

  function drainReads() {
    if (!readQueue.length || denied) return
    var lane = -1
    for (var i = 0; i < activeReads.length; i++) if (!activeReads[i]) { lane = i; break }
    if (lane < 0) return
    var queue = readQueue.slice()
    var request = queue.shift()
    readQueue = queue
    // A release still waiting when the console reopens could delete a fresh frame; the next close sends one.
    if (request.access !== accessGeneration || (request.kind === "preview-release" && panelOpen)) { Qt.callLater(root.drainReads); return }
    readSequence += 1
    request.requestId = "read-" + readSequence + "-" + request.kind
    var active = activeReads.slice()
    active[lane] = request
    activeReads = active
    daemonSend(request.requestId, request.args, { kind: "read", lane: lane })
    Qt.callLater(root.drainReads)
  }

  // Every refresh reads the directory and the requests to use this computer, then schedules the next.
  function refresh() {
    var cleared = Object.assign({}, sessions)
    for (var id in cleared) if (cleared[id] && cleared[id].active_task_ref)
      cleared[id] = Object.assign({}, cleared[id], { active_task_ref: "" })
    publishSessions(cleared)
    loadComputers()
    // Requests to use this computer are checked on every refresh, open or closed, so the
    // person here hears about one while the other screen still shows its code.
    loadPairRequests()
    pollClosedStatus()
    schedulePoll(failureCount ? Math.min(60000, closedRefreshMs * failureCount) : panelOpen ? openRefreshMs : closedRefreshMs)
  }
  // The operator's own name for a verified computer, kept in this machine's private directory
  // (the computer itself never learns it); the directory is re-read once the name is saved.
  function renameComputer(computerId, label) {
    var id = String(computerId || ""), name = String(label || "").trim()
    if (!sessions[id] || sessions[id].trust_state !== "verified" || mutating) return false
    if (!name || name.length > 128 || /[\u0000-\u001f\u007f-\u009f\u2028\u2029]/.test(name)) {
      actionError = "A computer name needs 1–128 characters on one line."
      return false
    }
    startHelper("rename-computer", ["rename-computer", "--computer", id, "--label", name])
    return true
  }
  // Take a computer out of this machine's fleet: its directory row, its pinned host key and what
  // ibara held for it here. The computer itself isn't asked; Add Computer adds it again, with the
  // same checks as the first time (a reinstalled computer, say, that no longer answers as itself).
  function removeComputer(computerId) {
    var id = String(computerId || "")
    if (!sessions[id] || mutating) return false
    startHelper("remove-computer", ["remove-computer", "--computer", id])
    return pendingMutation === "remove-computer"
  }

  function loadComputers() { requestRead("directory", ["directory"]) }
  // Computers whose status stays fresh. The console's wall and list publish what is on screen
  // (empty when hidden). The quick panel shows no pictures, only states, so while it alone is
  // open every listed computer keeps its status current without any preview.
  function statusComputerIds() {
    if (consoleOpen) return visibleComputerIds
    if (!quickOpen) return []
    return computers.map(function(item) { return item.computer_id })
  }
  // Failed session bootstraps and denied-status re-checks retry per computer with
  // jittered exponential backoff (5 s → 60 s), so refusing targets cannot fill the read lanes.
  property var sessionRetry: ({})
  function deferSessionRetry(computerId) {
    var retry = Object.assign({}, sessionRetry), prior = retry[computerId]
    var delay = Math.min(60000, prior ? prior.delay * 2 : 5000)
    retry[computerId] = { delay: delay, until: Date.now() + Math.round(delay * (0.8 + Math.random() * 0.4)) }
    sessionRetry = retry
  }
  function clearRetryState(computerId) {
    if (sessionRetry[computerId]) { var retry = Object.assign({}, sessionRetry); delete retry[computerId]; sessionRetry = retry }
    if (previewBackoff[computerId]) { var backoff = Object.assign({}, previewBackoff); delete backoff[computerId]; previewBackoff = backoff }
  }
  function refreshComputerSessions() {
    // Nothing open: the bar's own polling (right after the directory answers, then each refresh).
    if (!panelOpen) { pollClosedStatus(); return }
    var wanted = statusComputerIds().slice()
    if (selectedComputerId && wanted.indexOf(selectedComputerId) === -1) wanted.push(selectedComputerId)
    var now = Date.now()
    for (var i = 0; i < wanted.length; i++) {
      var computer = sessions[wanted[i]]
      if (!computer || computer.trust_state !== "verified" || (sessionRetry[computer.computer_id] && now < sessionRetry[computer.computer_id].until)) continue
      if (!computer.controller_epoch && !readPending("session-bootstrap:" + computer.computer_id))
        requestRead("session-bootstrap:" + computer.computer_id, ["operator-session", "--computer", computer.computer_id], computer.computer_id)
      // A denied card stays blank until an authenticated status says access returned.
      else if (computer.controller_epoch && computer.connection === "unauthorized" && !readPending("session-status:" + computer.computer_id))
        requestRead("session-status:" + computer.computer_id, ["operator-status", "--computer", computer.computer_id, "--epoch", computer.controller_epoch],
          computer.computer_id, sessionStatusSnapshot(computer.computer_id))
    }
  }
  function selectComputer(computerId) {
    var id = String(computerId || "")
    for (var i = 0; i < computers.length; i++) {
      if (computers[i].computer_id === id) {
        if (selectedComputerId !== id) {
          selectedComputerId = id
          previewQueue = previewQueue.filter(function(item) { return item.quality !== "selected" })
        }
        Qt.callLater(root.refreshComputerSessions)
        return true
      }
    }
    return false
  }
  function clearSelectedComputer() {
    selectedComputerId = ""
    previewQueue = previewQueue.filter(function(item) { return item.quality !== "selected" })
  }
  function setComputerDisplay(computerId, displayId) {
    var session = sessions[String(computerId || "")]
    if (!session || session.trust_state !== "verified" || !/^[A-Za-z0-9_.:-]{1,128}$/.test(String(displayId || ""))) return false
    var updated = Object.assign({}, sessions)
    updated[computerId] = Object.assign({}, session, { display_id: String(displayId), frame: null, frame_error: "Waiting for an authorized preview." })
    publishSessions(updated)
    previewGeneration += 1
    previewQueue = []
    return true
  }
  function takeControlFor(computerId, force) {
    if (!selectComputer(computerId)) return false
    var s = sessions[computerId]
    if (holdsControlOn(computerId)) { actionError = "You already have control of this computer."; return false }
    if (!s || s.interactive_control !== "available_if_exclusive" || !s.controller_epoch ||
        !s.owner_name || !s.ownership_revision || mutating) {
      actionError = mutating ? "Wait for the current action to finish." : "ibara is still reading who is using " + computerLabelFor(computerId) + ". Choose Join again in a moment."
      return false
    }
    pendingControl = { computer_id: computerId, endpoint_id: s.endpoint_id,
      binding_revision: s.binding_revision, controller_epoch: s.controller_epoch,
      authorization_generation: s.authorization_generation }
    startHelper("operator-take-control", ["operator-control", "--computer", computerId, "--epoch", s.controller_epoch,
      "--op", (s.screen_engine === "ibara" || s.screen_engine === "unavailable") && force !== true ? "join" : "take_control", "--owner", s.owner_name, "--revision", s.ownership_revision])
    return true
  }
  function connectingOn(computerId) {
    return (!!pendingControl && pendingMutation === "operator-take-control" && pendingControl.computer_id === computerId) ||
      (pendingMutation === "open-viewer" && pendingViewerComputerId === computerId) || !!viewerConnecting[computerId]
  }
  function controlBlockedReason(computerId) {
    var s = sessions[String(computerId || "")]
    if (!s) return "This computer is not available."
    if (connectingOn(computerId)) return "Connecting…"
    if (denied || s.connection === "unauthorized") return "You don't have access."
    if (s.trust_state !== "verified") return "Pair this computer first."
    if (s.connection !== "ready") return "This computer isn't answering."
    if (s.interactive_control !== "available_if_exclusive") return "Join isn't available."
    if (!s.controller_epoch || !s.owner_name || !s.ownership_revision) return "Still checking who has control."
    if (mutating) return "Wait for the current action to finish."
    return ""
  }
  function requestTakeControl(computerId, confirm) {
    var reason = controlBlockedReason(computerId), s = sessions[computerId]
    if (reason) { if (!connectingOn(computerId)) actionError = reason; return false }
    if (holdsControlOn(computerId)) return viewerOpenOn(computerId) || openViewerFor(computerId)
    if (s.screen_engine === "ibara" && viewerOpenOn(computerId)) {
      actionNotice = computerLabelFor(computerId) + ": already watching."
      return true
    }
    // Only replacing another person asks first. The daemon still checks owner and revision.
    if (s.screen_engine !== "ibara" && /^operator:/.test(String(s.owner_name))) {
      var owner = s.owner_name, revision = s.ownership_revision, epoch = s.controller_epoch
      var endpoint = s.endpoint_id, binding = s.binding_revision, authorization = s.authorization_generation
      if (typeof confirm !== "function") return false
      confirm({ message: "Another person has control of " + computerLabelFor(computerId) + ". Take Over?",
        confirmLabel: "Join", subject: computerId,
        valid: function() { var now = root.sessions[computerId]; return !!now && now.controller_epoch === epoch && now.endpoint_id === endpoint && now.binding_revision === binding && now.authorization_generation === authorization && now.owner_name === owner && now.ownership_revision === revision },
        run: function() { root.takeControlFor(computerId) } })
      return true
    }
    return takeControlFor(computerId)
  }
  // Best effort on its own lane: older cores reject warm without a toast or a mutation lock.
  property var warmAt: ({})
  property int warmSequence: 0
  function warmComputer(computerId) {
    var id = String(computerId || ""), s = sessions[id], now = Date.now()
    if (!s || s.connection !== "ready" || s.trust_state !== "verified" || !s.controller_epoch || denied || daemonDown) return false
    if (warmAt[id] && now - warmAt[id] < 30000) return true
    var next = Object.assign({}, warmAt); next[id] = now; warmAt = next
    daemonSend("warm-" + generation + "-" + (++warmSequence),
      ["operator-control", "--computer", id, "--epoch", s.controller_epoch, "--op", "warm"], { kind: "warm" })
    return true
  }
  function handBackFor(computerId) {
    if (!selectComputer(computerId)) return false
    var s = sessions[computerId]
    if (!s || s.interactive_control !== "available_if_exclusive" || !s.controller_epoch || !holdsControlOn(computerId) ||
        !/^operator:/.test(String(s.owner_name || "")) || !s.ownership_revision || mutating) {
      actionError = mutating ? "Wait for the current action to finish." : "You don't have control of " + computerLabelFor(computerId) + ", so there is nothing to hand back."
      return false
    }
    pendingControl = { computer_id: computerId, endpoint_id: s.endpoint_id,
      binding_revision: s.binding_revision, controller_epoch: s.controller_epoch,
      authorization_generation: s.authorization_generation }
    startHelper("operator-handback", ["operator-control", "--computer", computerId, "--epoch", s.controller_epoch,
      "--op", "handback", "--owner", s.owner_name, "--revision", s.ownership_revision])
    return true
  }
  // A restarted controller has a new epoch; bootstrap the session again so
  // interactive control reflects the new controller.
  function forgetControllerEpoch(computerId) {
    var current = sessions[computerId]
    if (!current) return
    clearTargetEpoch(computerId, current.controller_epoch)
    var updated = Object.assign({}, sessions)
    updated[computerId] = Object.assign({}, current, { controller_epoch: "", interactive_control: "", holds_control: false })
    publishSessions(updated)
  }
  // Wall cards and quick-panel rows show who holds each computer and its live task, so those
  // computers re-read their authenticated status in rotation, each second (statusTick). One
  // where something moves (an agent working, a person in control, a pause, something that needs
  // you) is read about every second, up to eight per tick, and for 10 s after it settles, so its
  // step line, clicks, approvals and Done arrive at once; the open computer too while it moves.
  // The rest are read at most every 20 s, up to four per tick. One read per computer at a time.
  // Denied and offline cards keep their own backoff paths.
  property var statusPolledAt: ({})
  // computerId → ms until which it is read at the fast pace; not bound.
  property var statusFastUntil: ({})
  readonly property int statusFastMs: 1000
  readonly property int statusSlowMs: 20000
  function statusMoving(s) {
    var state = s && s.fleet_state
    return state === "working" || state === "human" || state === "paused" || state === "attention"
  }
  // Reads one computer's status now and notes when (the open computer's and held ones' 5 s reads too).
  function pollStatus(computerId) {
    var s = sessions[computerId]
    if (!s || s.trust_state !== "verified" || !s.controller_epoch || readPending("session-status:" + computerId)) return false
    var polled = Object.assign({}, statusPolledAt)
    polled[computerId] = Date.now()
    statusPolledAt = polled
    requestRead("session-status:" + computerId, ["operator-status", "--computer", computerId, "--epoch", s.controller_epoch], computerId, sessionStatusSnapshot(computerId))
    return true
  }
  function pollVisibleStatus() {
    if (denied) return
    if (!panelOpen) {
      Object.keys(viewers).concat(heldComputerIds).filter(function(id, i, ids) { return ids.indexOf(id) === i }).forEach(function(id) { if (Date.now() - Number(root.statusPolledAt[id] || 0) >= root.statusFastMs - 100) root.pollStatus(id) })
      return
    }
    var now = Date.now(), ids = statusComputerIds().slice(), fast = [], slow = []
    if (selectedComputerId && ids.indexOf(selectedComputerId) === -1) ids.push(selectedComputerId)
    var anyMoving = false, fresh = []
    for (var i = 0; i < ids.length; i++) {
      var id = ids[i], s = sessions[id]
      if (!s || s.trust_state !== "verified" || !s.controller_epoch ||
          s.connection === "unauthorized" || s.connection === "offline") continue
      if (statusMoving(s)) statusFastUntil[id] = now + 10000
      var quick = now < Number(statusFastUntil[id] || 0)
      // Approvals and questions come quickly while an agent may ask on a computer in view (not
      // paused, not held by a person), and for 10 s after it settles, including once an approval
      // turns its card to Needs Attention. Those where an agent works or something waits are named
      // so ibarad reads them now rather than answer from its last read.
      if (quick && s.fleet_state !== "paused" && s.fleet_state !== "human") anyMoving = true
      if (quick && (s.fleet_state === "working" || s.fleet_state === "attention")) fresh.push(id)
      if (readPending("session-status:" + id)) continue
      var age = now - Number(statusPolledAt[id] || 0)
      // The open computer and the ones held here keep their own 5 s read while nothing moves there.
      if (quick && age >= statusFastMs - 100) fast.push(id)
      else if (!quick && age >= statusSlowMs && id !== selectedComputerId && !holdsControlOn(id)) slow.push(id)
    }
    if (!anyMoving) fresh = []
    if (!sameValue(attentionFresh, fresh)) attentionFresh = fresh
    if (attentionQuick !== anyMoving) attentionQuick = anyMoving
    // Longest waiting first, so every moving computer gets its turn when more than eight move.
    var oldest = function(a, b) { return Number(statusPolledAt[a] || 0) - Number(statusPolledAt[b] || 0) }
    fast.sort(oldest)
    slow.sort(oldest)
    for (var f = 0; f < fast.length && f < 8; f++) pollStatus(fast[f])
    for (var w = 0; w < slow.length && w < 4; w++) pollStatus(slow[w])
  }
  Timer { id: statusTick; interval: root.statusFastMs; repeat: true; running: (root.panelOpen || root.heldComputerIds.length > 0 || Object.keys(root.viewers).length > 0) && !root.denied; onTriggered: root.pollVisibleStatus() }
  // Keep turn presence live even while the console itself is hidden.
  Timer { interval: 250; repeat: true; running: Object.keys(root.viewers).length > 0 && !root.denied; onTriggered: Object.keys(root.viewers).forEach(function(id) { root.pollStatus(id) }) }
  property var pendingFallbackViewers: ({})
  Timer { interval: 250; repeat: true; running: Object.keys(root.pendingFallbackViewers).length > 0 && !root.mutating; onTriggered: root.fallbackViewerFor(Object.keys(root.pendingFallbackViewers)[0]) }
  function fallbackViewerFor(id) {
    var s = sessions[id]
    if (mutating) return
    var waiting = Object.assign({}, pendingFallbackViewers)
    delete waiting[id]
    pendingFallbackViewers = waiting
    if (!viewerOpenOn(id) || !s || !s.fallback_reason) return
    pendingControl = { computer_id: id, endpoint_id: s.endpoint_id, binding_revision: s.binding_revision,
      controller_epoch: s.controller_epoch, authorization_generation: s.authorization_generation }
    startHelper("operator-take-control", ["operator-control", "--computer", id, "--epoch", s.controller_epoch,
      "--op", "screen_failed", "--owner", s.owner_name, "--revision", s.ownership_revision])
  }
  readonly property string previewLimitNotice: "Not previewed: at most 20 computers preview at once. Scroll or filter to see this one."
  // `large`: the wall shows these as at most four large cards, which preview at selected quality
  // at the Fleet picture interval (as tiles do), so their pictures stay sharp.
  function setVisibleComputerIds(ids, large) {
    var allowed = ({})
    for (var i = 0; i < computers.length; i++) allowed[computers[i].computer_id] = true
    var requested = Array.isArray(ids) ? ids.filter(function(id) { return typeof id === "string" && allowed[id] }) : []
    var next = requested.slice(0, 20)
    // On-screen cards past the limit say why they have no picture instead of "loading".
    var capped = requested.slice(20), labeled = null
    for (var c = 0; c < capped.length; c++) {
      var s = sessions[capped[c]]
      // Only a healthy card waiting for a picture gets the limit notice; denial or other reasons stay.
      if (s && !s.frame && (s.connection === "ready" || s.connection === "loading") &&
          ["", "Preview pending", "Preview unavailable", "Waiting for an authorized preview."].indexOf(String(s.frame_error || "")) !== -1) {
        labeled = labeled || Object.assign({}, sessions)
        labeled[capped[c]] = Object.assign({}, s, { frame_error: previewLimitNotice })
      }
    }
    for (var n = 0; n < next.length; n++) {
      var shown = sessions[next[n]]
      if (shown && shown.frame_error === previewLimitNotice) {
        labeled = labeled || Object.assign({}, sessions)
        labeled[next[n]] = Object.assign({}, shown, { frame_error: "Preview pending" })
      }
    }
    if (labeled) {
      publishSessions(labeled)
    }
    // Cards turning large (or back) drop the other quality's pictures on their way and ask for
    // the new one at once.
    var wallLarge = large === true && next.length > 0 && next.length <= 4
    if (wallLarge !== largeWall) {
      largeWall = wallLarge
      previewQueue = previewQueue.filter(function(item) { return root.previewWanted(item) })
      var due = wallLarge ? ":selected" : ":tile", last = {}
      for (var key in previewLastRequested) if (!key.endsWith(due)) last[key] = previewLastRequested[key]
      previewLastRequested = last
    }
    if (next.length === visibleComputerIds.length && next.every(function(id, index) { return id === visibleComputerIds[index] })) return
    visibleComputerIds = next
    previewQueue = previewQueue.filter(function(item) { return item.quality === "selected" || next.indexOf(item.computer_id) !== -1 })
    Qt.callLater(root.refreshComputerSessions)
  }
  property var visibleComputerIds: []
  // The wall shows its cards large (at most four): they preview at selected quality.
  property bool largeWall: false
  readonly property string wallQuality: largeWall ? "selected" : "tile"
  // Whether a picture asked for is still wanted: a tile while its card (not a large one) or list
  // row shows; a selected picture while its computer's Screen tab is open, or while it is a large
  // wall card.
  function previewWanted(request) {
    var id = request.computer_id, shown = visibleComputerIds.indexOf(id) !== -1
    return request.quality === "tile" ? shown && !largeWall : selectedComputerId === id || (largeWall && shown)
  }
  property var previewQueue: []
  // The size, in device pixels, each preview quality is shown at, as its SteadyPreview last
  // settled it. ibarad writes each frame scaled to fit it; until one is known, its default.
  // A larger size asks for that quality's next frames at once rather than showing the
  // smaller ones stretched until they are due.
  property var previewShown: ({})
  function notePreviewSize(quality, width, height) {
    if (quality !== "tile" && quality !== "selected") return
    width = Math.round(Number(width)); height = Math.round(Number(height))
    if (!(width >= 16 && width <= 4096 && height >= 16 && height <= 4096)) return
    var known = previewShown[quality]
    if (known && known.width === width && known.height === height) return
    var shown = Object.assign({}, previewShown)
    shown[quality] = { width: width, height: height }
    previewShown = shown
    if (!known || width > known.width || height > known.height) {
      var last = {}
      for (var key in previewLastRequested) if (!key.endsWith(":" + quality)) last[key] = previewLastRequested[key]
      previewLastRequested = last
    }
  }
  property var previewActive: [null, null, null, null]
  property var previewLastRequested: ({})
  property int previewSequence: 0
  property int tileCursor: 0
  property int previewGeneration: 0
  property var previewTargetGenerations: ({})

  function targetPreviewGeneration(computerId) {
    return Number(previewTargetGenerations[String(computerId || "")] || 0)
  }
  function invalidateTargetPreviews(computerId) {
    var id = String(computerId || "")
    if (!id) return
    var generations = Object.assign({}, previewTargetGenerations)
    generations[id] = targetPreviewGeneration(id) + 1
    previewTargetGenerations = generations
    previewQueue = previewQueue.filter(function(item) { return item.computer_id !== id })
  }

  function bindSessionEpoch(computerId, endpointId, revision, epoch, authorizationGeneration) {
    var current = sessions[String(computerId || "")]
    if (!current || current.trust_state !== "verified" || current.endpoint_id !== endpointId || current.binding_revision !== revision || current.authorization_generation !== authorizationGeneration || !/^[A-Za-z0-9_.:-]{1,128}$/.test(String(epoch || ""))) return false
    var changed = !!current.controller_epoch && current.controller_epoch !== String(epoch)
    if (changed) clearTargetEpoch(computerId, current.controller_epoch)
    // A restarted target has forgotten any control this operator was told about.
    if (changed) actionNotice = ""
    var updated = Object.assign({}, sessions)
    updated[computerId] = Object.assign({}, current, { controller_epoch: String(epoch), connection: "loading",
      frame: changed ? null : current.frame, capture_age_ms: changed ? null : current.capture_age_ms,
      frame_error: changed ? "Controller restarted; waiting for a new preview." : (current.frame ? current.frame_error : "Preview pending"),
      active_task_ref: changed ? "" : current.active_task_ref, active_task: changed ? null : current.active_task })
    publishSessions(updated)
    clearRetryState(computerId)
    requestRead("session-status:" + computerId, ["operator-status", "--computer", computerId, "--epoch", String(epoch)], computerId, sessionStatusSnapshot(computerId))
    settleWaiters(String(computerId))
    return true
  }

  function queuePreview(computerId, quality) {
    var session = sessions[String(computerId || "")]
    var selected = quality === "selected"
    // Watch: the open computer's Screen tab, a picture a second. A large wall card's selected
    // pictures come at the Fleet picture interval.
    var watching = selected && watchVisible && selectedComputerId === computerId
    var key = String(computerId) + ":" + quality
    var now = Date.now()
    // Selected Watch owns this computer's frames while its Screen tab shows; a tile duplicate would
    // only compete for its single capture. Its small list thumbnail still needs a tile frame, so
    // until it has one a tile is asked for at the tile pace. Elsewhere the open computer
    // previews at tile quality.
    if (!selected && consoleOpen && watchVisible && selectedComputerId && computerId === selectedComputerId && hasTileFrame(session)) return false
    // While a computer's Live Video plays, its pictures (the poster under the video) come every 30 s.
    if (now - Number(previewLastRequested[key] || 0) < (videoPlayingFor(computerId) ? 30000 : watching ? 1000 : tilePreviewMs)) return false
    if (denied || !session || session.trust_state !== "verified" || session.connection === "unauthorized" || !session.controller_epoch) return false
    if (!watching && visibleComputerIds.indexOf(computerId) === -1) return false
    if (selected && !watching && !largeWall) return false
    // Offline backoff: tiles wait it out; a watched computer retries after at most 3 s.
    var backoffEntry = previewBackoff[computerId]
    if (backoffEntry && now < (watching ? Math.min(backoffEntry.until, backoffEntry.failed + 3000) : backoffEntry.until)) return false
    for (var busy = 0; busy < previewActive.length; busy++)
      if (previewActive[busy] && previewActive[busy].computer_id === computerId) return false
    var queue = previewQueue.filter(function(item) { return item.key !== key })
    if (queue.length >= 20) return false
    queue.push({ key: key, computer_id: computerId, quality: quality,
      endpoint_id: session.endpoint_id, binding_revision: session.binding_revision,
      authorization_generation: session.authorization_generation,
      controller_epoch: session.controller_epoch, generation: previewGeneration,
      target_generation: targetPreviewGeneration(computerId),
      display_id: session.display_id || "" })
    previewQueue = queue
    var last = Object.assign({}, previewLastRequested)
    last[key] = now
    previewLastRequested = last
    drainPreviews()
    return true
  }
  function hasTileFrame(session) {
    var frame = session && session.frame
    return !!frame && (frame.quality === "tile" || !!frame.tile)
  }
  function tileFrameOf(frame) {
    if (!frame) return null
    return frame.quality === "tile" ? { url: frame.url, file: frame.file, quality: "tile" } : frame.tile || null
  }

  function drainPreviews() {
    if (!previewQueue.length) return
    if (!panelOpen) { previewQueue = []; return }
    var lane = -1
    for (var i = 0; i < previewActive.length; i++) if (!previewActive[i]) { lane = i; break }
    if (lane < 0) return
    var queue = previewQueue.slice()
    // Computers already known offline share one lane, so several unreachable targets
    // cannot hold every lane for their full timeout while healthy cards wait.
    var isOffline = function(item) { var s = item && sessions[item.computer_id]; return !!s && s.connection === "offline" }
    var offlineActive = previewActive.some(isOffline)
    var chosen = queue.findIndex(function(item) { return item.quality === "selected" })
    if (chosen < 0) chosen = queue.findIndex(function(item) { return !(offlineActive && isOffline(item)) })
    if (chosen < 0) return
    var request = queue.splice(chosen, 1)[0]
    previewQueue = queue
    var session = sessions[request.computer_id]
    if (!request.display_id) { Qt.callLater(root.drainPreviews); return }
    if (denied || !session || session.connection === "unauthorized" || session.endpoint_id !== request.endpoint_id || session.binding_revision !== request.binding_revision || session.authorization_generation !== request.authorization_generation || session.controller_epoch !== request.controller_epoch || request.generation !== previewGeneration || request.target_generation !== targetPreviewGeneration(request.computer_id) || !previewWanted(request)) { Qt.callLater(root.drainPreviews); return }
    request.request_id = "preview-" + (++previewSequence)
    request.dispatched_ms = Date.now()
    var active = previewActive.slice()
    active[lane] = request
    previewActive = active
    var shown = previewShown[request.quality]
    daemonSend(request.request_id, ["operator-observe", "--computer", request.computer_id, "--epoch", request.controller_epoch,
      "--display", request.display_id, "--quality", request.quality].concat(shown ? ["--width", String(shown.width), "--height", String(shown.height)] : []), { kind: "preview" })
    Qt.callLater(root.drainPreviews)
  }

  // Every preview crosses the one ibarad connection; frames arrive as events by request ID,
  // in any order. A deliberate release (close, settings, denial) frees the lanes quietly and
  // ignores any frame still on its way.
  function releasePreviewLanes() {
    var pending = Object.assign({}, daemonPending), released = false
    for (var id in pending) if (pending[id].kind === "preview") { delete pending[id]; released = true }
    if (released) {
      daemonPending = pending
      daemonOutbox = daemonOutbox.filter(function(item) { return !!pending[item.id] })
    }
    if (previewActive.some(function(item) { return !!item })) previewActive = [null, null, null, null]
    Qt.callLater(root.drainPreviews)
  }
  // Closing the console: sessions forget selected-quality frames, then ibarad deletes every
  // frame file no session still shows. Tiles show at once when it reopens, and a forgotten
  // selected picture (Watch, or a large wall card) is asked for again at once.
  function closePreviews() {
    var kept = Object.assign({}, sessions), forgot = false
    for (var sid in kept) if (kept[sid] && kept[sid].frame && kept[sid].frame.quality === "selected") {
      kept[sid] = Object.assign({}, kept[sid], { frame: null, capture_age_ms: null, frame_error: "Preview pending" })
      forgot = true
    }
    if (forgot) {
      publishSessions(kept)
      var last = {}
      for (var key in previewLastRequested) if (!key.endsWith(":selected")) last[key] = previewLastRequested[key]
      previewLastRequested = last
    }
    releasePreviewLanes()
    releasePreviewFiles()
  }
  // ibarad keeps only the frame files named here: the ones a session still points at.
  function releasePreviewFiles() {
    var keep = []
    for (var id in sessions) {
      var shown = sessions[id] && sessions[id].frame
      var file = shown ? String(shown.file || "") : ""
      if (file) keep.push(file.slice(file.lastIndexOf("/") + 1))
      var tile = shown && shown.tile ? String(shown.tile.file || "") : ""
      if (tile) keep.push(tile.slice(tile.lastIndexOf("/") + 1))
    }
    requestRead("preview-release", ["preview-release"].concat(keep))
  }
  // ibarad answers each preview within 15 s. A lane older than 20 s never got its answer:
  // fail it, and ignore the answer if it still comes.
  function expireStalePreviews() {
    var now = Date.now()
    for (var lane = 0; lane < previewActive.length; lane++) {
      var request = previewActive[lane]
      if (!request || now - Number(request.dispatched_ms || now) < 20000) continue
      forgetDaemonRequest(request.request_id)
      consumePreview(lane, { version: 2, request_id: request.request_id, command: "operator-observe",
        connection: "offline", data: null, error: { code: "TIMEOUT", message: "The computer didn't send a picture in time. ibara will keep trying.", retry_safe: true } })
    }
  }

  // A frame file is a P6 header and RGB pixels, at most the target's 480 × 270 tile or 1280 × 720 selected frame.
  function previewFileLimit(quality) {
    return (quality === "tile" ? 480 * 270 : 1280 * 720) * 3 + 32
  }
  function consumePreview(lane, text) {
    var request = previewActive[lane]
    var active = previewActive.slice()
    active[lane] = null
    previewActive = active
    if (!request) return
    var session = sessions[request.computer_id]
    if (!denied && session && session.connection !== "unauthorized" && session.endpoint_id === request.endpoint_id && session.binding_revision === request.binding_revision && session.authorization_generation === request.authorization_generation && session.controller_epoch === request.controller_epoch && request.generation === previewGeneration && request.target_generation === targetPreviewGeneration(request.computer_id) && previewWanted(request)) {
      try {
        var parsed = StatusModel.parseEnvelope(text, 2 * 1024 * 1024)
        if (parsed.request_id !== request.request_id) throw new Error("ibara dropped a picture it couldn't trust. The next one is on its way.")
        if (parsed.error) refusePreview(request.computer_id, parsed.error)
        else {
          var data = parsed.data || ({}), frame = data.result || ({})
          if (data.computer_id !== request.computer_id || data.endpoint_id !== request.endpoint_id || data.binding_revision !== request.binding_revision || data.controller_epoch !== request.controller_epoch || frame.endpoint_id !== request.endpoint_id || frame.controller_epoch !== request.controller_epoch || frame.authorization_generation !== request.authorization_generation || !StatusModel.previewFile(frame.file, request.computer_id, request.quality) || !(Number(frame.bytes) > 0 && Number(frame.bytes) <= previewFileLimit(request.quality))) throw new Error("ibara dropped a picture it couldn't trust. The next one is on its way.")
          // ibarad writes the picture to a private file; the script never holds its bytes.
          frame = Object.assign({}, frame, { url: "file://" + frame.file, quality: request.quality, received_ms: Date.now() })
          // A selected frame carries the computer's last tile frame for its list thumbnail. A tile
          // that arrives while that computer's Screen shows joins the selected picture rather
          // than replace it.
          var prior = session.frame
          var watched = consoleOpen && watchVisible && selectedComputerId === request.computer_id
          if (request.quality === "selected") frame.tile = tileFrameOf(prior)
          else if (watched && prior && prior.quality === "selected") frame = Object.assign({}, prior, { tile: tileFrameOf(frame) })
          var updated = Object.assign({}, sessions)
          var capturedAt = StatusModel.isoMs(frame.capture_time)
          var age = Date.now() - capturedAt
          // A picture means the screen is unlocked: a locked one refuses pictures.
          updated[request.computer_id] = Object.assign({}, session, { frame: frame, frame_error: "", connection: "ready", locked: false, capture_age_ms: isFinite(age) && age >= 0 ? age : null })
          publishSessions(updated)
          enforcePreviewMemory()
          if (previewBackoff[request.computer_id]) { var cleared = Object.assign({}, previewBackoff); delete cleared[request.computer_id]; previewBackoff = cleared }
        }
      } catch (error) {
        var failed = Object.assign({}, sessions)
        failed[request.computer_id] = Object.assign({}, session, { frame_error: StatusModel.clip(error && error.message ? error.message : error, 200) })
        publishSessions(failed)
      }
    }
    Qt.callLater(root.drainPreviews)
  }

  // A restarted or rebound target answers the old epoch with "Target binding changed." The
  // one-shot and preview routes wrap it as OPERATOR_REFUSED with the target code in the message.
  function targetEpochChanged(error) {
    var code = String(error && error.code || ""), message = String(error && error.message || "")
    return code === "IDENTITY_MISMATCH" || code === "STALE_EPOCH" || /^STALE_EPOCH\b/.test(message) || /(^|: )Target binding changed\.$/.test(message)
  }
  // Fail closed. Only known transient conditions keep the last authorized frame: route
  // timeouts/loss (labeled offline, with backoff) and a busy, missing or late capture
  // (annotated). Any other refusal (denial, revocation, forget, identity change, unknown)
  // clears the pixels until an authenticated status confirms access again.
  property var previewBackoff: ({})
  function refusePreview(computerId, error) {
    var session = sessions[computerId]
    if (!session) return
    var code = String(error && error.code || ""), message = String(error && (error.message || error.code) || "Preview refused.")
    // ibara on this computer stopped, not the computer: it keeps its last picture and state.
    if (code === "DAEMON_UNAVAILABLE") return
    var offline = ["TIMEOUT", "OPERATOR_TRANSPORT_UNAVAILABLE"].indexOf(code) !== -1 || /^Selected operator transport (timed out|closed)/.test(message)
    // A locked screen refuses pictures (HUMAN_CONTROL): the computer answers and is Locked, not refused.
    var locked = /\bHUMAN_CONTROL\b/.test(code + " " + message) && /\blocked\b/i.test(message)
    var updated = Object.assign({}, sessions)
    if (offline || locked || /^(BUDGET_EXCEEDED|CAPABILITY_UNAVAILABLE|TIMEOUT)\b/.test(message)) {
      if (offline) {
        // Per-target jittered exponential backoff (3 s → 60 s) so unreachable computers cannot hold the lanes.
        var backoff = Object.assign({}, previewBackoff), prior = backoff[computerId]
        var delay = Math.min(60000, prior ? prior.delay * 2 : 3000)
        backoff[computerId] = { delay: delay, failed: Date.now(), until: Date.now() + Math.round(delay * (0.8 + Math.random() * 0.4)) }
        previewBackoff = backoff
      }
      updated[computerId] = Object.assign({}, session, { connection: offline ? "offline" : session.connection, locked: locked || session.locked === true, frame_error: locked ? "Screen locked · Join to unlock" : StatusModel.clip(message, 200) })
    } else {
      var epochChanged = targetEpochChanged(error)
      invalidateTargetPreviews(computerId)
      clearWatchAcknowledgment(computerId)
      if (epochChanged) clearTargetEpoch(computerId, session.controller_epoch)
      else forgetComputerCaches(computerId)
      updated[computerId] = Object.assign({}, session, {
        connection: epochChanged ? "loading" : "unauthorized", frame: null, capture_age_ms: null,
        controller_epoch: epochChanged ? "" : session.controller_epoch,
        active_task_ref: epochChanged ? "" : session.active_task_ref, active_task: null,
        frame_error: epochChanged ? "Controller restarted; waiting for a new preview." : StatusModel.clip("Access not confirmed. " + message, 200) })
      Qt.callLater(root.refreshComputerSessions)
    }
    publishSessions(updated)
  }

  // I4 bound: frame files plus decoded surfaces stay within 64 MiB per operator.
  // A visible card can hold two frames and two decoded layers while it cross-fades.
  readonly property int previewMemoryLimit: 64 * 1024 * 1024
  function framePayloadBytes(frame) {
    return frame && typeof frame === "object" ? Number(frame.bytes) || 0 : 0
  }
  function retainedPreviewBytes() {
    var total = 0
    for (var id in sessions) {
      var session = sessions[id]
      if (!session || !session.frame) continue
      var payload = framePayloadBytes(session.frame)
      if (visibleComputerIds.indexOf(id) !== -1) total += 2 * payload + 2 * (session.frame.quality === "selected" ? 1280 * 720 : 480 * 270) * 4
      else total += payload
      if (id === selectedComputerId && consoleOpen && watchVisible) total += 2 * payload + 2 * 1280 * 720 * 4
    }
    return total
  }
  function enforcePreviewMemory() {
    var total = retainedPreviewBytes()
    if (total <= previewMemoryLimit) return
    var candidates = []
    for (var id in sessions) {
      var session = sessions[id]
      if (session && session.frame && id !== selectedComputerId && visibleComputerIds.indexOf(id) === -1)
        candidates.push({ id: id, at: Number(session.frame.received_ms) || 0, bytes: framePayloadBytes(session.frame) })
    }
    candidates.sort(function(a, b) { return a.at - b.at })
    var updated = Object.assign({}, sessions)
    for (var i = 0; i < candidates.length && total > previewMemoryLimit; i++) {
      updated[candidates[i].id] = Object.assign({}, updated[candidates[i].id], { frame: null, capture_age_ms: null,
        frame_error: "Preview released to stay within the memory bound. It refreshes when this computer is visible." })
      total -= candidates[i].bytes
    }
    publishSessions(updated)
  }
  function previewStats() {
    var rows = []
    for (var i = 0; i < computers.length; i++) {
      var c = computers[i]
      if (!c) continue
      var frame = c.frame && typeof c.frame === "object" ? c.frame : null
      // displayed_age_ms matches the wall's label: age on arrival plus local time since arrival.
      var displayed = frame && c.capture_age_ms != null && isFinite(Number(frame.received_ms)) ? c.capture_age_ms + Math.max(0, Date.now() - Number(frame.received_ms)) : null
      rows.push({ computer_id: c.computer_id, connection: c.connection || "", visible: visibleComputerIds.indexOf(c.computer_id) !== -1,
        capture_time: frame ? String(frame.capture_time || "") : "", displayed_age_ms: displayed,
        frame_sequence: frame && typeof frame.frame_sequence === "number" ? frame.frame_sequence : null,
        quality: frame ? String(frame.quality || "") : "", frame_error: String(c.frame_error || "") })
    }
    return JSON.stringify({ now: new Date().toISOString(), selected: selectedComputerId, console_open: consoleOpen,
      lanes_active: previewActive.filter(function(item) { return !!item }).length, queue_length: previewQueue.length,
      reads_active: activeReads.filter(function(item) { return !!item }).length, read_queue_length: readQueue.length,
      retained_bytes_estimate: retainedPreviewBytes(), computers: rows })
  }

  // Commands for the computer in scope, over its pairing route.
  function openViewerFor(computerId) {
    if ((!holdsControlOn(computerId) && (!sessions[computerId] || sessions[computerId].screen_engine !== "ibara")) || mutating) { actionError = mutating ? "Wait for the current action to finish." : "Choose Join to open this screen."; return false }
    var epoch = sessions[String(computerId)] && sessions[String(computerId)].controller_epoch
    pendingViewerComputerId = String(computerId)
    startHelper("open-viewer", ["open-viewer", "--computer", String(computerId)].concat(epoch ? ["--epoch", String(epoch)] : []))
    return true
  }
  // The viewer this console opened for each computer (computer → pid, from the Join and
  // Open Viewer answers), kept while that process runs, so the console offers Open Viewer or
  // Close Viewer as it really is, also after the person closes the viewer window themselves.
  property var viewers: ({})
  property var viewerConnecting: ({})
  property var viewerScreens: ({})
  property string pendingViewerComputerId: ""
  property var viewerClosed: ({})
  function keepControl(computerId) {
    var next = Object.assign({}, viewerClosed); delete next[computerId]; viewerClosed = next
  }
  function viewerDidClose(computerId) {
    var handingBack = pendingMutation === "operator-handback" && pendingControl && pendingControl.computer_id === computerId
    if (viewerConnecting[computerId] && holdsControlOn(computerId) && !handingBack) actionError = computerLabelFor(computerId) + ": the viewer closed before connecting. Try Open Viewer."
    var waiting = Object.assign({}, viewerConnecting); delete waiting[computerId]; viewerConnecting = waiting
    if (!holdsControlOn(computerId) || handingBack) return
    var next = Object.assign({}, viewerClosed); next[computerId] = true; viewerClosed = next
  }
  function viewerOpenOn(computerId) { return !!viewers[String(computerId || "")] }
  function noteViewer(computerId, pid) {
    var id = String(computerId || ""), next = Object.assign({}, viewers), waiting = Object.assign({}, viewerConnecting)
    pid = Number(pid)
    if (id && Number.isInteger(pid) && pid > 0) {
      next[id] = pid; keepControl(id)
      waiting[id] = { pid: pid, deadline: Date.now() + 15000 }
    } else { delete next[id]; delete waiting[id] }
    viewerConnecting = waiting
    if (!sameValue(viewers, next)) viewers = next
  }
  // A launched process isn't a visible viewer yet. Stop Connecting once its window maps.
  Timer {
    interval: Object.keys(root.viewerConnecting).length ? 150 : 300
    repeat: true
    running: Object.keys(root.viewerConnecting).length > 0 || Object.keys(root.sessions).some(function(id) { return root.holdsControlOn(id) })
    onTriggered: if (!viewerWindows.running) {
      viewerWindows.checked = root.viewerConnecting
      viewerWindows.running = true
    }
  }
  Process {
    id: viewerWindows
    property var checked: ({})
    command: ["hyprctl", "-j", "clients"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var clients = []
        try { clients = JSON.parse(text) } catch (error) {}
        var pids = Array.isArray(clients) ? clients.filter(function(c) { return c.mapped === true && c.hidden !== true }).map(function(c) { return Number(c.pid) }) : []
        var screens = Object.assign({}, root.viewerScreens)
        for (var computer in root.viewers) {
          var client = clients.find(function(c) { return Number(c.pid) === Number(root.viewers[computer]) && c.mapped === true })
          if (!client || !Array.isArray(client.at) || !Array.isArray(client.size)) continue
          var x = client.at[0] + client.size[0] / 2, y = client.at[1] + client.size[1] / 2
          var screen = Quickshell.screens.find(function(s) { return x >= s.x && x < s.x + s.width && y >= s.y && y < s.y + s.height })
          if (screen) screens[computer] = screen.name
        }
        if (!root.sameValue(root.viewerScreens, screens)) root.viewerScreens = screens
        var waiting = Object.assign({}, root.viewerConnecting)
        for (var id in viewerWindows.checked) {
          var pin = viewerWindows.checked[id]
          if (!waiting[id] || waiting[id].pid !== pin.pid) continue
          if (pids.indexOf(pin.pid) !== -1) delete waiting[id]
          else if (Date.now() >= pin.deadline) {
            delete waiting[id]
            root.actionError = root.computerLabelFor(id) + ": the viewer didn't appear. Try Open Viewer."
          }
        }
        root.viewerConnecting = waiting
      }
    }
  }
  // Close Viewer closes the window; Sunshine retains its existing pause until Hand Back.
  function closeViewerFor(computerId) {
    var id = String(computerId || ""), pid = viewers[id]
    if (!pid) return false
    var closer = oneShotComponent.createObject(root)
    closer.command = ["sh", "-c", "case \"$(cat /proc/$1/comm 2>/dev/null)\" in ibara-screen|ibara-view|moonlight) kill -TERM \"$1\";; esac", "sh", String(pid)]
    closer.running = true
    noteViewer(id, 0)
    viewerDidClose(id)
    return true
  }
  // Every 1.5 s while a viewer is open: which of them still run (ibara-screen, ibara-view, or Moonlight for an
  // older computer, checked by name so a reused process id never counts).
  Timer {
    interval: 1500
    repeat: true
    running: Object.keys(root.viewers).length > 0
    onTriggered: {
      if (viewerCheck.running) return
      var pids = []
      for (var id in root.viewers) pids.push(root.viewers[id])
      viewerCheck.checked = pids
      viewerCheck.command = ["sh", "-c", "for p; do case \"$(cat /proc/$p/comm 2>/dev/null)\" in ibara-screen|ibara-view|moonlight) echo \"$p\";; esac; done", "sh"].concat(pids.map(String))
      viewerCheck.running = true
    }
  }
  Process {
    id: viewerCheck
    property var checked: []
    // Only a viewer that was checked and has gone is dropped: one opened meanwhile stays.
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var alive = String(text || "").split("\n").map(Number), next = ({})
        for (var id in root.viewers) {
          var pid = root.viewers[id]
          if (viewerCheck.checked.indexOf(pid) === -1 || alive.indexOf(pid) !== -1) next[id] = pid
          else root.viewerDidClose(id)
        }
        if (!root.sameValue(root.viewers, next)) root.viewers = next
      }
    }
  }
  // A terminal on this desktop, signed in to the computer over Tailscale.
  function openTerminalFor(computerId) {
    if (!sessions[String(computerId || "")] || mutating) { if (mutating) actionError = "Wait for the current action to finish."; return false }
    startHelper("open-terminal", ["open-terminal", "--computer", String(computerId)])
    return true
  }
  function loadTasks() { requestScopedRead("tasks", "operator-tasks", []) }
  function loadArtifacts() { requestScopedRead("artifacts", "operator-artifacts", ["--limit", "20"]) }
  function loadProcedures() { requestScopedRead("procedures", "operator-procedures", []) }
  function loadHealth() { requestScopedRead("health", "operator-health", []) }
  function loadLogs(which) {
    logsWhich = which === "viewer" ? "viewer" : "ibara"
    requestScopedRead("logs", "operator-logs", ["--which", logsWhich, "--lines", "80"])
  }
  function loadComputerSettings() { requestScopedRead("computer-settings", "operator-settings", ["get"]) }
  function loadWindows() { requestScopedRead("windows", "operator-windows", []) }
  function inspectTask(taskRef) {
    var ref = String(taskRef || "")
    if (!ref) return
    if (selectedTaskRef !== ref) { selectedTask = null; selectedTaskDetail = null; selectedTaskDetailRef = ""; selectedTaskDetailObservedAt = "" }
    selectedTaskRef = ref
    selectedTaskLoading = true
    requestScopedRead("task", "operator-task", ["--task", ref], ref)
  }
  function inspectProcedure(procedureRef) {
    var ref = String(procedureRef || "")
    if (!ref) return
    if (selectedProcedureRef !== ref) selectedProcedure = null
    selectedProcedureRef = ref
    selectedProcedureLoading = true
    requestScopedRead("procedure", "operator-procedure", ["--ref", ref], ref)
  }
  function extendTask(taskRef, seconds) { return startComputerAction("extend", "operator-task-extend", scopedComputerId, ["--task", String(taskRef), "--seconds", String(seconds)]) }
  function releaseComputerTask(computerId, taskRef) {
    var current = sessions[computerId]
    if (!current || String(current.active_task_ref || "") !== taskRef) {
      actionError = "The active task changed. Review this computer before stopping it."
      return false
    }
    return startComputerAction("revoke", "operator-task-revoke", computerId, ["--task", taskRef])
  }
  function revokeTask(taskRef) { return startComputerAction("revoke", "operator-task-revoke", scopedComputerId, ["--task", String(taskRef)]) }
  // Close and Move To… on the Windows tab. Close is Hyprland's polite close, so an app can still ask to save.
  function windowTitle(address) {
    var spaces = computerWindows.workspaces || []
    for (var i = 0; i < spaces.length; i++)
      for (var j = 0; j < spaces[i].windows.length; j++)
        if (spaces[i].windows[j].address === address) return String(spaces[i].windows[j].title || spaces[i].windows[j].class || "the window")
    return "the window"
  }
  function windowActionFor(kind, command, address, rest) {
    var id = scopedComputerId
    if (mutating) { actionError = "Wait for the current action to finish."; return false }
    windowAction = { computerId: id, address: String(address), title: windowTitle(String(address)) }
    if (startComputerAction(kind, command, id, ["--address", String(address)].concat(rest))) return true
    windowAction = null
    return false
  }
  function closeWindow(address, pid) { return windowActionFor("window-close", "operator-window-close", address, ["--pid", String(pid)]) }
  function moveWindow(address, pid, workspace) { return windowActionFor("window-move", "operator-window-move", address, ["--pid", String(pid), "--workspace", String(workspace)]) }
  // Collect File saves one of the open computer's results into the download folder, never
  // overwriting a file already there.
  function fetchArtifact(artifactRef, name) {
    var local = humanReceiveLocal(downloadFolder, String(name || ""))
    if (!local) { actionError = "ibara can't save " + String(name || "this result") + " in " + downloadFolder + ". Choose another Download folder in ibara's Settings."; return false }
    return startComputerAction("fetch", "operator-artifact-save", scopedComputerId, ["--ref", String(artifactRef), "--to", local])
  }
  function approveProcedure(procedureRef, sha) { return startComputerAction("approve-procedure", "operator-procedure-review", scopedComputerId, ["--ref", String(procedureRef), "--decision", "approve", "--sha", String(sha)]) }
  function quarantineProcedure(procedureRef) { return startComputerAction("quarantine-procedure", "operator-procedure-review", scopedComputerId, ["--ref", String(procedureRef), "--decision", "quarantine"]) }
  // The open computer's access table: every identity, its rules and its pairing.
  function loadAccess() { requestScopedRead("access", "operator-access", []) }
  // An access change (access-set, access-remove, access-unpair) finished for one computer.
  signal accessChangeSettled(string computerId, bool ok)
  function setAccess(subject, capability, rule, revision, effects) {
    var body = {subject: subject, capability: capability, rule: rule, expected_revision: revision}
    if (effects) body.effects = effects
    return startComputerAction("access-set", "operator-access-set", scopedComputerId, [JSON.stringify(body)])
  }
  function removeAccess(subject, revision, unpair) {
    var kind = unpair ? "access-unpair" : "access-remove"
    return startComputerAction(kind, "operator-" + kind, scopedComputerId, [JSON.stringify({subject: subject, expected_revision: revision})])
  }

  function loadTailnet() { if (!readPending("tailnet")) requestRead("tailnet", ["tailnet"]) }
  function loadPairRequests() { if (!readPending("pair-requests")) requestRead("pair-requests", ["pair-requests"]) }
  function loadConnectPrompt() { if (!readPending("connect-prompt")) requestRead("connect-prompt", ["connect-prompt"]) }
  function sameValue(a, b) { return JSON.stringify(a) === JSON.stringify(b) }
  // Adding computers and the everyday features below take a lane of their own
  // (the side lane): several run at once, and none holds up Join or a file transfer.
  function sideSend(args, route) {
    sideSequence += 1
    daemonSend("side-" + sideSequence + "-" + args[0], args, Object.assign({ kind: "side", op: String(args[0]) }, route || {}))
  }
  // An add that stopped (declined, expired or failed, such as a refused invite code) says why
  // in an error toast; its row on Add Computer keeps only "Couldn't add it" and Try Again.
  function setPairing(node, fields) {
    var next = Object.assign({}, pairings)
    next[node] = Object.assign({ node: node }, pairings[node] || {}, fields)
    pairings = next
    var p = next[node], name = p.label || StatusModel.computerName(node)
    if (["declined", "expired", "failed"].indexOf(fields.state) !== -1)
      actionError = p.state === "declined" ? "Someone at " + name + " declined. Ask them to accept, then choose Try Again."
        : p.state === "expired" ? "Nobody accepted within 5 minutes. Choose Try Again, then accept on " + name + "."
        : p.message || "ibara couldn't add " + name + ". Choose Try Again."
  }
  function forgetPairing(node) {
    if (!pairings[node]) return
    var next = Object.assign({}, pairings)
    delete next[node]
    pairings = next
  }
  // Adds one computer from the tailnet. Your own computer adds itself; another person's
  // computer shows a code here that its screen shows too, and waits for someone there to accept.
  // With the invite code its owner gave you (`inviteCode`), another person's computer adds at once.
  function startPairing(node, inviteCode) {
    var name = String(node || ""), current = pairings[name], invite = String(inviteCode || "").trim()
    if (!StatusModel.tailnetNode(name) || denied || (current && ["starting", "waiting", "canceling"].indexOf(current.state) !== -1)) return false
    if (invite && !/^[A-Za-z0-9 -]{1,32}$/.test(invite)) {
      setPairing(name, { state: "failed", invite: true, message: "An invite code has 8 letters and numbers, like 4H7K-92QX. Check it, then try again." })
      return false
    }
    setPairing(name, { requestId: "", code: "", mode: "", message: "", computerId: "", label: "", state: "starting", startedAt: Date.now(), invite: !!invite })
    sideSend(invite ? ["pair-start", name, invite] : ["pair-start", name], { node: name })
    return true
  }
  function cancelPairing(node) {
    var p = pairings[String(node || "")]
    if (!p || ["starting", "waiting"].indexOf(p.state) === -1) return
    setPairing(p.node, { state: "canceling" })
    // A start still on its way is canceled as soon as its request id arrives.
    if (p.requestId) sideSend(["pair-cancel", p.requestId], { node: p.node, requestId: p.requestId })
  }
  function pollPairings() {
    var asked = Object.assign({}, pairStatusAsked)
    for (var node in pairings) {
      var p = pairings[node]
      if (p.state !== "waiting" || !p.requestId || asked[p.requestId]) continue
      // A request expires after 5 minutes; a console that stops hearing back stops waiting too.
      if (Date.now() - p.startedAt > 6 * 60000) { setPairing(node, { state: "expired" }); continue }
      asked[p.requestId] = true
      sideSend(["pair-status", p.requestId], { node: node, requestId: p.requestId })
    }
    pairStatusAsked = asked
  }
  Timer { interval: 2000; repeat: true; running: root.pairingActive; onTriggered: root.pollPairings() }
  function answerPairRequest(requestId, answer) {
    var id = String(requestId || ""), request = null
    for (var i = 0; i < pairRequests.length; i++) if (pairRequests[i].request_id === id) request = pairRequests[i]
    if (!request || ["accept", "decline"].indexOf(answer) === -1 || pairAnswers[id]) return false
    var next = Object.assign({}, pairAnswers)
    next[id] = answer
    pairAnswers = next
    sideSend(["pair-answer", id, answer], { requestId: id, answer: answer, from: request.from_computer })
    return true
  }
  // One notice per new request, so the person at this computer knows to look.
  function notifyPairRequests() {
    var seen = Object.assign({}, notifiedPairRequests), fresh = false
    for (var i = 0; i < pairRequests.length; i++) {
      var request = pairRequests[i]
      if (seen[request.request_id]) continue
      seen[request.request_id] = true
      fresh = true
      var name = StatusModel.computerName(request.from_computer)
      queueNotice({ title: "ibara", body: name + (request.from_owner ? " (" + request.from_owner + ")" : "") + " wants to use this computer. If " + name + " shows the code " + request.code + ", choose Accept in ibara." })
    }
    if (fresh) notifiedPairRequests = seen
  }
  // Sign In to Tailscale, as Omarchy's own Tailscale menu does it: Tailscale's sign-in page when
  // it has one waiting, else `tailscale up`, whose sign-in page opens as soon as it prints it.
  function signInToTailscale() {
    var tailscale = tailnet ? tailnet.tailscale : null
    if (tailscale && tailscale.login_url) { Qt.openUrlExternally(tailscale.login_url); return }
    if (tailscaleUp.running) return
    tailscaleUp.opened = false
    tailscaleUp.lastLine = ""
    tailscaleUp.running = true
  }
  function consumeSide(route, text) {
    var parsed
    try { parsed = StatusModel.parseEnvelope(text) }
    catch (error) { parsed = { error: { code: "UNREADABLE", message: "ibara couldn't read the answer. Try again.", retry_safe: true } } }
    var data = parsed.data || ({}), error = parsed.error || null
    // Pairing failures arrive in plain English already, naming the computer.
    var message = error ? StatusModel.clip(error.message || error.code, 300) : ""
    if (route.op === "pair-start" || route.op === "pair-status") consumePairing(route, data, error, message)
    else if (route.op === "pair-cancel") {
      var canceled = pairings[route.node]
      if (!canceled || canceled.state !== "canceling") return
      // A request answered before the cancel arrived keeps its answer: ask where it stands.
      if (error) setPairing(route.node, { state: "waiting", requestId: route.requestId })
      else forgetPairing(route.node)
    }
    else if (route.op === "pair-answer") {
      var answers = Object.assign({}, pairAnswers)
      delete answers[route.requestId]
      pairAnswers = answers
      var name = StatusModel.computerName(route.from)
      var accepted = !error && route.answer === "accept" && data.state !== "expired"
      // Accepted: the request's card turns to its match for a moment and says so itself.
      if (accepted) {
        pairRequests = pairRequests.map(function(request) { return request.request_id === route.requestId ? Object.assign({}, request, { accepted: true }) : request })
        acceptedLinger.restart()
      } else if (!error) pairRequests = pairRequests.filter(function(request) { return request.request_id !== route.requestId })
      if (error) actionError = message || "ibara couldn't answer " + name + ". Try again."
      else if (data.state === "expired") actionError = "The request from " + name + " expired. Add this computer again from " + name + "."
      else if (!accepted) actionNotice = "Declined. " + name + " can't use this computer."
      loadPairRequests()
    }
    else if (route.op === "invite-create" || route.op === "invite-revoke") consumeInvite(route, data, error, message)
    else if (route.op === "video") consumeVideo(route, data, error)
    else consumeEveryday(route, data, error, message)
  }
  function consumePairing(route, data, error, message) {
    var node = route.node, p = pairings[node]
    if (route.op === "pair-status") {
      var asked = Object.assign({}, pairStatusAsked)
      delete asked[route.requestId]
      pairStatusAsked = asked
    }
    if (!p || (route.op === "pair-status" && p.requestId !== route.requestId)) return
    if (p.state === "canceling") {
      // Canceled while the start was on its way: cancel the request it made.
      if (route.op !== "pair-start") return
      if (!error && data.request_id) sideSend(["pair-cancel", String(data.request_id)], { node: node, requestId: String(data.request_id) })
      else forgetPairing(node)
      return
    }
    if (["starting", "waiting"].indexOf(p.state) === -1) return
    if (error) {
      // A check that got no answer is asked again; the request itself is still waiting.
      if (route.op === "pair-status" && ["DAEMON_UNAVAILABLE", "TIMEOUT"].indexOf(String(error.code)) !== -1) return
      setPairing(node, { state: "failed", message: message || "ibara couldn't add " + StatusModel.computerName(node) + "." })
      return
    }
    var view = StatusModel.pairingView(data)
    if (!view) { setPairing(node, { state: "failed", message: "ibara got an answer it couldn't read. Choose Try Again." }); return }
    setPairing(node, { requestId: view.request_id, code: view.code || p.code, mode: view.mode || p.mode, state: view.state,
      message: view.message, computerId: view.computer_id, label: view.label })
    if (view.state === "paired") {
      loadComputers()
      loadTailnet()
      pairingAdded(node, view.label || StatusModel.computerName(node))
    }
  }
  // ---- Share This Computer: single-use invite codes for friends, made on this computer
  // (`invites`, `invite-create`, `invite-revoke`). A new code shows once, right after it is made.
  property var invites: []
  property bool invitesLoaded: false
  // This computer's page in the Tailscale admin console ({ address, share_url }), or null.
  property var shareTailscale: null
  property var newInvite: null
  // "create", or the id of the invite being revoked.
  property string inviteBusy: ""
  function loadInvites() { if (!readPending("invites")) requestRead("invites", ["invites"]) }
  function createInvite(level, lasts) {
    if (inviteBusy || ["watch", "use_with_approval", "take_control"].indexOf(level) === -1 || ["hour", "day", "week", "never"].indexOf(lasts) === -1) return false
    inviteBusy = "create"
    newInvite = null
    sideSend(["invite-create", level, lasts], {})
    return true
  }
  function revokeInvite(id) {
    var key = String(id || "")
    if (inviteBusy || !StatusModel.requestId(key)) return false
    inviteBusy = key
    sideSend(["invite-revoke", key], { inviteId: key })
    return true
  }
  function consumeInvite(route, data, error, message) {
    inviteBusy = ""
    if (error) actionError = message || (route.op === "invite-create" ? "ibara couldn't make the invite. Try again." : "ibara couldn't revoke the invite. Try again.")
    else if (route.op === "invite-create") newInvite = StatusModel.inviteView(data.invite)
    else {
      if (newInvite && newInvite.id === route.inviteId) newInvite = null
      actionNotice = data.ended === true ? "Revoked. Their access to this computer ended." : "Revoked. That code no longer works."
    }
    loadInvites()
  }
  // The fleet's card for this computer, once it is on the fleet.
  readonly property string thisComputerId: {
    var list = tailnet ? tailnet.computers : []
    for (var i = 0; i < list.length; i++) if (list[i].is_self && list[i].paired && list[i].computer_id) return list[i].computer_id
    return ""
  }
  // The computer this console runs on, as the tailnet names it.
  function isThisComputer(node) {
    var name = String(node || "").toLowerCase()
    if (!tailnet || !name) return false
    if (String(tailnet.tailscale.self_node || "").toLowerCase() === name) return true
    return tailnet.computers.some(function(c) { return c.is_self && c.node.toLowerCase() === name })
  }

  // ---- everyday features over the pairing route (release wave 2). Each travels on the side lane;
  // `busy` names what is on its way, by key, so its button can say so.
  property var busy: ({})
  function setBusy(key, value) {
    if (!key || (!value && !busy[key])) return
    var next = Object.assign({}, busy)
    if (value) next[key] = value
    else delete next[key]
    busy = next
  }
  // An error for one computer, with the fix it has when there is one, so the console can show
  // Fix It beside the message: the computer's own needs_person first, else one the error names.
  // `cause` is the error as ibarad sent it ({code, message}), whose code the shown text leaves out.
  property var actionErrorFix: null
  function reportError(message, computerId, cause) {
    var id = String(computerId || ""), s = sessions[id]
    var code = cause && typeof cause === "object" ? String(cause.code || "") : String(cause || "")
    var raw = cause && typeof cause === "object" ? String(cause.message || "") : ""
    var fix = s && s.needs_person && s.needs_person.fix ? s.needs_person.fix : StatusModel.fixForError(code, raw || message)
    actionErrorFix = id && fix ? { computerId: id, fix: fix } : null
    actionError = String(message || "")
  }
  // A computer's commands need its current connection (its controller epoch). One asked for before
  // that exists (the console was closed, or just opened) waits here and runs once it is up.
  property var connectWaiters: ({})
  function whenConnected(computerId, run, failed) {
    var id = String(computerId || ""), s = sessions[id]
    if (!s || s.trust_state !== "verified") { if (failed) failed(s ? "This computer isn't paired from here. Pair it again from Add Computer." : "That computer is no longer on your fleet."); return }
    if (s.controller_epoch) { run(); return }
    var next = Object.assign({}, connectWaiters)
    next[id] = (next[id] || []).concat([{ run: run, failed: failed || null }])
    connectWaiters = next
    if (!readPending("session-bootstrap:" + id)) requestRead("session-bootstrap:" + id, ["operator-session", "--computer", id], id)
  }
  function settleWaiters(computerId, error) {
    var list = connectWaiters[computerId]
    if (!list) return
    var next = Object.assign({}, connectWaiters)
    delete next[computerId]
    connectWaiters = next
    for (var i = 0; i < list.length; i++) {
      if (!error) list[i].run()
      else if (list[i].failed) list[i].failed(error)
    }
  }
  // A command for one computer on the side lane, as soon as its connection is up.
  function sendToComputer(command, computerId, rest, route, busyKey) {
    var id = String(computerId || "")
    if (busyKey) setBusy(busyKey, command)
    whenConnected(id, function() {
      var args = root.computerArgs(command, id, rest)
      if (!args) { if (busyKey) root.setBusy(busyKey, ""); root.reportError("ibara lost its connection to " + root.computerLabelFor(id) + ". Try again.", id, ""); return }
      root.sideSend(args, Object.assign({ computerId: id, busyKey: busyKey || "" }, route || {}))
    }, function(message) {
      if (busyKey) root.setBusy(busyKey, "")
      if (route && route.failed) route.failed(message)
      else root.reportError(message, id, "")
    })
  }
  function updateSession(computerId, fields) {
    var s = sessions[computerId]
    if (!s) return
    var next = Object.assign({}, sessions)
    next[computerId] = Object.assign({}, s, fields)
    publishSessions(next)
  }
  // Re-reads one computer's status soon after something changed it.
  function recheckComputer(computerId) {
    var s = sessions[computerId]
    if (s && s.controller_epoch && !readPending("session-status:" + computerId))
      requestRead("session-status:" + computerId, ["operator-status", "--computer", computerId, "--epoch", s.controller_epoch], computerId, sessionStatusSnapshot(computerId))
  }
  // The health report also carries the computer's repair state, wake details and whether a
  // restart stops at its disk password.
  function applyHealth(computerId, result) {
    var s = sessions[computerId]
    if (!s) return
    var fields = { disk_password: result.disk_password === true }
    if (result.wake !== undefined) fields.wake = StatusModel.wakeView(result.wake)
    if (result.repair && typeof result.repair === "object") fields.needs_person = StatusModel.needsPersonView(result.repair.needs_person)
    updateSession(computerId, fields)
  }

  // ---- approvals, agents' questions and computers that need you, across the fleet
  // (fleet-attention). Read every 10 s whether or not a console is open, so the bar can count
  // them and each new approval can ask on the desktop; every 1.5 s while a computer in view has an
  // agent working or something waiting for you (attentionQuick). Each has a standing toast in the console.
  property var attention: []
  property bool attentionLoaded: false
  // Computers whose last approvals read failed; their items are the ones read before.
  property var attentionUnreachable: []
  // Approvals to answer, oldest first, so a new one never displaces the one being read.
  readonly property var approvals: attention.filter(function(item) { return item.kind === "approval" })
  // Questions put away here because ibara (here or there) is too old to dismiss them itself.
  property var hiddenQuestions: ({})
  // Agents' questions, oldest first: answered with one of their options, or dismissed.
  readonly property var questions: attention.filter(function(item) { return item.kind === "question" && !hiddenQuestions[item.ref] })
  // Needs put away in the console: {computerId: the message}; one shows again once it changes.
  property var hiddenNeeds: ({})
  // Computers that need a person, one entry each (StatusModel.needsYouView).
  readonly property var needsYou: StatusModel.needsYouView(attention, computers, hiddenNeeds)
  readonly property int needsYouCount: approvals.length + logins.length + questions.length + needsYou.length
  function hideNeed(computerId) {
    var id = String(computerId || ""), hidden = Object.assign({}, hiddenNeeds)
    for (var i = 0; i < needsYou.length; i++) if (needsYou[i].computer_id === id) hidden[id] = needsYou[i].message
    hiddenNeeds = hidden
  }
  // Computers offline or needing attention for a minute or more (StatusModel.lastingProblems):
  // the bar's red dot, each with its toast. `problemStarts` is when each problem began;
  // `hiddenProblems` what was put away, by computer (its words; it shows again once they change).
  property var problemStarts: ({})
  property var hiddenProblems: ({})
  property var problems: []
  function refreshProblems() {
    var now = Date.now(), starts = StatusModel.problemSince(computers, problemStarts, now)
    if (!sameValue(problemStarts, starts)) problemStarts = starts
    var hidden = hiddenProblems, kept = ({})
    for (var id in hidden) if (starts[id]) kept[id] = hidden[id]
    if (!sameValue(hidden, kept)) hiddenProblems = kept
    var next = StatusModel.lastingProblems(computers, starts, now, kept)
    if (!sameValue(problems, next)) problems = next
  }
  onComputersChanged: refreshProblems()
  function hideProblem(computerId) {
    var id = String(computerId || ""), hidden = Object.assign({}, hiddenProblems)
    for (var i = 0; i < problems.length; i++) if (problems[i].computer_id === id) hidden[id] = problems[i].signature
    hiddenProblems = hidden
    refreshProblems()
  }
  // With no console or quick panel open nothing else reads a computer's status, yet the bar shows
  // what is happening from the start: every computer's status is read at most once a minute (its
  // session opened first), and one the bar counts as offline or needing attention on every
  // refresh, each with its own backoff, so a problem that has passed never keeps the bar red.
  readonly property int closedStatusMs: 60000
  function pollClosedStatus() {
    if (panelOpen || denied || daemonDown) return
    var now = Date.now(), polled = Object.assign({}, statusPolledAt), asked = false
    for (var i = 0; i < computers.length; i++) {
      var c = computers[i], id = c.computer_id, state = StatusModel.computerState(c)
      if (c.trust_state !== "verified" || (sessionRetry[id] && now < sessionRetry[id].until)) continue
      if (state !== "offline" && state !== "attention" && c.controller_epoch && now - Number(polled[id] || 0) < closedStatusMs) continue
      if (!c.controller_epoch) { if (!readPending("session-bootstrap:" + id)) requestRead("session-bootstrap:" + id, ["operator-session", "--computer", id], id) }
      else if (!readPending("session-status:" + id)) {
        polled[id] = now
        asked = true
        requestRead("session-status:" + id, ["operator-status", "--computer", id, "--epoch", c.controller_epoch], id, sessionStatusSnapshot(id))
      }
    }
    if (asked) statusPolledAt = polled
  }
  function approvalsFor(computerId) { return approvals.filter(function(item) { return item.computer_id === String(computerId) }) }
  function loadAttention() {
    if (busy["attention"]) return
    setBusy("attention", "reading")
    var args = ["fleet-attention"]
    for (var i = 0; i < attentionFresh.length; i++) args.push("--fresh", attentionFresh[i])
    sideSend(args, { busyKey: "attention" })
  }
  property bool attentionQuick: false
  // Computers in view where an agent works or something waits, while attentionQuick: `--fresh`
  // asks ibarad to read them now rather than answer from its last read of them.
  property var attentionFresh: []
  onPanelOpenChanged: if (!panelOpen) { attentionQuick = false; attentionFresh = [] }
  onAttentionQuickChanged: if (attentionQuick) loadAttention()
  Timer { interval: root.attentionQuick ? 1500 : 10000; repeat: true; running: true; onTriggered: root.loadAttention() }
  // Repair items name the computers whose self-repair needs a person; a computer that is on and
  // not named no longer does.
  function applyAttention(items) {
    if (!sameValue(attention, items)) attention = items
    attentionLoaded = true
    var repairs = ({}), next = null
    for (var i = 0; i < items.length; i++) if (items[i].kind === "repair" && !repairs[items[i].computer_id]) repairs[items[i].computer_id] = items[i]
    for (var id in sessions) {
      var s = sessions[id], item = repairs[id]
      if (!s || (!item && (!s.needs_person || s.connection === "offline"))) continue
      var want = item ? StatusModel.needsPersonView({ fix: item.ref, message: item.summary, code: s.needs_person ? s.needs_person.code : "" }) : null
      if (sameValue(s.needs_person || null, want)) continue
      if (!next) next = Object.assign({}, sessions)
      next[id] = Object.assign({}, s, { needs_person: want })
    }
    if (next) publishSessions(next)
    syncRequestNotices()
  }
  // Approve, Deny or Always Allow, from the console or a desktop notification. One no longer
  // listed (answered elsewhere, or gone) is said so rather than ignored. Always Allow is only for
  // an agent's send, spend or delete step (StatusModel.approvalAsks).
  signal agentAllowed(string computerId, string text)
  function answerApproval(ref, answer, fromNotice, label) {
    var item = null
    for (var i = 0; i < approvals.length; i++) if (approvals[i].ref === ref) item = approvals[i]
    if (["approve", "deny", "always"].indexOf(answer) === -1 || busy["answer:" + ref]) return false
    if (answer === "always" && item && !item.effect) return false
    if (!item || item.unreachable) {
      var name = item ? (item.label || computerLabelFor(item.computer_id)) : label
      var said = item ? name + " isn't answering right now. You can answer once it's back." : "That request" + (label ? " on " + label : "") + " is no longer waiting."
      actionNotice = said
      if (fromNotice === true && !consoleOpen) queueNotice({ title: "ibara", body: said })
      return false
    }
    sendToComputer("operator-answer-attention", item.computer_id, [ref, answer],
      { ref: ref, answer: answer, label: item.label || computerLabelFor(item.computer_id), summary: item.summary, fromNotice: fromNotice === true,
        stopAsking: item.stopAsking === true, failed: function(message) { root.approvalFailed(item, answer, message, fromNotice === true) } }, "answer:" + ref)
    return true
  }
  function approvalFailed(item, answer, message, fromNotice) {
    var verb = answer === "always" ? "always allow" : answer === "approve" ? (item.stopAsking ? "allow" : "approve") : item.stopAsking ? "answer" : "deny"
    var text = "ibara couldn't " + verb + " the request on " + (item.label || computerLabelFor(item.computer_id)) + ". " + message
    reportError(text, item.computer_id, "")
    if (fromNotice && !consoleOpen) queueNotice({ title: "ibara", body: text })
  }
  // An agent's question: answered with one of its options (any short answer when it has none),
  // or dismissed, which tells the agent nobody will answer. The toast leaves once the computer
  // confirms. ibara older than this console on either end can't do either; then Dismiss puts
  // the question away here only (an update of that computer closes it for good).
  function answerQuestion(ref, answer, handBack) {
    var item = questionByRef(ref), text = String(answer || "")
    if (!item || busy["answer:" + ref] || !text.trim() || text.length > 1000) return false
    if (item.options.length && item.options.indexOf(text) === -1) return false
    if (item.unreachable) { actionNotice = (item.label || computerLabelFor(item.computer_id)) + " isn't answering right now. You can answer once it's back."; return false }
    sendToComputer("operator-answer-attention", item.computer_id, [ref, "--answer", text],
      { ref: ref, question: true, answer: text, handBack: handBack || null, label: item.label || computerLabelFor(item.computer_id) }, "answer:" + ref)
    return true
  }
  property var questionHandBacks: []
  function answerAndHandBack(ref, answer) {
    var item = questionByRef(ref), s = item ? sessions[item.computer_id] : null
    if (!s || !holdsControlOn(item.computer_id) || mutating) return false
    return answerQuestion(ref, answer, { id: item.computer_id, epoch: s.controller_epoch,
      endpoint: s.endpoint_id, binding: s.binding_revision, authorization: s.authorization_generation,
      owner: s.owner_name, revision: s.ownership_revision })
  }
  function finishQuestionHandBack() {
    if (mutating || !questionHandBacks.length) return
    var pin = questionHandBacks[0], s = sessions[pin.id]
    questionHandBacks = questionHandBacks.slice(1)
    if (s && holdsControlOn(pin.id) && s.controller_epoch === pin.epoch && s.endpoint_id === pin.endpoint &&
        s.binding_revision === pin.binding && s.authorization_generation === pin.authorization &&
        s.owner_name === pin.owner && s.ownership_revision === pin.revision) handBackFor(pin.id)
    else actionError = computerLabelFor(pin.id) + ": answered, but control changed. Check Hand Back."
    if (!mutating) Qt.callLater(root.finishQuestionHandBack)
  }
  onMutatingChanged: if (!mutating) Qt.callLater(root.finishQuestionHandBack)
  function dismissQuestion(ref) {
    var item = questionByRef(ref)
    if (!item || busy["answer:" + ref]) return false
    if (item.unreachable) { hideQuestion(ref); actionNotice = "Dismissed here. " + (item.label || computerLabelFor(item.computer_id)) + " isn't answering, so its agent hasn't heard."; return true }
    sendToComputer("operator-answer-attention", item.computer_id, [ref, "--dismiss"],
      { ref: ref, question: true, dismiss: true, label: item.label || computerLabelFor(item.computer_id) }, "answer:" + ref)
    return true
  }
  function questionByRef(ref) {
    for (var i = 0; i < questions.length; i++) if (questions[i].ref === ref) return questions[i]
    return null
  }
  function hideQuestion(ref) {
    var hidden = Object.assign({}, hiddenQuestions)
    hidden[ref] = true
    hiddenQuestions = hidden
  }
  // The reply to an answer or a dismiss. An older ibara takes only approve or deny.
  function questionAnswered(route, error, plain) {
    var tooOld = !!error && /Choose approve or deny/.test(String(error.message || "") + " " + plain)
    if (error && route.dismiss && tooOld) {
      hideQuestion(route.ref)
      actionNotice = "Dismissed here. ibara on " + route.label + " or on this computer is too old to close the question itself; updating it closes the question."
    } else if (error) {
      reportError(tooOld ? "ibara on " + route.label + " or on this computer is too old to answer an agent's question from here. Update ibara, or choose Dismiss."
        : "ibara couldn't " + (route.dismiss ? "dismiss" : "answer") + " the question on " + route.label + ". " + plain, "", "")
    } else {
      attention = attention.filter(function(entry) { return entry.ref !== route.ref })
      actionNotice = route.dismiss ? "Dismissed the question on " + route.label + "." : "Answered on " + route.label + ": " + StatusModel.clip(route.answer, 120)
      if (route.handBack) { questionHandBacks = questionHandBacks.concat([route.handBack]); Qt.callLater(root.finishQuestionHandBack) }
    }
    loadAttention()
  }
  // One desktop notification per new approval, with Approve and Deny on it, Always Allow on a
  // send, spend or delete, and Allow and Not Now on an agent's request to stop asking (notify-send --action;
  // a notification server that renders actions shows the buttons). Each waits in its own process,
  // which prints the notification's id, then the action chosen.
  property var approvalNotices: ({})
  property var noticedApprovals: ({})
  function loginNeedsNotification(item) {
    return !item || !item.login || !item.login.assistance || item.login.assistance.notification !== "quiet"
  }
  function desktopRequestEnabled(kind) {
    return !requestsInConsole && dndReady && (!requestDnd || requestBool("popup_dnd")) && requestKindEnabled(kind) &&
      (!requestBool("request_popups") || requestBool("popup_desktop_notifications"))
  }
  function syncRequestNotices() {
    closeStaleApprovalNotices()
    notifyApprovals()
    notifyQuestions()
    notifyLogins()
  }
  function notifyApprovals() {
    if (!desktopRequestEnabled("approval")) return
    var seen = Object.assign({}, noticedApprovals), notices = Object.assign({}, approvalNotices), fresh = false
    for (var i = 0; i < approvals.length; i++) {
      var item = approvals[i]
      if (seen[item.ref] || !requestNeedsAlert(item)) continue
      seen[item.ref] = true
      fresh = true
      var name = item.label || computerLabelFor(item.computer_id)
      var notice = approvalNoticeComponent.createObject(root, { ref: item.ref, computerId: item.computer_id, label: name })
      var actions = item.stopAsking ? ["-A", "approve=Allow", "-A", "deny=Not Now"]
        : ["-A", "approve=Approve", "-A", "deny=Deny"].concat(item.effect ? ["-A", "always=Always Allow"] : [])
      notice.command = ["notify-send", "-a", "ibara", "-p", "-u", "normal", "-t", "15000", "-A", "default=Open"].concat(actions, ["--", name + (item.stopAsking ? " needs your answer" : " needs your approval"), StatusModel.approvalNoticeBody(item, name, sessions[item.computer_id])])
      notice.running = true
      notices[item.ref] = notice
    }
    if (fresh) { noticedApprovals = seen; approvalNotices = notices }
  }
  function notifyQuestions() {
    if (!desktopRequestEnabled("question")) return
    var seen = Object.assign({}, noticedApprovals), notices = Object.assign({}, approvalNotices), fresh = false
    for (var i = 0; i < questions.length; i++) {
      var item = questions[i]
      if (seen[item.ref] || !requestNeedsAlert(item)) continue
      seen[item.ref] = true
      fresh = true
      var name = item.label || computerLabelFor(item.computer_id)
      var notice = approvalNoticeComponent.createObject(root, { ref: item.ref, computerId: item.computer_id, label: name, question: true, options: item.options.slice() })
      var actions = []
      for (var j = 0; item.options.length <= 2 && j < item.options.length; j++) actions.push("-A", "option-" + j + "=" + StatusModel.titleCase(item.options[j]))
      if (!actions.length) actions = ["-A", "open=Open"]
      if (sessions[item.computer_id] && sessions[item.computer_id].interactive_control === "available_if_exclusive") actions.push("-A", "take=Join")
      notice.command = ["notify-send", "-a", "ibara", "-p", "-u", "normal", "-t", "15000", "-A", "default=Open"].concat(actions,
        ["--", name + " needs your answer", StatusModel.requestNoticeBody(item, name, sessions[item.computer_id], item.summary)])
      notice.running = true
      notices[item.ref] = notice
    }
    if (fresh) { noticedApprovals = seen; approvalNotices = notices }
  }
  function approvalNoticeLine(notice, line) {
    var text = String(line || "").trim()
    if (/^[0-9]{1,10}$/.test(text) && !notice.noticeId) { notice.noticeId = text; closeStaleApprovalNotices(); return }
    if (notice.answered) return
    if (text === "open" || text === "default") {
      notice.answered = true
      openRequest({ computer_id: notice.computerId, ref: notice.ref })
    } else if (notice.question && text === "take") {
      if (!takeControlFor(notice.computerId) && !consoleOpen) queueNotice({ title: "ibara", body: actionError || "Join could not start. Open the console to check this computer." })
    } else if (notice.question && /^option-[0-9]+$/.test(text)) {
      var index = Number(text.substring(7))
      if (index >= notice.options.length) return
      notice.answered = true
      if (!answerQuestion(notice.ref, notice.options[index]) && !consoleOpen)
        queueNotice({ title: "ibara", body: "That question on " + notice.label + " could not be answered. Open the ibara console to check whether it is still waiting." })
    } else if (notice.login && text === "defer") {
      notice.answered = true
      hideRequestPopup(notice.ref)
      var deferred = loginByRef(notice.ref)
      if (deferred && deferred.login.assistance) answerLoginAssistance(notice.ref,
        StatusModel.loginDecisions(notice.loginSites, {}, "defer"), notice.loginRevision)
    } else if (notice.login && (text === "share" || text === "share_all" || text === "decline")) {
      notice.answered = true
      var item = loginByRef(notice.ref)
      if (item) answerLogin(notice.ref, StatusModel.loginDecisions(item.login.sites, {}, text), false, true)
      else if (!consoleOpen) queueNotice({ title: "ibara", body: "That request" + (notice.label ? " on " + notice.label : "") + " is no longer waiting." })
    } else if (!notice.login && !notice.question && (text === "approve" || text === "deny" || text === "always")) {
      notice.answered = true
      answerApproval(notice.ref, text, true, notice.label)
    }
  }
  function approvalNoticeClosed(notice) {
    if (!notice.answered) hideRequestPopup(notice.ref)
    if (approvalNotices[notice.ref] === notice) { var notices = Object.assign({}, approvalNotices); delete notices[notice.ref]; approvalNotices = notices }
    notice.destroy()
  }
  // An approval or a login request answered anywhere else, or gone, closes its notification. One
  // from a computer that isn't answering stays open: its request still waits there.
  function closeStaleApprovalNotices() {
    var seen = Object.assign({}, noticedApprovals)
    for (var ref in approvalNotices) {
      var notice = approvalNotices[ref]
      var kind = notice.question ? "question" : notice.login ? "login" : "approval"
      var pending = requestItems.some(function(item) { return item.ref === ref && root.requestNeedsAlert(item) })
      if (notice.answered || !notice.noticeId || (pending && desktopRequestEnabled(kind))) continue
      notice.answered = true
      if (pending) delete seen[ref] // Settings/focus/DND may allow a new notification later.
      var closer = oneShotComponent.createObject(root)
      closer.command = ["busctl", "--user", "call", "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications", "CloseNotification", "u", notice.noticeId]
      closer.running = true
    }
    requestItems.forEach(function(item) { if (!desktopRequestEnabled(item.kind)) delete seen[item.ref] })
    var pendingSeen = ({})
    requestItems.forEach(function(item) { if (seen[item.ref]) pendingSeen[item.ref] = seen[item.ref] })
    noticedApprovals = pendingSeen
  }

  // ---- login sharing (docs/reference.md, Logins). This computer can be the sharing computer: the
  // one whose browser logins come from (`login-settings`, `login-on`, `login-off`). An agent's
  // request for a site's login is an attention item of kind `login`, answered here only while this
  // computer shares (`login-answer`); each answer except Don't Share sets a rule, for that computer
  // or All Computers (`login-rule`). A computer's Logins tab reads its rules (`login-rows`), shares a
  // login with chosen computers (`login-share-with`) and removes one (`login-remove`); Fleet
  // Actions' Sync Logins makes every allowed site Allowed for All Computers (`login-sync`).
  // loginSettings: StatusModel.loginSettingsView, or null until read (and on an ibara without login sharing).
  property var loginSettings: null
  readonly property bool loginsKnown: loginSettings !== null
  readonly property var logins: attention.filter(function(item) { return item.kind === "login" })
  onLoginsChanged: Qt.callLater(root.republishWaiting)
  // The card asking whether agents may use your logins: once the console is set up (it has a
  // computer) and until it is answered, never on a computer that also runs agents.
  readonly property bool loginSetupWanted: !!loginSettings && !loginSettings.decided && !loginSettings.enabled && !loginSettings.targetRole && computers.length > 0
  // The browser and profile Turn On uses: the one chosen here, else the one sharing, else the
  // first supported browser found, with its Default profile when it has one.
  property var loginChoice: null
  readonly property var loginPick: {
    var s = loginSettings, list = s ? s.browsers.filter(function(b) { return b.supported && b.profiles.length }) : []
    var has = function(browser, profile) { return list.some(function(b) { return b.browser === browser && b.profiles.indexOf(profile) !== -1 }) }
    if (loginChoice && has(loginChoice.browser, loginChoice.profile)) return loginChoice
    if (s && has(s.browser, s.profile)) return { browser: s.browser, profile: s.profile }
    if (!list.length) return null
    return { browser: list[0].browser, profile: list[0].profiles.indexOf("Default") !== -1 ? "Default" : list[0].profiles[0] }
  }
  readonly property string loginBrowserName: StatusModel.loginBrowserName(loginSettings, loginSettings && loginSettings.enabled ? loginSettings.browser : loginPick ? loginPick.browser : "")
  // The other computer a login request, or a computer (`computerId`), takes logins from, or "" when
  // it is this one. A request names its sharing computer; a computer pinned to another sharing
  // computer says so even while this one shares. With neither, any computer pinned elsewhere while
  // this one doesn't share.
  function loginSourceElsewhere(item, computerId) {
    var s = loginSettings, own = s && s.enabled ? s.label : ""
    var named = item && item.login ? item.login.sourceLabel : ""
    if (named) return named !== own ? named : ""
    var id = item ? String(item.computer_id || "") : String(computerId || "")
    var byId = s ? s.computers : ({})
    if (id) return byId[id] && byId[id].sourceElsewhere !== own ? byId[id].sourceElsewhere : ""
    if (s && s.enabled) return ""
    for (var other in byId) if (byId[other].sourceElsewhere) return byId[other].sourceElsewhere
    return ""
  }
  // What the last answer left on each request: { ref: { signedOut: [site], decisions } }. A request
  // with a signed-out site stays open until Retry or Don't Share.
  property var loginOutcomes: ({})
  function loginByRef(ref) {
    for (var i = 0; i < logins.length; i++) if (logins[i].ref === ref) return logins[i]
    return null
  }
  // A message about logins for the console: a note, or (error) one that stays until dismissed.
  signal loginNote(string key, string text, bool error)
  // Another computer shares logins now; `label` names it. Turn On asks whether to share from here instead.
  signal loginAnotherSource(string label, string browser, string profile)
  // Sync Logins' plan, from its dry run: { sites, computers, signedOut, older }.
  signal loginSyncPlanned(var plan)
  signal loginRuleSettled(string computerId, string site, string rule, bool ok)

  function loadLoginSettings() {
    if (busy["login-settings"]) return
    setBusy("login-settings", "reading")
    sideSend(["login-settings"], { busyKey: "login-settings" })
  }
  // The console reads its login settings again every 10 s: a site that rejected a shared login
  // shows as soon as ibara learns it.
  Timer { interval: 10000; repeat: true; running: root.loginsKnown && !root.daemonDown; onTriggered: root.loadLoginSettings() }
  function chooseLoginBrowser(browser, profile) { loginChoice = { browser: String(browser), profile: String(profile) } }
  function turnOnLogins(replace) {
    var pick = loginPick
    if (!pick || busy["login-on"]) return false
    setBusy("login-on", "Turning on…")
    sideSend(["login-on", "--browser", pick.browser, "--profile", pick.profile].concat(replace === true ? ["--replace"] : []), { busyKey: "login-on", browser: pick.browser, profile: pick.profile })
    return true
  }
  function loginNotNow() {
    if (busy["login-on"]) return false
    setBusy("login-on", "Saving…")
    sideSend(["login-not-now"], { busyKey: "login-on" })
    return true
  }
  function turnOffLogins() {
    if (busy["login-on"]) return false
    setBusy("login-on", "Turning off…")
    sideSend(["login-off"], { busyKey: "login-on" })
    return true
  }
  // `computerId` is a computer's id or "all" (All Computers).
  function setLoginRule(computerId, site, rule) {
    var key = "login-rule:" + computerId + ":" + site
    if (busy[key] || ["allow", "ask", "deny"].indexOf(rule) === -1 || !StatusModel.loginSite(site)) return false
    setBusy(key, rule)
    sideSend(["login-rule", "--computer", String(computerId), "--site", StatusModel.loginSite(site), "--rule", rule], { busyKey: key, loginComputer: String(computerId), site: StatusModel.loginSite(site), rule: rule })
    return true
  }
  // The open computer's Logins tab.
  property var loginRows: ({ rows: [], deniedAll: [] })
  property string loginRowsComputerId: ""
  property double loginRowsAt: 0
  property string loginRowsError: ""
  function loadLoginRows(computerId) {
    var id = String(computerId || "")
    if (!id || busy["login-rows"]) return
    if (id !== loginRowsComputerId) { loginRows = { rows: [], deniedAll: [] }; loginRowsAt = 0; loginRowsError = ""; loginRowsComputerId = id }
    setBusy("login-rows", "reading")
    sideSend(["login-rows", "--computer", id], { busyKey: "login-rows", loginComputer: id })
  }
  // Share, Share With All Computers, Don't Share or Never Share, per site (StatusModel.loginDecisions).
  // `retry` shares again the sites the person wasn't signed in to.
  function answerLogin(ref, decisions, retry, fromNotice, revision) {
    var item = loginByRef(ref)
    if (!item || busy["answer:" + ref]) return false
    if (item.unreachable) { actionNotice = (item.label || computerLabelFor(item.computer_id)) + " isn't answering right now. You can answer once it's back."; return false }
    sendLogin(["login-answer", "--computer", item.computer_id, "--att", ref, "--decisions", JSON.stringify(decisions)].concat(retry === true ? ["--retry"] : []).concat(revision !== undefined ? ["--revision", String(revision)] : []),
      { busyKey: "answer:" + ref, ref: ref, decisions: decisions, loginComputer: item.computer_id, label: item.label || computerLabelFor(item.computer_id), fromNotice: fromNotice === true })
    return true
  }
  function answerLoginAssistance(ref, decisions, revision) {
    var item = loginByRef(ref)
    if (!item || !item.login.assistance || item.login.assistance.version !== 1 || busy["answer:" + ref]) return false
    if (item.unreachable) { actionError = "That computer is not answering right now."; return false }
    sendLogin(["login-assist", "--computer", item.computer_id, "--att", ref, "--revision", String(revision), "--decisions", JSON.stringify(decisions)],
      { busyKey: "answer:" + ref, ref: ref, decisions: decisions, loginComputer: item.computer_id })
    return true
  }
  function sendLogin(args, route) {
    if (route.busyKey) setBusy(route.busyKey, args[0])
    sideSend(args, route)
  }
  // `to`: computer ids, or "all".
  function shareLoginWith(site, to) {
    var key = "login-share:" + site
    if (busy[key]) return false
    var list = to === "all" ? "all" : (Array.isArray(to) ? to : []).map(String).join(",")
    if (!list) return false
    sendLogin(["login-share-with", "--site", site, "--to", list], { busyKey: key, site: site })
    return true
  }
  function removeLogin(computerId, site) {
    var key = "login-remove:" + computerId + ":" + site
    if (busy[key]) return false
    sendLogin(["login-remove", "--computer", String(computerId), "--site", site], { busyKey: key, loginComputer: String(computerId), site: site })
    return true
  }
  // Sync Logins: its dry run first, whose plan the console confirms (loginSyncPlanned), then the sync.
  function planLoginSync() {
    if (busy["login-sync"]) return false
    sendLogin(["login-sync", "--dry-run"], { busyKey: "login-sync", dryRun: true })
    return true
  }
  function syncLogins() {
    if (busy["login-sync"]) return false
    sendLogin(["login-sync"], { busyKey: "login-sync" })
    return true
  }
  // Sites that rejected a shared login: one toast each, with Join, until put away.
  property var loginRejectedSeen: ({})
  property var loginRejected: []
  readonly property double loginRejectedSince: Date.now() - 600000
  function dismissLoginRejected(key) {
    loginRejected = loginRejected.filter(function(entry) { return entry.key !== key })
  }
  function loginRejectedByKey(key) {
    for (var i = 0; i < loginRejected.length; i++) if (loginRejected[i].key === key) return loginRejected[i]
    return null
  }
  // "Tater4 runs an older ibara. Update ibara there to share logins with it."
  function olderLoginWords(ids) {
    var names = (Array.isArray(ids) ? ids : []).map(function(id) { return computerLabelFor(String(id)) })
    if (!names.length) return ""
    return StatusModel.listWords(names) + (names.length === 1 ? " runs an older ibara. Update ibara there to share logins with it." : " run an older ibara. Update ibara there to share logins with them.")
  }
  function consumeLogin(route, data, error, plain) {
    var op = route.op
    if (op === "login-settings") {
      // An ibara without login sharing refuses the command: loginSettings stays null and nothing
      // about logins shows. A failed read keeps what was read before.
      if (error) return
      var view = StatusModel.loginSettingsView(data)
      if (!sameValue(loginSettings, view)) loginSettings = view
      var fresh = StatusModel.loginRejectedFresh(view.rejected, loginRejectedSeen, loginRejectedSince)
      if (fresh.length) {
        var seen = Object.assign({}, loginRejectedSeen)
        for (var i = 0; i < fresh.length; i++) seen[fresh[i].key] = true
        loginRejectedSeen = seen
        loginRejected = loginRejected.concat(fresh)
      }
      return
    }
    if (op === "login-on") {
      if (error && String(error.code || "") === "ANOTHER_SOURCE") { loginAnotherSource(StatusModel.clip(data.label, 128) || "another computer", route.browser || "", route.profile || ""); return }
      if (error) { reportError(plain || "ibara couldn't turn on login sharing.", "", ""); loadLoginSettings(); return }
      if (route.browser) actionNotice = "Login sharing is on. Logins come from " + StatusModel.loginBrowserName(loginSettings, route.browser) + " on this computer, only when you allow them."
      loadLoginSettings()
    } else if (op === "login-not-now") {
      if (error) { reportError(plain || "ibara couldn't save your answer.", "", ""); return }
      actionNotice = "Login sharing stays off. You can turn it on in the Logins page."
      loadLoginSettings()
    } else if (op === "login-off") {
      if (error) { reportError(plain || "ibara couldn't turn off login sharing.", "", ""); return }
      // The extension leaves the browser when ibara can set it up there; else it says what stays.
      var extension = data.extension_removed === true ? " ibara removed its extension from " + loginBrowserName + "."
        : typeof data.extension_error === "string" && data.extension_error ? " " + StatusModel.clip(data.extension_error, 240)
        : " Its extension stays in " + loginBrowserName + " until you remove it there or turn sharing on and off from a computer ibara can set up."
      actionNotice = "Login sharing is off. Logins already on your computers stay until they run out or are removed." + extension
      loadLoginSettings()
    } else if (op === "login-rule") {
      if (error) reportError(plain || "ibara couldn't change the rule for " + route.site + ".", "", "")
      loginRuleSettled(route.loginComputer, route.site, route.rule, !error)
      loadLoginSettings()
      if (loginRowsComputerId) loadLoginRows(loginRowsComputerId)
    } else if (op === "login-rows") {
      if (route.loginComputer !== loginRowsComputerId) return
      loginRowsError = error ? (plain || "ibara couldn't read this computer's logins.") : ""
      if (!error) { var rows = StatusModel.loginRowsView(data); if (!sameValue(loginRows, rows)) loginRows = rows; loginRowsAt = Date.now() }
    } else if (op === "login-assist") {
      if (error) { reportError(plain || "The request changed. Refresh and choose again.", "", ""); loadAttention(); return }
      actionNotice = data.continuation === "ready_to_resume" ? "Your answer is saved. Ready when the agent resumes." : "Your answer is saved."
      loadAttention()
    } else if (op === "login-answer") {
      var name = route.label
      if (error) {
        var failed = "ibara couldn't answer the login request on " + name + ". " + plain
        reportError(failed, "", "")
        if (route.fromNotice && !consoleOpen) queueNotice({ title: "ibara", body: failed })
        loadAttention()
        return
      }
      var words = StatusModel.loginAnswerWords(data.sites, loginBrowserName, name)
      var outcomes = Object.assign({}, loginOutcomes)
      if (words.signedOut.length) outcomes[route.ref] = { signedOut: words.signedOut, decisions: route.decisions }
      else delete outcomes[route.ref]
      loginOutcomes = outcomes
      if (!words.signedOut.length) attention = attention.filter(function(entry) { return entry.ref !== route.ref })
      if (words.note) actionNotice = words.note
      if (words.waiting) loginNote("login-waiting:" + route.ref, words.waiting, true)
      if (words.unknown) loginNote("login-unknown:" + route.ref, words.unknown, true)
      if (words.off) { loginNote("login-off:" + route.ref, "Login sharing is off, so nothing was shared. Turn it on in the Logins page.", true); loadLoginSettings() }
      if (route.fromNotice && !consoleOpen) {
        var said = [words.note, words.waiting, words.unknown, StatusModel.loginSignedOutWords(words.signedOut, loginBrowserName)].filter(function(t) { return t !== "" }).join(" ")
        if (said) queueNotice({ title: "ibara", body: said })
      }
      loadAttention()
    } else if (op === "login-share-with") {
      if (error) { reportError(plain || "ibara couldn't share the login for " + route.site + ".", "", ""); return }
      var delivered = (Array.isArray(data.delivered) ? data.delivered : []).map(function(id) { return computerLabelFor(String(id)) })
      var deferred = (Array.isArray(data.deferred) ? data.deferred : []).map(function(id) { return computerLabelFor(String(id)) })
      var unknown = (Array.isArray(data.unknown) ? data.unknown : []).map(function(id) { return computerLabelFor(String(id)) })
      var parts = []
      if (delivered.length) parts.push("Shared your login for " + route.site + " with " + StatusModel.listWords(delivered) + ".")
      if (deferred.length) parts.push(StatusModel.listWords(deferred) + (deferred.length === 1 ? " gets" : " get") + " it the first time an agent needs it.")
      var older = olderLoginWords(data.older)
      if (parts.length) actionNotice = parts.join(" ")
      else if (!unknown.length && !older) actionNotice = "Nothing was shared: none of those computers can take it."
      if (unknown.length) loginNote("login-share-unknown:" + route.site, "Couldn't confirm the login for " + route.site + " was written on " + StatusModel.listWords(unknown) + ". Try Share With… again or use Join.", true)
      if (older) loginNote("login-older", older, true)
      loadLoginSettings()
      if (loginRowsComputerId) loadLoginRows(loginRowsComputerId)
    } else if (op === "login-remove") {
      var where = computerLabelFor(route.loginComputer)
      if (error) { reportError(plain || "ibara couldn't remove the login for " + route.site + " from " + where + ".", "", ""); return }
      // removed: how many cookies went now (null while queued); deferred: the rest go when its browser next opens.
      var removedNow = typeof data.removed === "number" && data.removed > 0
      actionNotice = data.deferred === true && !removedNow ? route.site + " is off the list now. It will be removed from " + where + "'s browser when it next opens."
        : data.deferred === true ? "Removed part of the login for " + route.site + " from " + where + ". The rest goes when its browser next opens, and the next request for it asks you again."
        : "Removed the login for " + route.site + " from " + where + ". The next request for it asks you again."
      loadLoginSettings()
      if (loginRowsComputerId) loadLoginRows(loginRowsComputerId)
    } else if (op === "login-sync") {
      if (error) { reportError(plain || "ibara couldn't sync your logins.", "", ""); return }
      var plan = { sites: Math.max(0, Math.floor(Number(data.sites) || 0)), computers: Math.max(0, Math.floor(Number(data.computers) || 0)),
        signedOut: (Array.isArray(data.signed_out) ? data.signed_out : []).map(StatusModel.loginSite).filter(function(s) { return s !== "" }),
        older: Array.isArray(data.older) ? data.older.map(String) : [] }
      if (route.dryRun) { loginSyncPlanned(plan); return }
      actionNotice = "Synced your logins: " + (plan.sites === 1 ? "1 site is" : plan.sites + " sites are") + " Allowed on all " + (plan.computers === 1 ? "1 computer" : plan.computers + " computers") + " now."
      var olderSync = olderLoginWords(plan.older)
      if (olderSync) loginNote("login-older", olderSync, true)
      var unknownSync = (Array.isArray(data.unknown) ? data.unknown : []).map(function(id) { return computerLabelFor(String(id)) })
      if (unknownSync.length) loginNote("login-sync-unknown", "Couldn't confirm the logins were written on " + StatusModel.listWords(unknownSync) + ". Try Sync Logins again or use Join.", true)
      loadLoginSettings()
      if (loginRowsComputerId) loadLoginRows(loginRowsComputerId)
    }
  }
  // One desktop notification per new login request, on the computer that answers it: Share and
  // Don't Share (for every site it names). While no computer shares, it says where to turn
  // sharing on; where another computer shares, it asks there, not here.
  function notifyLogins() {
    if (!desktopRequestEnabled("login")) return
    var seen = Object.assign({}, noticedApprovals), notices = Object.assign({}, approvalNotices), fresh = false
    for (var i = 0; i < logins.length; i++) {
      var item = logins[i]
      var noticeKey = "login:" + requestAlertSignature(item)
      if (seen[item.ref] === noticeKey || !requestNeedsAlert(item)) continue
      if (!item.login.assistance && loginSourceElsewhere(item)) continue
      seen[item.ref] = noticeKey; fresh = true
      var name = item.label || computerLabelFor(item.computer_id)
      var notice = approvalNoticeComponent.createObject(root, { ref: item.ref, computerId: item.computer_id, label: name, login: true,
        loginRevision: item.login.assistance ? item.login.assistance.revision : -1, loginSites: item.login.sites })
      // Sharing/signup scope is reviewed on the card, never a truncated notification.
      var actions = item.login.assistance ? ["-A", "defer=Not now", "-A", "open=Options and details"] : ["-A", "open=Open request"]
      notice.command = ["notify-send", "-a", "ibara", "-p", "-u", "normal", "-t", "15000", "-A", "default=Open"].concat(actions,
        ["--", "Sign-in needed", item.login.sites.map(function(s){return s.site}).join(", ") + " · " + name])
      notice.running = true; notices[item.ref] = notice
    }
    if (fresh) { noticedApprovals = seen; approvalNotices = notices }
  }

  // ---- Pause and Resume a computer's agents. Either can be undone by the other, so each happens
  // at once and the console offers the other as Undo (agentsPaused).
  signal agentsPaused(string computerId, bool paused)
  function pauseAgents(computerId) { return changePause(computerId, "pause") }
  function resumeAgents(computerId) { return changePause(computerId, "resume") }
  function changePause(computerId, op) {
    var id = String(computerId || "")
    if (!sessions[id] || busy["pause:" + id]) return false
    sendToComputer("operator-control", id, ["--op", op], { pauseOp: op }, "pause:" + id)
    return true
  }
  // ---- Fix It: runs the repair a computer asked a person for (repair.needs_person.fix).
  function repair(computerId, fix) {
    var id = String(computerId || "")
    if (!sessions[id] || !StatusModel.fixDescription(fix) || busy["repair:" + id]) return false
    sendToComputer("operator-repair", id, [String(fix)], { fix: String(fix) }, "repair:" + id)
    return true
  }
  // ---- Restart, Shut Down, Sleep, Lock, Update ibara (ibara's signed release) and Update Omarchy
  // (Omarchy's system update), both with nobody at the computer. The console confirms those that
  // can't be undone.
  // `apart`: the answer comes as powerAnswered, one message per computer (Update … on All),
  // instead of the console's one action message.
  signal powerAnswered(string computerId, string action, string text, bool failed)
  function power(computerId, action, apart) {
    var id = String(computerId || "")
    if (!sessions[id] || ["restart", "shutdown", "sleep", "lock", "update_ibara", "update_omarchy"].indexOf(action) === -1 || busy["power:" + id]) return false
    var route = { action: action, tracksIbaraUpdate: sessions[id].ibara_update_supported === true }
    if (apart === true) {
      route.apart = true
      route.failed = function(message) { root.powerSettled(id, action, root.computerLabelFor(id) + " didn't update. " + message, true) }
    }
    sendToComputer("operator-power", id, ["--action", action], route, "power:" + id)
    return true
  }
  function powerSettled(id, action, text, failed) {
    powerAnswered(id, action, text, failed)
    if (!updateAllRun || updateAllRun.action !== action) return
    var waiting = updateAllRun.waiting.filter(function(other) { return other !== id })
    updateAllRun = { action: updateAllRun.action, waiting: waiting, last: updateAllRun.last, baseline: updateAllRun.baseline }
    if (!waiting.length) sendLastUpdate()
  }
  // ---- Update ibara on All and Update Omarchy on All: `action` (update_ibara or update_omarchy)
  // on each of these computers at once, and on `last` (this computer) once every other one has
  // finished (ibara) or answered (Omarchy), so its own update cannot cut the others off.
  // updateAllRun: null, or { action, waiting: [ids not answered yet], last }.
  property var updateAllRun: null
  function updateAll(action, ids, last) {
    if (updateAllRun || ["update_ibara", "update_omarchy"].indexOf(action) === -1) return false
    var sent = []
    for (var i = 0; i < ids.length; i++) if (power(ids[i], action, true)) sent.push(String(ids[i]))
    var baseline = ({})
    for (var i = 0; i < sent.length; i++) {
      var u = sessions[sent[i]] && sessions[sent[i]].ibara_update
      baseline[sent[i]] = u ? String(u.started_at) + ":" + u.state + ":" + String(u.shell) : ""
    }
    updateAllRun = { action: action, waiting: sent, last: String(last || ""), baseline: baseline }
    if (!sent.length) sendLastUpdate()
    return true
  }
  function sendLastUpdate() {
    var last = updateAllRun ? updateAllRun.last : "", action = updateAllRun ? updateAllRun.action : ""
    updateAllRun = null
    if (last) power(last, action, true)
  }
  // ---- Wake: this computer, or another on the same network, sends the wake signal.
  function canWake(computerId) {
    var s = sessions[String(computerId || "")]
    return !!s && !!s.wake && s.connection === "offline"
  }
  function wake(computerId) {
    var id = String(computerId || "")
    if (!sessions[id] || busy["wake:" + id]) return false
    setBusy("wake:" + id, "wake")
    sideSend(["wake", id], { computerId: id, busyKey: "wake:" + id })
    return true
  }
  // ---- Apply Theme to Fleet: this computer's Omarchy theme on every other computer that is on.
  // themeRun: null, or { state: "applying" | "done" | "failed", theme, results, message }.
  property var themeRun: null
  function applyThemeToFleet() {
    if (busy["theme-fleet"]) return false
    setBusy("theme-fleet", "applying")
    themeRun = { state: "applying", theme: "", results: [], message: "" }
    sideSend(["theme-fleet"], { busyKey: "theme-fleet" })
    return true
  }
  function dismissThemeRun() { if (!busy["theme-fleet"]) themeRun = null }
  // ---- While you were away: what happened on each computer since this console last looked.
  property var away: ({ since: 0, computers: [] })
  function loadAway() {
    if (busy["away"]) return
    setBusy("away", "reading")
    sideSend(["away"], { busyKey: "away" })
  }
  function markAwaySeen() {
    if (busy["away-seen"]) return
    setBusy("away-seen", "saving")
    sideSend(["away-seen"], { busyKey: "away-seen" })
  }

  // ---- settings: this console's own (`settings`) and each computer's (`operator-settings`), as
  // the sections ibarad describes. A change applies at once; one that worked is offered back as
  // Undo (settingsApplied, with each setting's value before and after).
  property var consoleSettings: []
  property bool consoleSettingsLoaded: false
  property string consoleSettingsError: ""
  // "computerId:key" (computerId empty for this console) → a plain sentence under that setting.
  property var settingErrors: ({})
  // A change that worked: what changed, and one sentence for the message that offers Undo.
  signal settingsApplied(string computerId, var changes, string text)
  function consoleSetting(key, fallback) { return StatusModel.settingValue(consoleSettings, key, fallback) }
  function consoleBool(key, fallback) { var v = consoleSetting(key, fallback); return v === true || v === "true" }
  // Seconds between pictures of each computer on the fleet page (the Fleet picture interval setting).
  readonly property int tilePreviewMs: Math.max(1, Math.min(60, Number(consoleSetting("fleet_preview_seconds", 5)) || 5)) * 1000
  function loadConsoleSettings() {
    if (busy["settings:get"]) return
    setBusy("settings:get", "reading")
    sideSend(["settings", "get"], { busyKey: "settings:get", settingsAction: "get" })
  }
  function settingsList(computerId) { return computerId ? (String(computerId) === scopedComputerId ? computerSettings : []) : consoleSettings }
  function setSettingError(computerId, key, message) {
    var errors = Object.assign({}, settingErrors), at = String(computerId || "") + ":" + key
    if (message) errors[at] = message
    else if (errors[at] !== undefined) delete errors[at]
    else return
    settingErrors = errors
  }
  function sendSettings(computerId, rest, route) {
    var id = String(computerId || ""), key = route.busyKey
    if (busy[key]) return false
    if (id) sendToComputer("operator-settings", id, rest, route, key)
    else { setBusy(key, rest[0]); sideSend(["settings"].concat(rest), route) }
    return true
  }
  function changeSetting(computerId, key, value, undoing) {
    if (!computerId && isRequestSetting(key)) return changeRequestSetting(key, value, undoing)
    var setting = StatusModel.findSetting(settingsList(computerId), key)
    if (!setting) return false
    setSettingError(computerId, key, "")
    return sendSettings(computerId, ["set", key, String(value)], { busyKey: "setting:" + String(computerId || "") + ":" + key, settingsAction: "set",
      settingKey: key, undoing: undoing === true, before: [{ key: key, title: setting.title, value: StatusModel.settingText(setting.value) }] })
  }
  function resetSetting(computerId, key) {
    if (!computerId && isRequestSetting(key)) { var d = requestSettingDefinitions.filter(function(d) { return d.key === key })[0]; return changeRequestSetting(key, d.fallback) }
    var setting = StatusModel.findSetting(settingsList(computerId), key)
    if (!setting) return false
    setSettingError(computerId, key, "")
    return sendSettings(computerId, ["reset", key], { busyKey: "setting:" + String(computerId || "") + ":" + key, settingsAction: "reset",
      settingKey: key, before: [{ key: key, title: setting.title, value: StatusModel.settingText(setting.value) }] })
  }
  function resetSection(computerId, sectionId) {
    if (!computerId && sectionId === "request_popups") return resetRequestSettings()
    var list = settingsList(computerId), section = null
    for (var i = 0; i < list.length; i++) if (list[i].id === sectionId) section = list[i]
    if (!section) return false
    var before = section.settings.map(function(s) { return { key: s.key, title: s.title, value: StatusModel.settingText(s.value) } })
    return sendSettings(computerId, ["reset", "--section", sectionId], { busyKey: "section:" + String(computerId || "") + ":" + sectionId, settingsAction: "reset", section: section.title, before: before })
  }
  // Undo: each setting back to the value it had. Sent as it is, so it works after the person has
  // left that computer (its settings are only listed while it is open).
  function restoreSettings(computerId, changes) {
    for (var i = 0; i < changes.length; i++) {
      var change = changes[i]
      if (!computerId && isRequestSetting(change.key)) { changeRequestSetting(change.key, change.before, true); continue }
      setSettingError(computerId, change.key, "")
      sendSettings(computerId, ["set", change.key, String(change.before)], { busyKey: "setting:" + String(computerId || "") + ":" + change.key,
        settingsAction: "set", settingKey: change.key, undoing: true, before: [{ key: change.key, title: change.title, value: change.after }] })
    }
  }
  // The keys on their way and the refusals, per setting, for one computer's list ("" for this console).
  function settingsBusyFor(computerId) {
    var prefix = "setting:" + String(computerId || "") + ":", out = ({})
    for (var key in busy) if (key.indexOf(prefix) === 0) out[key.substring(prefix.length)] = busy[key] === "terminal" ? "Finish in the terminal…" : true
    return out
  }
  function settingsErrorsFor(computerId) {
    var prefix = String(computerId || "") + ":", out = ({})
    for (var key in settingErrors) if (key.indexOf(prefix) === 0) out[key.substring(prefix.length)] = settingErrors[key]
    return out
  }
  // A setting's value in words: on or off, a choice's label, or the value itself.
  function settingWords(setting, text) {
    if (!setting) return text
    if (setting.type === "bool") return text === "true" ? "on" : "off"
    if (setting.type === "choice") for (var i = 0; i < setting.choices.length; i++) if (setting.choices[i].value === text) return setting.choices[i].label
    return text === "" ? "blank" : text
  }

  // ---- Live Video (Preview): with the Live Video setting on, a computer whose ibara can stream
  // (its status's `video`) plays video on its fleet card and Screen tab (LiveVideo.qml). Each one
  // showing asks ibarad to open the stream (`video open`) and asks again every 5 s; ibarad closes
  // a stream nobody renews within 10 s, and the last one to let go closes it (`video close`). A
  // stream that fails, stalls or is refused isn't tried again for a minute; pictures carry on.
  readonly property bool liveVideo: consoleBool("live_video", false)
  // False once the video player can't load here (QtMultimedia is missing): pictures only.
  property bool videoPlayerWorks: true
  // "computerId:WxH" → { path, state } from ibarad's last answer for that stream.
  property var videoStreams: ({})
  // holder → computerId, for each LiveVideo showing a stream.
  property var videoHolders: ({})
  // holder → computerId while its video plays: that computer's pictures come every 30 s.
  property var videoPlaying: ({})
  // computerId → ms before which its video isn't tried again.
  property var videoRetryAt: ({})
  // computerId → the plain reason ibarad gave for not streaming it.
  property var videoRefused: ({})
  // Streams asked for and not yet answered, and computers with a stream open; not bound.
  property var videoAsking: ({})
  property var videoOpen: ({})
  property int videoHolderSequence: 0
  function newVideoHolder() { videoHolderSequence += 1; return "video-" + videoHolderSequence }
  function videoAllowed(computerId) {
    var id = String(computerId || ""), s = sessions[id]
    return liveVideo && videoPlayerWorks && panelOpen && !denied && !!s && s.trust_state === "verified" && s.connection === "ready"
      && !!s.controller_epoch && !s.holds_control && !!s.video && s.video.capable === true && nowMs >= Number(videoRetryAt[id] || 0)
  }
  function videoPlayingFor(computerId) {
    for (var holder in videoPlaying) if (videoPlaying[holder] === computerId) return true
    return false
  }
  // The size to stream: the picture's shape inside the shown box, at most 640 × 360, even.
  function videoSize(boxWidth, boxHeight, aspect) {
    if (!(boxWidth > 0 && boxHeight > 0 && aspect > 0)) return null
    var w = boxWidth, h = boxHeight
    if (w / h > aspect) w = h * aspect
    else h = w / aspect
    var scale = Math.min(1, 640 / w, 360 / h)
    w = Math.floor(w * scale / 16) * 16
    h = Math.floor(h * scale / 2) * 2
    return w >= 64 && h >= 36 ? { width: w, height: h } : null
  }
  // A LiveVideo that shows computerId asks for (or renews) its stream.
  function holdVideo(holder, computerId, width, height) {
    var id = String(computerId || "")
    if (!holder || !id) return
    if (videoHolders[holder] !== undefined && videoHolders[holder] !== id) releaseVideo(holder)
    if (!videoAllowed(id)) { releaseVideo(holder); return }
    if (videoHolders[holder] !== id) {
      var holders = Object.assign({}, videoHolders)
      holders[holder] = id
      videoHolders = holders
    }
    var key = id + ":" + width + "x" + height
    if (videoAsking[key]) return
    videoAsking[key] = true
    videoOpen[id] = true
    sideSend(["video", "open", id, "--width", String(width), "--height", String(height)], { videoAction: "open", videoComputer: id, videoKey: key })
  }
  function releaseVideo(holder) {
    var id = videoHolders[holder]
    setVideoPlaying(holder, id, false)
    if (id === undefined) return
    var holders = Object.assign({}, videoHolders)
    delete holders[holder]
    videoHolders = holders
    for (var other in holders) if (holders[other] === id) return
    closeVideo(id)
  }
  function closeVideo(computerId) {
    var streams = {}, changed = false
    for (var key in videoStreams) {
      if (key.indexOf(computerId + ":") === 0) changed = true
      else streams[key] = videoStreams[key]
    }
    if (changed) videoStreams = streams
    if (!videoOpen[computerId]) return
    delete videoOpen[computerId]
    sideSend(["video", "close", computerId], { videoAction: "close", videoComputer: computerId })
  }
  function setVideoPlaying(holder, computerId, playing) {
    if (!holder || (videoPlaying[holder] !== undefined) === playing) return
    var next = Object.assign({}, videoPlaying)
    if (playing) next[holder] = String(computerId)
    else delete next[holder]
    videoPlaying = next
  }
  // Video that failed or stalled on a computer: pictures only, and no video there for a minute.
  function videoFailed(computerId) {
    var id = String(computerId || "")
    var retry = Object.assign({}, videoRetryAt)
    retry[id] = Date.now() + 60000
    videoRetryAt = retry
    for (var holder in videoPlaying) if (videoPlaying[holder] === id) setVideoPlaying(holder, id, false)
    closeVideo(id)
  }
  function consumeVideo(route, data, error) {
    var id = route.videoComputer
    if (route.videoAction !== "open") return
    delete videoAsking[route.videoKey]
    var held = false
    for (var holder in videoHolders) if (videoHolders[holder] === id) held = true
    if (!held) return
    var refused = Object.assign({}, videoRefused)
    if (!error && typeof data.unsupported === "string") {
      refused[id] = StatusModel.clip(data.unsupported, 200) || "This computer can't stream video."
      videoRefused = refused
      videoFailed(id)
      return
    }
    if (error || !StatusModel.videoFile(data.path, id)) { videoFailed(id); return }
    if (refused[id] !== undefined) { delete refused[id]; videoRefused = refused }
    var stream = { path: String(data.path), state: data.state === "playing" ? "playing" : "starting" }
    var known = videoStreams[route.videoKey]
    if (known && known.path === stream.path && known.state === stream.state) return
    var streams = Object.assign({}, videoStreams)
    streams[route.videoKey] = stream
    videoStreams = streams
  }
  // For Settings: each computer that can't stream, with its reason.
  readonly property string videoReasons: {
    var lines = []
    for (var id in sessions) {
      var s = sessions[id]
      var reason = videoRefused[id] || (s && s.video && s.video.capable !== true ? s.video.reason : "")
      if (reason) lines.push(computerLabelFor(id) + ": " + reason)
    }
    return lines.sort().join("\n")
  }

  // ---- files dropped on a computer's card or view. Each goes with the Files tab's send
  // (operator-file-send) into the computer's shared folder, one after another, and shows in
  // Transfers. drops[computerId] = { files: [{path, name}], index, rootId, sent, failed: [{name,
  // message}], state: "starting" | "sending" | "done", current, doneAt }.
  property var drops: ({})
  function setDrop(id, fields) {
    var next = Object.assign({}, drops)
    next[id] = Object.assign({}, drops[id] || {}, fields)
    drops = next
  }
  // A finished drop with nothing left to say leaves the card after a few seconds; one with a
  // failure stays until dismissed.
  function expireDrops() {
    for (var id in drops) if (drops[id].state === "done" && !drops[id].failed.length && Date.now() - drops[id].doneAt > 6000) dismissDrop(id)
  }
  function dismissDrop(computerId) {
    if (!drops[computerId]) return
    var next = Object.assign({}, drops)
    delete next[computerId]
    drops = next
  }
  function dropFiles(computerId, urls) {
    var id = String(computerId || ""), files = [], skipped = 0
    var list = Array.isArray(urls) ? urls : []
    for (var i = 0; i < list.length && files.length < 50; i++) {
      var path = localPath(list[i]), name = humanBaseName(path)
      if (String(list[i]).indexOf("file://") !== 0 || !humanAbsolutePath(path) || !humanFileSegment(name)) { skipped += 1; continue }
      files.push({ path: path, name: name })
    }
    if (!sessions[id]) return false
    if (!files.length) { reportError("ibara sends files you drop from your file manager. " + (skipped ? "What you dropped isn't a file ibara can send." : "Nothing was dropped."), "", ""); return false }
    var current = drops[id]
    if (current && (current.state === "starting" || current.state === "sending")) { setDrop(id, { files: current.files.concat(files) }); return true }
    setDrop(id, { files: files, index: 0, rootId: "", sent: 0, failed: [], state: "starting", current: files[0].name, doneAt: 0 })
    sendToComputer("operator-files", id, ["--op", "files_roots"], { drop: true,
      failed: function(message) { root.finishDrop(id, message) } }, "drop:" + id)
    return true
  }
  function sendNextDrop(id) {
    var d = drops[id]
    if (!d) return
    if (d.index >= d.files.length) { finishDrop(id, ""); return }
    var file = d.files[d.index], s = sessions[id]
    var pin = { phase: "running", notice: "", computerId: id, rootId: d.rootId, remote: file.name, local: file.path, jobId: "",
      transferId: "transfer-" + (++transferSequence), direction: "send", total: 0, openWhenDone: false }
    syncTransfer(pin)
    setDrop(id, { state: "sending", current: file.name })
    sendToComputer("operator-file-send", id, ["--root", d.rootId, "--remote", file.name, "--local", file.path], { drop: true, pin: pin,
      failed: function(message) { root.droppedFileSettled(id, pin, null, message) } }, "drop:" + id)
  }
  function droppedFileSettled(id, pin, data, message) {
    var d = drops[id]
    if (!d) return
    var receipt = data ? humanFileReceipt(data) : null, jobId = data ? humanJobId(data.job_id) : ""
    var ok = !!data && data.state === "verified" && !!receipt && !!jobId && data.computer_id === id && data.root_id === pin.rootId && data.remote_path === pin.remote
    if (ok) {
      var settled = copyHumanPin(pin, "verified", "Verified. Sent " + pin.remote + ". " + receipt.size + " bytes, sha256 " + receipt.sha + ".", jobId)
      settled.receipt = receipt
      syncTransfer(settled)
      setDrop(id, { sent: d.sent + 1, index: d.index + 1 })
    } else {
      var why = message || "What " + computerLabelFor(id) + " reported didn't match what was sent. Look for it in the Files tab before sending it again."
      syncTransfer(copyHumanPin(pin, "failed", "Failed. " + why, ""))
      setDrop(id, { failed: d.failed.concat([{ name: pin.remote, message: why }]), index: d.index + 1 })
    }
    sendNextDrop(id)
  }
  function finishDrop(id, message) {
    var d = drops[id]
    if (!d) return
    var name = computerLabelFor(id)
    if (message) { setDrop(id, { state: "done", doneAt: Date.now(), failed: d.failed.concat([{ name: "", message: message }]) }); reportError("Nothing was sent to " + name + ". " + message, id, ""); return }
    setDrop(id, { state: "done", doneAt: Date.now() })
    if (d.sent) actionNotice = "Sent " + (d.sent === 1 && d.files.length === 1 ? d.files[0].name : d.sent + (d.sent === 1 ? " file" : " files")) + " to " + name + ", in its " + d.rootId + " folder."
    if (d.failed.length) reportError(d.failed.map(function(f) { return f.name + " wasn't sent: " + f.message }).join(" "), id, "")
  }

  // Answers on the side lane for the everyday features above.
  function consumeEveryday(route, data, error, message) {
    var op = route.op, id = String(route.computerId || "")
    if (route.busyKey) setBusy(route.busyKey, "")
    // A per-computer answer names that computer; anything else is not its answer.
    var result = data.result && typeof data.result === "object" ? data.result : ({})
    if (id && !error && data.computer_id !== undefined && data.computer_id !== id) {
      error = { code: "MISMATCH" }
      message = "The answer came from a different computer than " + computerLabelFor(id) + ", so ibara can't tell whether this worked."
    }
    var plain = error ? (id ? plainError(message, id, false) : message) : ""
    if (op === "fleet-attention") {
      if (!error) {
        var unreachable = (Array.isArray(data.unreachable) ? data.unreachable : []).filter(function(c) { return StatusModel.requestId(c) }).map(String)
        if (!sameValue(attentionUnreachable, unreachable)) attentionUnreachable = unreachable
        applyAttention(StatusModel.attentionView(data))
      }
    } else if (op === "operator-answer-attention" && route.question) {
      questionAnswered(route, error, plain)
    } else if (op === "operator-answer-attention") {
      var item = { computer_id: id, label: route.label }
      if (error) { approvalFailed(item, route.answer, plain, route.fromNotice); loadAttention(); return }
      attention = attention.filter(function(entry) { return entry.ref !== route.ref })
      // Always Allow, or Allow on an agent's request to stop asking: what changed, and where to undo it.
      if (result.allowed && typeof result.allowed === "object") {
        var allowedText = StatusModel.allowedNotice(route.label, result.allowed, route.answer === "always")
        agentAllowed(id, allowedText)
        if (route.fromNotice && !consoleOpen) queueNotice({ title: "ibara", body: allowedText })
      }
      else if (route.stopAsking) actionNotice = route.answer === "approve" ? "Allowed on " + route.label + "." : "Nothing changed on " + route.label + ". The agent still asks you first."
      else actionNotice = (route.answer === "approve" ? "Approved" : "Denied") + " on " + route.label + (route.summary ? ": " + StatusModel.clip(route.summary, 120) : ".")
      loadAttention()
    } else if (op === "operator-control") {
      if (error) { reportError(plain || "ibara couldn't " + route.pauseOp + " " + computerLabelFor(id) + ".", id, error); recheckComputer(id); return }
      var paused = result.paused === true
      // A person's pause or resume ends any wait of ibara's own (system_wait).
      updateSession(id, { paused: paused, pause_origin: paused ? String(result.pause_origin || "person") : null, system_wait: null,
        owner_name: typeof result.owner === "string" ? result.owner : sessions[id] && sessions[id].owner_name,
        ownership_revision: result.ownership_revision || (sessions[id] && sessions[id].ownership_revision) })
      agentsPaused(id, paused)
      recheckComputer(id)
    } else if (op === "operator-repair") {
      if (error) { reportError(plain || "Fix It didn't run on " + computerLabelFor(id) + ".", id, error); return }
      var state = String(result.state || "")
      if (state === "fixed" || state === "restarting") {
        updateSession(id, { needs_person: null })
        attention = attention.filter(function(entry) { return !(entry.kind === "repair" && entry.computer_id === id) })
        actionNotice = StatusModel.clip(result.message, 240) || (state === "restarting" ? computerLabelFor(id) + " is restarting ibara. It's back in a few seconds." : computerLabelFor(id) + " is working again.")
        if (state === "restarting") forgetControllerEpoch(id)
      } else {
        reportError(StatusModel.clip(result.message, 240) || computerLabelFor(id) + " still needs you. Restart it, or check it in person.", id, "")
      }
      recheckComputer(id)
      loadAttention()
    } else if (op === "operator-power") {
      var name = computerLabelFor(id), s = sessions[id]
      // An older ibara there doesn't know the update it was asked for: Update ibara needs one update
      // by hand there; Update Omarchy needs Update ibara first.
      var older = !!error && ["update_ibara", "update_omarchy"].indexOf(route.action) !== -1 && error.code === "INVALID_ARGUMENT"
      if (older) plain = name + " runs an older ibara. " + (route.action === "update_ibara" ? "Update it once at that computer (ibara update), then Update ibara works." : "Choose Update ibara first.")
      if (error) {
        if (route.apart && route.action === "update_ibara" && error.retry_safe === false) {
          powerAnswered(id, route.action, "Waiting for the update result on " + name + ".", true)
          recheckComputer(id)
          return
        }
        if (route.apart) powerSettled(id, route.action, older ? plain : name + " didn't update. " + (plain || "ibara couldn't reach it."), true)
        else reportError(plain || "ibara couldn't do that on " + name + ".", id, error)
        return
      }
      var said
      if (route.action === "restart") said = name + " is restarting." + (result.disk_password_warning === true ? " Its disk asks for its password when it starts, so someone there must type it before ibara can reach it again." : " ibara reconnects when it's back.")
      else if (route.action === "shutdown") said = name + " is shutting down." + (s && s.wake ? " Choose Wake to turn it on again." : "")
      else if (route.action === "sleep") said = name + " is going to sleep. Choose Wake to wake it."
      else if (route.action === "lock") said = name + "'s screen is locked."
      else if (route.action === "update_ibara") said = name + ": " + (StatusModel.clip(result.message, 200) || (result.state === "current" ? "ibara is already up to date." : "ibara is updating to the latest release. It may restart its bar when it finishes."))
      else said = name + ": " + (StatusModel.clip(result.message, 200) || (result.state === "running" ? "Omarchy is already updating on this computer." : "Omarchy is updating. The computer stays usable; it says when a restart is needed."))
      var awaitingIbara = route.action === "update_ibara" && ["started", "running"].indexOf(result.state) !== -1
      // Older daemons accept the same plain update, but cannot send a terminal progress record.
      if (route.apart && (!awaitingIbara || !route.tracksIbaraUpdate)) powerSettled(id, route.action, said, result.state === "busy")
      else if (route.action === "update_ibara") powerAnswered(id, route.action, said, result.state === "busy")
      else actionNotice = said
      if (awaitingIbara) {
        updateSession(id, { ibara_update: { state: "running", version: "", started_at: Date.now(), finished_at: 0, message: "" } })
        pollStatus(id)
      }
      // It stops answering now; the card says why, and dims, until it answers again.
      if (["restart", "shutdown", "sleep"].indexOf(route.action) !== -1) updateSession(id, { power_state: route.action, connection: "offline" })
      // Its card says Updating Omarchy… at once; the next status read says how it goes.
      if (route.action === "update_omarchy" && result.state === "started") { updateSession(id, { omarchy_update: { state: "running", finished_at: 0, restart_needed: false, message: "" } }); recheckComputer(id) }
    } else if (op === "wake") {
      if (error) { reportError(plain || "ibara couldn't wake " + computerLabelFor(id) + ".", id, error); return }
      var via = String(data.sent_via || "")
      actionNotice = data.sure === false
        ? "Sent the signal to wake " + computerLabelFor(id) + " from this computer. If " + computerLabelFor(id) + " is on another network, it won't hear it."
        : "Sent " + computerLabelFor(id) + " the signal to wake" + (via && via !== "this computer" ? ", through " + via : "") + ". It shows here once it's on."
      updateSession(id, { power_state: "waking" })
      clearRetryState(id)
    } else if (op === "theme-fleet") {
      if (error) { themeRun = { state: "failed", theme: "", results: [], message: plain || "ibara couldn't apply the theme." }; return }
      var view = StatusModel.themeResultsView(data)
      themeRun = { state: "done", theme: view.theme, results: view.results, message: "" }
    } else if (op === "away") {
      // Older cores have no timeline; the fleet simply shows none.
      if (!error) { var awayNow = StatusModel.awayView(data); if (!sameValue(away, awayNow)) away = awayNow }
    } else if (op === "update-check") {
      if (!error) latestRelease = data
    } else if (op === "whats-new") {
      // Older cores have no What's New; nothing shows.
      whatsNew = !error && typeof data.version === "string" && data.version ? { version: data.version, from: String(data.from || ""), notes: StatusModel.releaseNotes(data, String(data.from || "")) } : null
    } else if (op === "unattended-boot") {
      // Older cores have no such command; the entry simply doesn't show.
      unattendedBootSections = error ? [] : StatusModel.unattendedBootSections(data)
      var shown = StatusModel.findSetting(unattendedBootSections, "unattended_boot")
      if (unattendedBootWanted !== null && shown && shown.value === unattendedBootWanted) settleUnattendedBoot("")
    } else if (op === "unattended-boot-lock") {
      if (error) { setSettingError("", "lock_at_sign_in", plain || "ibara couldn't change the lock at sign-in."); return }
      unattendedBootSections = StatusModel.unattendedBootSections(data)
      // Why starting without the password was refused may have been the lock.
      if (unattendedBootWanted === null) setSettingError("", "unattended_boot", "")
    } else if (op === "away-seen") {
      if (error) { actionError = plain || "ibara couldn't mark these as seen."; return }
      away = { since: Date.now(), computers: [] }
    } else if (op === "settings" || op === "operator-settings") {
      consumeSettings(route, id, id ? result : data, error, plain)
    } else if (op === "operator-files" && route.drop) {
      var roots = error ? null : sanitizedFileRoots(result.roots)
      if (!roots || !roots.length) { finishDrop(id, error ? plain : computerLabelFor(id) + " has no shared folder to receive files."); return }
      var root_ = roots.filter(function(r) { return r.root_id === "transfers" })[0] || roots[0]
      setDrop(id, { rootId: root_.root_id })
      sendNextDrop(id)
    } else if (op === "operator-file-send" && route.drop) {
      droppedFileSettled(id, route.pin, error ? null : data, error ? plain : "")
    } else if (op.indexOf("login-") === 0) {
      consumeLogin(route, data, error, plain)
    }
  }
  function consumeSettings(route, computerId, payload, error, message) {
    var action = route.settingsAction
    if (action === "get" && !computerId) {
      if (error) { consoleSettingsError = message || "ibara couldn't read its settings."; return }
      consoleSettings = StatusModel.settingsSections(payload)
      consoleSettingsError = ""
      consoleSettingsLoaded = true
      return
    }
    // An Undo for a computer whose settings this console no longer lists (the person left it, or
    // its list was cleared) has no setting row to show its answer on.
    var blind = route.undoing && computerId && !settingsList(computerId).length
    if (error) {
      if (route.settingKey && !blind) setSettingError(computerId, route.settingKey, message || "That didn't change.")
      else if (blind) actionError = "ibara couldn't undo " + (route.before[0] ? route.before[0].title : "that change") + " on " + computerLabelFor(computerId) + ". " + (message || "")
      else actionError = message || "ibara couldn't reset these settings."
      return
    }
    // A computer's name is the fleet's name for it too.
    if (computerId && route.before.some(function(b) { return b.key === "name" })) loadComputers()
    if (blind) return
    var list = settingsList(computerId)
    var next = StatusModel.settingsAfter(list, payload)
    if (!computerId) consoleSettings = next
    else if (computerId === scopedComputerId) { computerSettings = next; rememberScoped("computer-settings") }
    var changes = []
    for (var i = 0; i < route.before.length; i++) {
      var now = StatusModel.findSetting(next, route.before[i].key)
      var after = now ? StatusModel.settingText(now.value) : route.before[i].value
      if (after !== route.before[i].value) changes.push({ key: route.before[i].key, title: route.before[i].title, before: route.before[i].value, after: after })
    }
    if (!changes.length || route.undoing) return
    var where = computerId ? " on " + computerLabelFor(computerId) : ""
    var text = route.section ? route.section + " settings" + where + " are back to their defaults."
      : changes[0].title + where + (route.settingsAction === "reset" ? " is back to " : " is now ") + settingWords(StatusModel.findSetting(next, changes[0].key), changes[0].after) + "."
    // Ask before agents send, spend or delete says what it now means, and from Settings it
    // reaches every computer with the next approvals read, which goes now.
    var asking = changes.filter(function(c) { return c.key === "agents_ask_first" || c.key === "ask_first" })[0]
    if (asking && !route.section) text += StatusModel.askFirstMeaning(asking.key, asking.after, StatusModel.findSetting(next, asking.key))
    if (!computerId && changes.some(function(c) { return c.key === "agents_ask_first" })) loadAttention()
    settingsApplied(computerId, changes, text)
  }

  function clearProtectedViews() {
    accessGeneration += 1
    previewGeneration += 1
    previewQueue = []
    releasePreviewLanes()
    clearWatchAcknowledgment()
    var protectedSessions = Object.assign({}, sessions)
    for (var computerId in protectedSessions) if (protectedSessions[computerId])
      protectedSessions[computerId] = Object.assign({}, protectedSessions[computerId], { frame: null, capture_age_ms: null })
    publishSessions(protectedSessions)
    followedTasks = ({})
    attention = []
    for (var i = 0; i < pendingNotices.length; i++) logDroppedFollowNotice(pendingNotices[i], "protected-views-cleared")
    pendingNotices = []
    readQueue = []
    readErrors = ({})
    selectedTaskLoading = false
    selectedProcedureLoading = false
    tasks = []; artifacts = []; procedures = []; accessTable = null; computerSettings = []; computerWindows = ({ workspaces: [], agent: null })
    tasksLoaded = false; artifactsLoaded = false; proceduresLoaded = false; windowsLoaded = false
    selectedTask = null; selectedTaskDetail = null; selectedProcedure = null
    selectedTaskDetailRef = ""
    selectedTaskDetailObservedAt = ""
    selectedTaskRef = ""; selectedArtifactRef = ""; selectedProcedureRef = ""
    health = ({}); logLines = []; actionNotice = ""
    scopedCache = ({}); scopedReadAt = ({})
    clearRemoteFiles()
  }


  function consumeRead(lane, text) {
    var request = activeReads[lane]
    var active = activeReads.slice()
    active[lane] = null
    activeReads = active
    if (!request) { Qt.callLater(root.drainReads); return }
    var kind = request.kind
    try {
      var parsed = StatusModel.parseEnvelope(text)
      if (request.access !== accessGeneration || (parsed.request_id && parsed.request_id !== request.requestId)) {
        if (String(kind).indexOf("operator-files-") === 0) discardRemoteFileRead(kind, request.ref, "Failed. This file list no longer matches the current request. Refresh the folder and try again.")
        return
      }
      // A computer-scoped reply belongs to the scope that asked. Its refusal stays with that
      // computer's tab: it never clears other computers' views.
      if (request.scoped) {
        if (request.scope.generation !== scopeGeneration || request.scope.id !== scopedComputerId) return
        if (parsed.error) {
          var scopedMessage = plainError(parsed.error.message || parsed.error.code || "Could not load this information.", request.scope.id, false)
          var scopedErrors = Object.assign({}, readErrors)
          // A refusal drops everything kept for this computer; any other failure drops that list.
          if (StatusModel.accessDenied(parsed)) {
            forgetComputerCaches(request.scope.id)
            for (var c = 0; c < cachedReadKinds.length; c++) forgetScopedKind(request.scope.id, cachedReadKinds[c])
          } else if (cachedReadKinds.indexOf(kind) !== -1) forgetScopedKind(request.scope.id, kind)
          scopedErrors[kind] = scopedMessage
          readErrors = scopedErrors
          // A restarted computer has a new connection: ibara reconnects, then the tab reads again.
          if (targetEpochChanged(parsed.error)) { forgetControllerEpoch(request.scope.id); Qt.callLater(root.refreshComputerSessions) }
          return
        }
        if (!scopedReplyCurrent(request.scope, parsed)) throw new Error("The reply came from a different computer than the one selected.")
      }
      if (kind.indexOf("session-status:") === 0 && !sameSessionStatusRequest(request)) return
      if (kind.indexOf("follow-task:") === 0 && !sameFollowStatusRequest(request)) return
      if (kind.indexOf("session-status:") === 0 && (StatusModel.accessDenied(parsed) || parsed.error)) {
        var deniedByAccess = StatusModel.accessDenied(parsed), error = parsed.error || ({})
        // ibara on this computer stopped: nothing is known about the computer, so it keeps its
        // last known state (the console says ibara isn't running).
        if (String(error.code || "") === "DAEMON_UNAVAILABLE") return
        // No reply is not a refusal: like a preview that timed out or lost its transport, the
        // computer is offline and keeps its last frame, grayed.
        var unreachable = String(error.code || "") === "TIMEOUT" || /^Selected operator transport (timed out|closed|failed)/.test(String(error.message || ""))
        if (unreachable && sessions[request.ref]) {
          var offlineSessions = Object.assign({}, sessions)
          offlineSessions[request.ref] = Object.assign({}, sessions[request.ref], { connection: "offline", frame_error: plainError(String(error.message || error.code), request.ref, true) })
          publishSessions(offlineSessions)
          deferSessionRetry(request.ref)
          return
        }
        if (deniedByAccess) clearDeniedTarget(request.sessionSnapshot, request.sessionSnapshot.followSnapshot)
        var deniedSession = sessions[request.ref]
        if (deniedSession) {
          invalidateTargetPreviews(request.ref)
          var deniedSessions = Object.assign({}, sessions)
          var epochChanged = targetEpochChanged(error)
          deniedSessions[request.ref] = Object.assign({}, deniedSession, {
            connection: epochChanged ? "loading" : "unauthorized", frame: null, capture_age_ms: null,
            controller_epoch: epochChanged ? "" : deniedSession.controller_epoch,
            active_task_ref: epochChanged ? "" : deniedSession.active_task_ref, active_task: null,
            frame_error: plainError(String(error.message || error.code || "This computer refused access."), request.ref, false)
          })
          publishSessions(deniedSessions)
          clearWatchAcknowledgment(request.ref)
          if (!epochChanged) { deferSessionRetry(request.ref); forgetComputerCaches(request.ref) }
          if (epochChanged) {
            clearTargetEpoch(request.ref, deniedSession.controller_epoch)
            Qt.callLater(root.refreshComputerSessions)
          }
        }
        return
      }
      if (kind.indexOf("follow-task:") === 0 && StatusModel.accessDenied(parsed)) {
        if (sameFollowStatusRequest(request)) {
          var current = sessions[request.ref]
          invalidateTargetPreviews(request.ref)
          var deniedSessions = Object.assign({}, sessions)
          deniedSessions[request.ref] = Object.assign({}, current, {
            connection: "unauthorized", frame: null, capture_age_ms: null, active_task: null,
            frame_error: "You can no longer follow this task, so ibara stopped following it."
          })
          publishSessions(deniedSessions)
          clearWatchAcknowledgment(request.ref)
          clearDeniedTarget(request.followSnapshot, request.followSnapshot)
          forgetComputerCaches(request.ref)
        }
        return
      }
      if (kind.indexOf("follow-task:") === 0 && parsed.error) {
        return
      }
      if (kind.indexOf("operator-files-") === 0) {
        applyRemoteFileRead(request, parsed)
        return
      }
      // One computer refusing to open a session affects only that card, never the station. With
      // ibara on this computer stopped, the computer keeps its last known state.
      if (kind.indexOf("session-bootstrap:") === 0 && parsed.error && String(parsed.error.code || "") === "DAEMON_UNAVAILABLE") {
        settleWaiters(String(request.ref), daemonAbsentMessage)
        return
      }
      if (kind.indexOf("session-bootstrap:") === 0 && (parsed.error || StatusModel.accessDenied(parsed))) {
        var bootError = parsed.error || ({}), bootMessage = String(bootError.message || bootError.code || "ibara couldn't connect to this computer. It will try again.")
        var bootSession = sessions[request.ref]
        if (bootSession) {
          var bootDenied = /^(PERMISSION_DENIED|UNAUTHORIZED|AUTH_REQUIRED|GRANT_REVOKED|OPERATOR_TRANSPORT_UNAVAILABLE)\b/.test(bootMessage) ||
            ["PERMISSION_DENIED", "UNAUTHORIZED", "AUTH_REQUIRED", "GRANT_REVOKED"].indexOf(String(bootError.code || "")) !== -1
          invalidateTargetPreviews(request.ref)
          if (bootDenied) forgetComputerCaches(request.ref)
          var bootSessions = Object.assign({}, sessions)
          bootSessions[request.ref] = Object.assign({}, bootSession, { connection: bootDenied ? "unauthorized" : "offline",
            frame: null, capture_age_ms: null, active_task: null, frame_error: bootDenied ? StatusModel.clip(bootMessage, 200) : plainError(bootMessage, request.ref, true) })
          publishSessions(bootSessions)
          clearWatchAcknowledgment(request.ref)
          deferSessionRetry(request.ref)
          settleWaiters(String(request.ref), bootDenied ? plainError(bootMessage, request.ref, false) : computerLabelFor(request.ref) + " isn't answering. Check that it's on, then try again.")
        }
        return
      }
      if (kind === "directory" && StatusModel.accessDenied(parsed)) {
        connectionState = "unauthorized"
        clearProtectedViews()
        return
      }
      if (parsed.error) {
        if (kind === "pair-requests") pairRequests = keepAccepted([])
        // No directory answer is the one failure that affects the whole console: it shows until the next one works.
        if (kind === "directory") {
          failureCount += 1
          connectionState = String(parsed.connection || "failed")
          // Nothing answering means ibara here is missing or stopped: the fleet then says how to
          // install it, or one toast says it isn't running; neither is a refresh error.
          if (String(parsed.error.code || "") === "DAEMON_UNAVAILABLE") checkInstalled()
          lastError = ibaraMissing || String(parsed.error.code || "") === "DAEMON_UNAVAILABLE" ? "" : plainError(parsed.error.message || parsed.error.code || "", "", true)
        }
        var errors = Object.assign({}, readErrors)
        errors[kind] = String(parsed.error.message || parsed.error.code || "Could not load this information.")
        readErrors = errors
        return
      }
      var data = parsed.data || ({})
      // A computer's answer over its pairing route is in `result`.
      var result = data.result && typeof data.result === "object" && !Array.isArray(data.result) ? data.result : ({})
      if (kind.indexOf("session-status:") === 0) {
        var identity = sessions[request.ref]
        var authenticated = data.result || ({})
        if (identity && identity.controller_epoch && data.computer_id === request.ref && data.endpoint_id === identity.endpoint_id && data.binding_revision === identity.binding_revision && data.controller_epoch === identity.controller_epoch && authenticated.endpoint_id === identity.endpoint_id && authenticated.controller_epoch === identity.controller_epoch && authenticated.authorization_generation === identity.authorization_generation) {
          var statusSessions = Object.assign({}, sessions)
          if (authenticated.observation === "denied") {
            statusSessions[request.ref] = Object.assign({}, identity, {
              connection: "unauthorized", observation: "denied", outputs: [],
              active_task_ref: "", active_task: null, frame: null, capture_age_ms: null,
              frame_error: "This computer doesn't let you watch its screen. Its Access tab shows who can."
            })
            publishSessions(statusSessions)
            invalidateTargetPreviews(request.ref)
            clearWatchAcknowledgment(request.ref)
          } else {
          var outputs = Array.isArray(authenticated.outputs) ? authenticated.outputs.filter(function(output) {
            return output && /^[A-Za-z0-9_.:-]{1,128}$/.test(String(output.display_id || ""))
          }).slice(0, 16) : []
          var displayId = identity.display_id
          if (!outputs.some(function(output) { return output.display_id === displayId })) displayId = outputs.length === 1 ? outputs[0].display_id : ""
          statusSessions[request.ref] = Object.assign({}, identity, {
            connection: "ready", observation: authenticated.observation, outputs: outputs,
            interactive_control: authenticated.interactive_control,
            turn: authenticated.turn, turn_holder: authenticated.turn_holder, turn_since_ms: authenticated.turn_since_ms,
            screen_engine: authenticated.screen_engine, fallback_reason: authenticated.fallback_reason,
            owner_name: authenticated.owner, ownership_revision: authenticated.ownership_revision,
            holds_control: authenticated.holds_control === true, holds_control_epoch: identity.controller_epoch,
            active_task_ref: /^[A-Za-z0-9_.:-]{1,128}$/.test(String(authenticated.active_task_ref || "")) ? authenticated.active_task_ref : "",
            active_task: sanitizedActiveTask(authenticated.active_task, authenticated.active_task_ref),
            last_task: sanitizedLastTask(authenticated.last_task),
            files_access: ["available_if_root_approved", "denied"].indexOf(String(authenticated.files || "")) !== -1 ? String(authenticated.files) : "",
            display_id: displayId,
            frame: displayId === identity.display_id ? identity.frame : null,
            frame_error: !displayId ? (outputs.length ? "Choose a display on this computer's Screen tab." : noDisplayNotice)
              : displayId === identity.display_id ? (identity.connection === "unauthorized" ? "Preview pending" : identity.frame_error) : "Preview pending",
            // Paused by a person waits for Resume; paused by ibara itself resumes on its own.
            paused: authenticated.paused === true,
            pause_origin: ["person", "system"].indexOf(authenticated.pause_origin) !== -1 ? authenticated.pause_origin : null,
            // Why agents wait while only ibara paused it; "resume_off" waits for Resume.
            system_wait: ["starting", "settling", "needs_person", "resume_off"].indexOf(authenticated.system_wait) !== -1 ? authenticated.system_wait : null,
            needs_person: StatusModel.needsPersonView(authenticated.repair && authenticated.repair.needs_person),
            wake: authenticated.wake !== undefined ? StatusModel.wakeView(authenticated.wake) : identity.wake || null,
            power_state: "",
            // Its screen is locked: agents can't use it until a person unlocks it with Join.
            locked: authenticated.locked === true,
            disk_password: authenticated.disk_password !== undefined ? authenticated.disk_password === true : identity.disk_password === true,
            omarchy_update: StatusModel.omarchyUpdateView(authenticated.omarchy_update),
            ibara_update: StatusModel.ibaraUpdateView(authenticated.ibara_update),
            ibara_update_supported: authenticated.ibara_update !== undefined,
            version: StatusModel.clip(authenticated.version, 64),
            video: StatusModel.videoView(authenticated.video)
          })
          publishSessions(statusSessions)
          clearRetryState(request.ref)
          noteStatusMoments(request.ref, identity, statusSessions[request.ref])
          if (identity.screen_engine === "ibara" && authenticated.screen_engine === "sunshine" && authenticated.fallback_reason && viewerOpenOn(request.ref)) {
            var fallbackWaiting = Object.assign({}, pendingFallbackViewers)
            fallbackWaiting[request.ref] = true
            pendingFallbackViewers = fallbackWaiting
          }
          }
        } else if (identity && data.computer_id === request.ref && (identity.active_task_ref || identity.active_task)) {
          var staleSessions = Object.assign({}, sessions)
          staleSessions[request.ref] = Object.assign({}, identity, { active_task_ref: "", active_task: null })
          publishSessions(staleSessions)
        }
      }
      else if (kind.indexOf("follow-task:") === 0) {
        var expectedFollow = request.followSnapshot
        var follow = followedTasks[request.ref], current = sessions[request.ref], task = data.result || ({})
        if (!sameFollowIdentity(follow, expectedFollow)) return
        if (!current || expectedFollow.computerId !== request.ref || data.computer_id !== expectedFollow.computerId ||
            data.endpoint_id !== expectedFollow.endpointId || data.binding_revision !== expectedFollow.bindingRevision ||
            data.controller_epoch !== expectedFollow.epoch || current.endpoint_id !== expectedFollow.endpointId ||
            current.binding_revision !== expectedFollow.bindingRevision ||
            current.authorization_generation !== expectedFollow.authorizationGeneration || current.controller_epoch !== expectedFollow.epoch ||
            task.endpoint_id !== expectedFollow.endpointId || task.controller_epoch !== expectedFollow.epoch ||
            task.authorization_generation !== expectedFollow.authorizationGeneration || task.task_ref !== expectedFollow.taskRef) {
          unfollowTask(request.ref)
          return
        }
        if (task.terminal === true && ["completed", "partial", "cancelled", "blocked", "unverified"].indexOf(task.state) !== -1) {
          var verifiedComplete = task.verified_complete === true
          var completionNotice = task.state === "completed" && verifiedComplete
          var willSendNotice = completionNotice && consoleBool("notifications", true)
          console.info(root.notificationTracePrefix("follow-terminal") +
            " computer=" + root.notificationTraceToken(request.ref) +
            " task_ref=" + root.notificationTraceToken(follow.taskRef) +
            " state=" + task.state + " verified=" + (verifiedComplete ? "true" : "false") +
            " notice=" + (willSendNotice ? "verified-completion" : "none"))
          unfollowTask(request.ref)
          if (completionNotice)
            queueNotice({ title: String(current.label || "ibara"),
              body: followedTaskName(current, follow.taskRef) + " finished on " + String(current.label || request.ref) + ", and ibara checked it.",
              computerId: request.ref, endpointId: follow.endpointId, bindingRevision: follow.bindingRevision,
              authorizationGeneration: follow.authorizationGeneration, epoch: follow.epoch,
              trace: { computerId: request.ref, taskRef: follow.taskRef, state: task.state,
                verified: verifiedComplete, notice: "verified-completion" } })
        } else if (task.needs_attention === true && ["waiting_for_human", "interrupted"].indexOf(task.state) !== -1 && follow.lastAttention !== task.state) {
          var nextFollows = Object.assign({}, followedTasks)
          nextFollows[request.ref] = Object.assign({}, follow, { lastAttention: task.state })
          followedTasks = nextFollows
          queueNotice({ title: String(current.label || "ibara"),
            body: followedTaskName(current, follow.taskRef) + (task.state === "waiting_for_human" ? " is waiting for you" : " was interrupted") + " on " + String(current.label || request.ref) + ". Open its Activity tab in the ibara console.",
            computerId: request.ref, endpointId: follow.endpointId, bindingRevision: follow.bindingRevision,
            authorizationGeneration: follow.authorizationGeneration, epoch: follow.epoch })
        }
      }
      else if (kind.indexOf("session-bootstrap:") === 0) {
        var bootstrap = data.result || ({})
        var candidate = sessions[request.ref]
        if (candidate && data.computer_id === request.ref && data.endpoint_id === candidate.endpoint_id &&
            data.binding_revision === candidate.binding_revision &&
            bootstrap.endpoint_id === candidate.endpoint_id &&
            bootstrap.authorization_generation === candidate.authorization_generation &&
            bootstrap.controller_epoch === data.controller_epoch)
          bindSessionEpoch(request.ref, candidate.endpoint_id, candidate.binding_revision,
                           data.controller_epoch, candidate.authorization_generation)
      }
      else if (kind === "directory") {
        var rows = StatusModel.listOf(data, "computers")
        var updated = ({}), joined = []
        for (var c = 0; c < rows.length; c++) {
          var row = rows[c]
          if (!row || typeof row.computer_id !== "string" || !row.computer_id) continue
          var previous = sessions[row.computer_id] || ({})
          if (directoryLoaded && !sessions[row.computer_id]) joined.push(row.computer_id)
          updated[row.computer_id] = Object.assign({}, previous, {
            computer_id: row.computer_id, endpoint_id: row.endpoint_id,
            binding_revision: row.binding_revision,
            authorization_generation: row.authorization_generation,
            controller_epoch: previous.controller_epoch || "",
            display_id: previous.display_id || "",
            label: row.label, host: typeof row.host === "string" ? row.host : "", trust_state: row.trust_state,
            operator_principal: /^[a-z][a-z0-9_-]{0,31}$/.test(String(row.user || "")) ? String(row.user) : "",
            connection: row.trust_state === "verified" ? (previous.connection || "loading") : "unverified",
            owner: previous.owner || ({}), work: previous.work || [],
            frame: previous.frame || null, frame_error: previous.frame ? String(previous.frame_error || "") : (previous.frame_error || "Preview unavailable"),
            capture_age_ms: previous.capture_age_ms == null ? null : previous.capture_age_ms,
            // How to wake it when it is off or asleep, as recorded when it last answered.
            wake: row.wake !== undefined ? StatusModel.wakeView(row.wake) : previous.wake || null
          })
          if (previous.endpoint_id && (previous.endpoint_id !== row.endpoint_id || previous.binding_revision !== row.binding_revision || previous.authorization_generation !== row.authorization_generation)) {
            updated[row.computer_id].frame = null
            updated[row.computer_id].capture_age_ms = null
            updated[row.computer_id].work = []
            updated[row.computer_id].active_task = null
            updated[row.computer_id].controller_epoch = ""
            updated[row.computer_id].frame_error = "This computer's pairing changed. ibara is reconnecting."
          }
        }
        publishSessions(updated, rows)
        if (joined.length) {
          var arrived = Object.assign({}, arrivals)
          for (var a = 0; a < joined.length; a++) arrived[joined[a]] = Date.now()
          arrivals = arrived
        }
        pruneComputerCaches()
        directoryLoaded = true
        failureCount = 0
        connectionState = "ready"
        ibaraMissing = ""
        lastError = ""
        // One publication per refresh: two assignments rebuild the wall twice and lose its keyboard position.
        if (selectedComputerId && !sessions[selectedComputerId]) selectedComputerId = ""
        Qt.callLater(root.refreshComputerSessions)
      }
      else if (kind === "tasks") { tasks = StatusModel.listOf(result, "tasks"); tasksLoaded = true }
      else if (kind === "artifacts") { artifacts = StatusModel.listOf(result, "items").filter(function(item) { return item && typeof item.artifact_ref === "string" }); artifactsLoaded = true }
      else if (kind === "procedures") {
        procedures = StatusModel.listOf(result, "items").filter(function(item) { return item && typeof item.procedure_ref === "string" })
        proceduresLoaded = true
      }
      else if (kind === "health") { health = result; applyHealth(request.scope.id, result) }
      else if (kind === "logs") logLines = (Array.isArray(result.lines) ? result.lines : []).slice(-200).map(function(line) { return String(line).slice(0, 400) })
      else if (kind === "access") accessTable = result
      else if (kind === "computer-settings") computerSettings = StatusModel.settingsSections(result)
      // Published only when it changes, so the rows (and the keyboard on them) stay put between reads.
      else if (kind === "windows") {
        if (windowsHeld) return
        var windowsNow = { workspaces: StatusModel.listOf(result, "workspaces").filter(function(space) { return !!space && typeof space === "object" && Array.isArray(space.windows) }),
          agent: result.agent && typeof result.agent === "object" ? result.agent : null }
        if (!sameValue(computerWindows, windowsNow)) computerWindows = windowsNow
        windowsLoaded = true
      }
      // First-run lists are polled; each is published only when it changes, so the rows on
      // screen (and the keyboard focus on them) stay put between polls.
      else if (kind === "tailnet") { var tailnetNow = StatusModel.tailnetView(data); if (!sameValue(tailnet, tailnetNow)) tailnet = tailnetNow }
      else if (kind === "invites") {
        var invitesNow = StatusModel.invitesView(data)
        if (!sameValue(invites, invitesNow.invites)) invites = invitesNow.invites
        if (!sameValue(shareTailscale, invitesNow.tailscale)) shareTailscale = invitesNow.tailscale
        invitesLoaded = true
      }
      else if (kind === "pair-requests") {
        var requestsNow = keepAccepted(StatusModel.pairRequestsView(data))
        if (!sameValue(pairRequests, requestsNow)) pairRequests = requestsNow
        notifyPairRequests()
      }
      else if (kind === "connect-prompt") {
        var promptNow = StatusModel.connectPromptView(data)
        // Read before as not yet: an agent has just begun its first task here.
        var firstNow = connectPrompt !== "" && !firstTaskDone && promptNow.first_task_done
        connectPrompt = promptNow.prompt
        firstTaskDone = promptNow.first_task_done
        if (firstNow) announceFirstAgent()
      }
      else if (kind === "task" && request.ref === selectedTaskRef) {
        var detail = result
        var actualRef = detail.task && detail.task.task_ref || detail.task_ref || request.ref
        if (String(actualRef) !== request.ref) throw new Error("Task detail did not match the selected task.")
        selectedTaskDetail = detail
        selectedTask = detail.task || detail
        selectedTaskDetailRef = request.ref
        selectedTaskDetailObservedAt = String(parsed.observed_at || "")
        selectedTaskLoading = false
        var taskErrors = Object.assign({}, readErrors)
        delete taskErrors.task
        readErrors = taskErrors
      }
      else if (kind === "procedure" && request.ref === selectedProcedureRef) {
        var procedure = result.procedure || result
        if (procedure.procedure_ref && String(procedure.procedure_ref) !== request.ref) throw new Error("Procedure detail did not match the selected procedure.")
        selectedProcedure = procedure.procedure_ref ? procedure : null
        selectedProcedureLoading = false
      }
      if (request.scoped) rememberScoped(kind)
    } catch (error) {
      var failure = Object.assign({}, readErrors)
      failure[kind] = StatusModel.clip(error, 200)
      readErrors = failure
      if (request.scoped && request.scope.id === scopedComputerId && cachedReadKinds.indexOf(kind) !== -1) forgetScopedKind(request.scope.id, kind)
      if (String(kind).indexOf("operator-files-") === 0) {
        delete failure[kind]
        readErrors = failure
        discardRemoteFileRead(kind, request.ref, "Failed. ibara couldn't read the file list. Choose Refresh to try again.")
      }
    } finally {
      if (kind === "task" && request.ref === selectedTaskRef && !readPending("task")) selectedTaskLoading = false
      if (kind === "procedure" && request.ref === selectedProcedureRef && !readPending("procedure")) selectedProcedureLoading = false
      Qt.callLater(root.drainReads)
    }
  }

  function consumeAction(text) {
    mutating = false
    var kind = pendingMutation
    pendingMutation = ""
    pendingViewerComputerId = ""
    var control = pendingControl
    if (kind === "operator-take-control" || kind === "operator-handback") pendingControl = null
    var scope = actionScope
    actionScope = null
    var windowOp = String(kind).indexOf("window-") === 0 ? windowAction : null
    if (windowOp) windowAction = null
    try {
      var parsed = StatusModel.parseEnvelope(text)
      if (parsed.request_id && parsed.request_id !== actionRequestId) {
        if (String(kind).indexOf("human-file-") === 0) settleHumanFile(kind, null, "unmatched")
        return
      }
      if (actionAccessGeneration !== accessGeneration) {
        if (String(kind).indexOf("human-file-") === 0) settleHumanFile(kind, null, "stale-access")
        return
      }
      // An action for one computer reports only on that computer.
      if (scope) {
        if (parsed.error) {
          if (windowOp) {
            var windowCode = String(parsed.error.code || "")
            if (scope.id === scopedComputerId) loadWindows()
            // Already gone, or an agent is using it: the tab says so beside the window, not in a toast.
            if (windowCode === "NOT_FOUND" || windowCode === "BUSY") { windowActionSettled(scope.id, windowOp.address, windowCode === "BUSY" ? "busy" : "gone"); return }
            windowActionSettled(scope.id, windowOp.address, "failed")
          }
          reportError(plainError(parsed.error.message || parsed.error.code || "Action failed", scope.id, false) +
            (parsed.error.retry_safe === false ? " It may have partly happened; check this tab before trying again." : ""), scope.id, parsed.error)
          if (targetEpochChanged(parsed.error)) forgetControllerEpoch(scope.id)
          if (String(kind).indexOf("access-") === 0) accessChangeSettled(scope.id, false)
          return
        }
        var answer = parsed.data || ({}), done = answer.result && typeof answer.result === "object" ? answer.result : ({})
        if (answer.computer_id !== scope.id) {
          actionError = "The answer came from a different computer than the one open, so ibara can't confirm this worked. Check this tab before trying again."
          if (String(kind).indexOf("access-") === 0) accessChangeSettled(scope.id, false)
          return
        }
        actionError = ""
        actionNotice = kind === "fetch" ? (typeof done.local_path === "string" ? "Saved to " + done.local_path + "." : "Saved in " + downloadFolder + ".") : ""
        // A held request is answered like any other action's: its approval arrives with the others.
        if (windowOp) {
          var windowOutcome = done.state === "pending_approval" ? (kind === "window-close" ? "pending-close" : "pending-move")
            : kind === "window-close" ? (done.closed === true ? "closed" : "open")
            : done.moved === true ? "moved" : "failed"
          if (windowOutcome === "closed") actionNotice = "Closed " + StatusModel.clip(windowOp.title, 80) + "."
          // As Join does: the approval arrives with the others, and the person asks again after it.
          if (windowOutcome.indexOf("pending-") === 0) {
            actionNotice = "Waiting for approval. After approval, choose " + (kind === "window-close" ? "Close" : "Move To…") + " again."
            loadAttention()
          }
          if (scope.id === scopedComputerId) loadWindows()
          windowActionSettled(scope.id, windowOp.address, windowOutcome)
        }
        if (kind.indexOf("access-") === 0) {
          if (scope.generation === scopeGeneration) loadAccess()
          accessChangeSettled(scope.id, true)
        }
        if (scope.id === scopedComputerId && (kind === "extend" || kind === "revoke")) { loadTasks(); if (selectedTaskRef) inspectTask(selectedTaskRef) }
        // A stopped task frees its windows: the Windows tab shows them at once.
        if (scope.id === scopedComputerId && kind === "revoke" && windowsLoaded) loadWindows()
        if (scope.id === scopedComputerId && kind.indexOf("-procedure") !== -1) loadProcedures()
        // Ownership may have changed (a task ended): re-read this computer's own status.
        recheckComputer(scope.id)
        return
      }
      if (parsed.error) {
        if (String(kind).indexOf("human-file-") === 0) settleHumanFile(kind, parsed, "error")
        reportError(plainError(parsed.error.message || parsed.error.code || "Action failed", control ? control.computer_id : "", false) +
          (parsed.error.retry_safe === false ? " It may have partly happened; check the page you started it from before trying again." : ""), control ? control.computer_id : "", parsed.error)
        return
      }
      if ((kind === "operator-take-control" || kind === "operator-handback") && control) {
        var proof = parsed.data || ({}), reply = proof.result || ({}), current = sessions[control.computer_id]
        if (!current || proof.computer_id !== control.computer_id || proof.endpoint_id !== control.endpoint_id ||
            proof.binding_revision !== control.binding_revision || proof.controller_epoch !== control.controller_epoch ||
            reply.endpoint_id !== control.endpoint_id || reply.controller_epoch !== control.controller_epoch ||
            reply.authorization_generation !== control.authorization_generation || (!reply.ownership_revision && reply.state !== "pending_approval")) {
          actionError = "ibara couldn't confirm who has control of " + computerLabelFor(control.computer_id) + ". Its header shows Hand Back once you have control; ibara is checking again."
          return
        }
        if (reply.state === "pending_approval") {
          actionNotice = "Waiting for approval. After approval, choose Join again."
          loadAttention()
          return
        }
        var refreshed = Object.assign({}, sessions)
        refreshed[control.computer_id] = Object.assign({}, current, {
          owner_name: reply.owner, ownership_revision: reply.ownership_revision,
          screen_engine: reply.screen_engine || current.screen_engine,
          turn: reply.screen_engine === "ibara" ? (reply.turn || current.turn || "agent") : current.turn,
          holds_control: reply.screen_engine === "ibara" ? reply.turn === "person" : kind === "operator-take-control" && reply.viewer_ready === true,
          holds_control_epoch: control.controller_epoch
        })
        publishSessions(refreshed)
        // The viewer Join opened; Hand Back closes it.
        noteViewer(control.computer_id, kind === "operator-take-control" && proof.viewer_started === true ? proof.viewer_pid : 0)
        if (reply.fallback_reason) actionNotice = "Using Sunshine: " + StatusModel.clip(String(reply.fallback_reason), 180)
        if (kind === "operator-handback") keepControl(control.computer_id)
        // Hand Back lets agents work again (owner "none") unless someone paused them: a person
        // (before or while holding control), or ibara while it settles the computer.
        actionNotice = reply.fallback_reason ? "Using Sunshine: " + StatusModel.clip(String(reply.fallback_reason), 180) : kind === "operator-take-control" ?
          (reply.screen_engine === "ibara" ? computerLabelFor(control.computer_id) + ": watching. Click, scroll, or press a key for your turn." : computerLabelFor(control.computer_id) + ": you have control.") :
          reply.owner === "none" ? "You handed back control. Its agents can work again." :
          reply.pause_origin === "system" ? "You handed back control. Its agents stay paused until ibara has settled the computer." :
          "You handed back control. Its agents stay paused because a person paused them; choose Resume to let them work again."
        if (kind === "operator-take-control" && proof.viewer_started !== true) actionError = computerLabelFor(control.computer_id) + ": the viewer didn't open. Try Open Viewer."
        return
      }
      actionError = ""
      actionNotice = ""
      if (kind === "open-viewer" && parsed.data && parsed.data.viewer_started === true) noteViewer(parsed.data.computer_id, parsed.data.viewer_pid)
      if (kind === "open-viewer") actionNotice = "Viewer opened. Closing it does not hand back."
      if (kind === "open-terminal") actionNotice = "A terminal opened, signed in to that computer."
      if (kind === "rename-computer" && parsed.data && parsed.data.computer) {
        actionNotice = "Renamed to " + String(parsed.data.computer.label || "") + "."
        loadComputers()
      }
      if (kind === "remove-computer" && parsed.data && parsed.data.removed) {
        actionNotice = "Removed " + String(parsed.data.removed.label || "the computer") + " from your fleet. You can add it again from Add Computer."
        loadComputers()
        loadTailnet()
      }
      if (String(kind).indexOf("human-file-") === 0) settleHumanFile(kind, parsed, "ok")
      refresh()
    } catch (error) {
      actionError = StatusModel.clip(error, 200)
      if (String(kind).indexOf("human-file-") === 0) settleHumanFile(kind, null, "unreadable")
    }
  }


  function schedulePoll(delay) {
    pollTimer.interval = Math.max(1000, delay)
    pollTimer.restart()
  }


  Component.onCompleted: {
    root.notificationTraceRootOrdinal = ServiceBridge.allocateRootOrdinal()
    console.info(root.notificationTracePrefix("root-start"))
    ServiceBridge.publish(root)
    connectDaemon()
    refresh()
    loadConsoleSettings()
    readRequestDnd()
    loadAttention()
    loadLoginSettings()
  }
  Component.onDestruction: ServiceBridge.clear(root)

  Timer {
    id: clock
    interval: 1000
    repeat: true
    running: true
    onTriggered: { root.nowMs = Date.now(); root.expireStalePreviews(); root.expireDrops(); root.refreshProblems() }
  }
  Timer {
    interval: 5000; repeat: true; running: true
    onTriggered: {
      root.pollFollowedTasks()
      // Also re-check any computer this operator holds, so replacement or a restart shows promptly.
      var ids = [root.selectedComputerId]
      if (root.updateAllRun && root.updateAllRun.action === "update_ibara") ids = ids.concat(root.updateAllRun.waiting)
      for (var updating in root.sessions) if (root.sessions[updating].ibara_update && root.sessions[updating].ibara_update.state === "running" && ids.indexOf(updating) === -1) ids.push(updating)
      for (var id in root.sessions) if (root.holdsControlOn(id) && ids.indexOf(id) === -1) ids.push(id)
      for (var i = 0; i < ids.length; i++) if (ids[i]) root.pollStatus(ids[i])
    }
  }

  Timer {
    id: pollTimer
    interval: 15000
    repeat: false
    onTriggered: root.refresh()
  }

  // Offer every visible computer each tick, starting from a rotating cursor so no card is always
  // first; queuePreview admits only those due (the Fleet picture interval setting) and not busy.
  Timer {
    interval: 150
    repeat: true
    running: root.consoleOpen && root.visibleComputerIds.length > 0
    onTriggered: {
      var visible = root.visibleComputerIds
      if (!visible.length) return
      root.tileCursor = (root.tileCursor + 1) % visible.length
      for (var i = 0; i < visible.length; i++) root.queuePreview(visible[(root.tileCursor + i) % visible.length], root.wallQuality)
    }
  }

  // A short tick keeps Watch at one frame per second without waiting a whole
  // extra period after a slow reply; queuePreview enforces the 1 s spacing.
  Timer {
    interval: 200
    repeat: true
    running: root.consoleOpen && root.watchVisible && root.selectedComputerId !== ""
    onTriggered: root.queuePreview(root.selectedComputerId, "selected")
  }

  Component {
    id: daemonSocketComponent
    Socket {
      id: socket
      path: root.daemonSocketPath
      parser: SplitParser { onRead: data => root.consumeDaemonLine(data) }
      onConnectionStateChanged: root.daemonConnectionChanged(socket)
      onError: root.daemonConnectFailed(socket)
    }
  }
  // Reconnects with backoff (0.5 s doubling to 30 s); any request also reconnects at once.
  Timer { id: daemonReconnect; repeat: false; onTriggered: root.connectDaemon() }

  Process { id: openFolderProcess }
  // Whether the ibara package is installed and set up (see ibaraMissing); installed and set up
  // while nothing answers on its socket means its service stopped (serviceStopped).
  Process {
    id: installCheck
    command: ["sh", "-c", "if [ ! -x /usr/lib/ibara/bin/ibara ]; then echo package; elif [ ! -f /etc/ibara/station.json ]; then echo setup; else echo ready; fi"]
    stdout: SplitParser {
      onRead: data => {
        var state = String(data || "").trim()
        root.ibaraMissing = state === "package" || state === "setup" ? state : ""
        if (root.ibaraMissing) root.lastError = ""
        root.serviceStopped = state === "ready" && root.daemonDown
      }
    }
  }
  Process { id: installLauncher }
  // Whether this desktop's service manager knows ibara's service, so Start ibara can start it.
  Process {
    id: ibaraUnitCheck
    command: ["systemctl", "--user", "cat", "ibara-operator.service"]
    onExited: code => root.canStartIbara = code === 0
  }
  // Start ibara: the console connects by itself once the service answers (daemonConnectionChanged).
  Process {
    id: ibaraStarter
    command: ["systemctl", "--user", "start", "ibara-operator.service"]
    onExited: code => {
      if (code !== 0) { root.startState = "failed"; return }
      root.daemonRetryMs = 500
      root.connectDaemon()
    }
  }
  // Started, but still nothing answers after 15 s: the toast names the command instead.
  Timer {
    interval: 15000
    running: root.startState === "starting"
    onTriggered: if (root.startState === "starting") root.startState = "failed"
  }
  // The terminal opens and this returns at once; the switch follows the answers polled below.
  Process {
    id: unattendedLauncher
    onExited: (code, status) => {
      if (code !== 0) root.settleUnattendedBoot("No terminal opened. Open one and run: " + unattendedLauncher.command[1])
    }
  }
  Timer {
    interval: 3000; repeat: true; running: root.unattendedBootWanted !== null
    onTriggered: {
      if (Date.now() - root.unattendedBootAskedAt > 600000) root.settleUnattendedBoot("")
      else root.loadUnattendedBoot()
    }
  }

  Process { id: notifyProcess; onRunningChanged: if (!running) Qt.callLater(root.drainNotices) }
  // One waiting notify-send per approval notification: it prints the notification's id, then the
  // action chosen, and ends when the notification closes.
  Component {
    id: approvalNoticeComponent
    Process {
      id: notice
      property string ref: ""
      property string computerId: ""
      // A login request's notification answers Share or Don't Share.
      property bool login: false
      property int loginRevision: -1
      property var loginSites: []
      property bool question: false
      property var options: []
      property string label: ""
      property string noticeId: ""
      property bool answered: false
      property bool started: false
      stdout: SplitParser { onRead: data => root.approvalNoticeLine(notice, data) }
      onRunningChanged: { if (running) started = true; else if (started) root.approvalNoticeClosed(notice) }
    }
  }
  // A short command nobody waits for (closing a notification).
  Component {
    id: oneShotComponent
    Process { id: oneShot; onRunningChanged: if (!running) oneShot.destroy() }
  }

  // `tailscale up` for Sign In to Tailscale when Tailscale has no sign-in page waiting yet: the
  // page it prints opens in the browser, and the tailnet is read again once it finishes.
  Process {
    id: tailscaleUp
    property bool opened: false
    property string lastLine: ""
    command: ["tailscale", "up"]
    function seen(line) {
      var text = String(line || "").trim()
      if (text) lastLine = text
      var page = /https:\/\/\S+/.exec(text)
      if (page && !opened) { opened = true; Qt.openUrlExternally(page[0]) }
    }
    stdout: SplitParser { onRead: data => tailscaleUp.seen(data) }
    stderr: SplitParser { onRead: data => tailscaleUp.seen(data) }
    onExited: function(exitCode) {
      if (exitCode !== 0 && !opened) root.actionError = "Tailscale didn't start: " + StatusModel.clip(tailscaleUp.lastLine || "it stopped without saying why.", 200)
      root.loadTailnet()
    }
  }

  IpcHandler {
    target: "io.zet.ibara"
    function refresh(): void { root.refresh() }
    function previewStats(): string { return root.previewStats() }
    function status(): string {
      return JSON.stringify({
        connection: root.connectionState,
        error: root.lastError,
        computers: root.computers.length,
        needs_you: root.needsYouCount,
        questions: root.questions.length,
        problems: root.problems.map(function(p) { return p.heading })
      })
    }
  }
}
