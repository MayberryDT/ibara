.pragma library

function clip(value, limit) {
  var text = String(value || "").replace(/\s+/g, " ").trim()
  if (text.length <= limit) return text
  return text.substring(0, Math.max(0, limit - 1)) + "…"
}

// A button label in Title Case, for words the console didn't write (an agent's answer options):
// "Approve file manager" reads "Approve File Manager". Short joining words stay lowercase except
// first and last; a word that already has a capital ("iPhone", "FICTIONAL") and "ibara" stay as
// they are. Only the label changes: the answer sent is always the agent's own text.
var SMALL_WORDS = ["a", "an", "the", "and", "but", "or", "nor", "for", "so", "yet", "as", "at", "by", "in", "of", "off", "on", "per", "to", "up", "via", "vs", "with", "from", "into"]
function titleCase(value) {
  var words = String(value || "").split(" ")
  return words.map(function(word, i) {
    var letters = word.replace(/^[^A-Za-z]+|[^A-Za-z]+$/g, "")
    if (!letters || /[A-Z]/.test(word) || letters === "ibara" || !/^[^A-Za-z0-9]*[a-z]/.test(word)) return word
    if (i > 0 && i < words.length - 1 && SMALL_WORDS.indexOf(letters) !== -1) return word
    return word.replace(/[a-z]/, function(c) { return c.toUpperCase() }).replace(/-([a-z])/g, function(m, c) { return "-" + c.toUpperCase() })
  }).join(" ")
}

// Core texts that tell the person to "inspect" something name no place in the console. Each
// becomes plain English that names a place or says ibara is handling it. %NAME% is the computer.
var CORE_PHRASES = [
  [/The operator reply was lost\./i, "ibara lost the reply, so it can't tell whether this finished."],
  [/Operator request failed; inspect target status/i, "%NAME% closed the connection; your access may have changed. ibara will check again."],
  [/Control response changed target binding/i, "%NAME% changed its pairing during this request. ibara is reconnecting; choose Join again once it answers."],
  [/Handback did not prove settled ownership/i, "%NAME% didn't confirm that the hand back finished. ibara is checking."],
  [/Retained publication is uncertain/i, "ibara can't tell whether the file arrived. Look for it in the Files tab before sending it again."],
  [/Upload \S+ is partial or uncertain/i, "The upload stopped part-way. Use Retry under Transfers in the Files tab to finish it."],
  [/Procedure contents changed since review/i, "This procedure changed since you opened it. Open it again in the Activity tab before approving."],
  [/Selected operator reply is malformed or oversized/i, "%NAME% sent an answer ibara couldn't read. Choose the action again to retry."],
  [/(^|: )Target binding changed\.$/, "%NAME% changed its pairing. ibara is reconnecting."]
]

// A message the console can show. Transport failures (ssh, tailscale, timeouts, closed
// or refused connections) never show their raw stderr: they name the computer that is not
// answering. `retrying` says ibara will try again on its own (refreshes, not actions).
function plainError(message, computerName, retrying) {
  var text = clip(message, 400)
  if (!text) return ""
  var unreachable = /banner exchange|connection timed out|operation timed out|etimedout|i\/o timeout|no route to host|network is unreachable|host is down|connection refused|connection reset|connection closed|could not resolve hostname|name or service not known|port 65535|transport (timed out|closed|failed)/i
  if (unreachable.test(text)) {
    var name = String(computerName || "").trim()
    return (name || "The computer") + " isn't answering." + (retrying ? " ibara will keep trying." : "")
  }
  var missing = /^spawn(Sync)? (\S+) ENOENT$/.exec(text)
  if (missing) return "ibara needs " + missing[2] + ", which isn't installed on this computer."
  for (var i = 0; i < CORE_PHRASES.length; i++) if (CORE_PHRASES[i][0].test(text)) return CORE_PHRASES[i][1].replace("%NAME%", String(computerName || "").trim() || "This computer")
  // Core messages often start with their code ("CONTROL_UNSETTLED: …"); the sentence is what a person reads.
  return text.replace(/^[A-Z][A-Z0-9_]{3,}: +(?=\S)/, "")
}

// A preview frame file ibarad wrote for this computer and quality:
// <absolute runtime dir>/ibara/previews/<computer>-<quality>-<n>.ppm, no parent steps.
function previewFile(path, computerId, quality) {
  var text = String(path || "")
  if (text.charAt(0) !== "/" || text.indexOf("/../") !== -1 || text.indexOf("\0") !== -1) return false
  var name = "/ibara/previews/" + String(computerId) + "-" + String(quality) + "-"
  var at = text.lastIndexOf(name)
  return at > 0 && /^[0-9]+\.ppm$/.test(text.substring(at + name.length))
}

// A live video pipe ibarad opened for this computer (Live Video):
// <absolute runtime dir>/ibara/previews/video-<computer>-<width>x<height>-<n>.ts, no parent steps.
function videoFile(path, computerId) {
  var text = String(path || "")
  if (text.charAt(0) !== "/" || text.indexOf("/../") !== -1 || text.indexOf("\0") !== -1) return false
  var name = "/ibara/previews/video-" + String(computerId) + "-"
  var at = text.lastIndexOf(name)
  return at > 0 && /^[0-9]+x[0-9]+(-[0-9]+)?\.ts$/.test(text.substring(at + name.length))
}

// Whether a computer can stream Live Video, from its status: { capable, reason }. A computer
// whose ibara doesn't say can't.
function videoView(value) {
  var video = value && typeof value === "object" && !Array.isArray(value) ? value : null
  if (!video) return { capable: false, reason: "" }
  return { capable: video.capable === true, reason: video.capable === true ? "" : clip(String(video.reason || ""), 200) }
}

function asObject(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : ({})
}

// A complete ISO instant to epoch ms, else NaN. Qt's Date.parse goes through QDateTime and
// costs milliseconds per call on a small CPU; labels re-rendered every second use this.
var ISO_INSTANT = /^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d{1,9}))?(Z|([+-])(\d\d):(\d\d))$/
function isoMs(value) {
  var m = typeof value === "string" ? ISO_INSTANT.exec(value) : null
  if (!m) return NaN
  var year = +m[1], month = +m[2], day = +m[3], hour = +m[4], minute = +m[5], second = +m[6]
  if (month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 || second > 59) return NaN
  var ms = Date.UTC(year, month - 1, day, hour, minute, second, m[7] ? Math.floor(+(m[7] + "00").slice(0, 3)) : 0)
  if (new Date(ms).getUTCDate() !== day) return NaN
  if (m[8] !== "Z") {
    var offsetHours = +m[10], offsetMinutes = +m[11]
    if (offsetHours > 23 || offsetMinutes > 59) return NaN
    ms -= (m[9] === "-" ? -1 : 1) * (offsetHours * 60 + offsetMinutes) * 60000
  }
  return ms
}

function ageLabel(observedAt, nowMs) {
  if (!observedAt) return "No observation"
  var then = isoMs(String(observedAt))
  if (!isFinite(then)) return "Unknown age"
  var seconds = Math.max(0, Math.round((nowMs - then) / 1000))
  if (seconds < 5) return "just now"
  if (seconds < 60) return seconds + "s ago"
  var minutes = Math.round(seconds / 60)
  if (minutes < 60) return minutes + "m ago"
  return Math.round(minutes / 60) + "h ago"
}

function taskTitle(task) {
  task = asObject(task)
  return clip(task.goal || task.task_ref || "Untitled task", 80)
}

function sentence(value) {
  var text = String(value || "").replace(/[_-]/g, " ").trim()
  return text ? text.charAt(0).toUpperCase() + text.slice(1) : ""
}

// Task detail wording, shared by the list, outcome band and oldest-first timeline.
function taskLive(task) {
  task = asObject(task)
  return !!task.live || ["active", "running", "waiting_for_human"].indexOf(task.state) !== -1
}
function taskState(task) {
  task = asObject(task)
  if (taskLive(task)) return "In progress"
  var state = task.state || task.completion_outcome || "unknown"
  if (state === "completed" || state === "complete") return "Finished"
  if (state === "partial") return "Partly done"
  if (state === "revoked" || state === "cancelled" || state === "canceled") return "Ended"
  return sentence(state)
}
function taskTone(task) {
  var state = taskState(task)
  return state === "In progress" ? "working" : state === "Finished" ? "ready" : state === "Partly done" ? "paused" : "attention"
}
function taskProjection(item) {
  item = asObject(item)
  var task = asObject(item.task || item)
  return asObject(item.completion_projection || item.projection || task.completion_projection || task.verification)
}
function taskCriteria(item) {
  var projection = taskProjection(item)
  return Array.isArray(item && item.criteria) ? item.criteria : Array.isArray(projection.criteria) ? projection.criteria : []
}
function taskDeliveries(item) {
  var projection = taskProjection(item)
  return Array.isArray(item && item.deliveries) ? item.deliveries : Array.isArray(projection.deliveries) ? projection.deliveries : []
}
function taskChecks(item) {
  var checks = taskCriteria(item)
  return { total: checks.length, met: checks.filter(function(c) { return c.state === "satisfied" }).length }
}
function taskProof(item) {
  var task = asObject(asObject(item).task || item), proof = taskProjection(item)
  if (proof.verified_complete === true) return "ibara checked the result"
  if (!Object.keys(proof).length) return taskLive(task) ? "ibara hasn't checked the result yet" : "ibara didn't check the result"
  return "ibara couldn't confirm the result"
}
function taskCleanup(item) {
  var proof = taskProjection(item)
  if (proof.cleanup) return sentence(proof.cleanup)
  if (proof.cleanup_settled === true) return "Settled"
  if (proof.cleanup_settled === false) return "Unsettled"
  return "Not confirmed"
}
function taskAgent(task) { return task && task.principal ? "agent from " + String(task.principal) : "" }
function taskTime(iso) {
  var date = new Date(iso)
  return isFinite(date.getTime()) ? [date.getHours(), date.getMinutes(), date.getSeconds()].map(function(n) { return n < 10 ? "0" + n : String(n) }).join(":") : ""
}
function taskTimes(task) {
  task = asObject(task)
  var start = taskTime(task.created_at), end = taskTime(task.finished_at || task.updated_at)
  return start ? start + (taskLive(task) ? " to now" : end ? " to " + end : "") : ""
}
function taskBandDetail(item, task) {
  return [taskProof(item), "cleanup " + taskCleanup(item).toLowerCase(), taskAgent(task), taskTimes(task)].filter(Boolean).join(" · ")
}
function taskClaim(criterion) {
  // A Repeater wraps nested lists in Qt array-like model data.
  var claims = criterion && criterion.claims ? Array.prototype.slice.call(criterion.claims) : []
  return claims.map(function(c) {
    var detail = c.detail || asObject(c.assessment).reason
    return detail ? (c.basis === "automatic" ? "ibara checked: " : "The agent says: ") + String(detail) : ""
  }).filter(Boolean).join("\n")
}
function taskDeliveryTone(delivery) {
  var state = String(asObject(delivery).state || "pending")
  return ["verified", "satisfied", "completed", "delivered"].indexOf(state) !== -1 ? "ready"
    : ["failed", "unknown", "unavailable"].indexOf(state) !== -1 ? "attention" : "paused"
}
// Durable references belong in Technical Details, never in a step's visible words.
function taskPlain(text) { return String(text || "").replace(/\bjob_[A-Za-z0-9_-]+\b/g, "a command").replace(/\b(?:op|operation)_[A-Za-z0-9_-]+\b/g, "a step") }
function taskStep(receipt) {
  var r = asObject(receipt), summary = String(r.summary || "")
  var step = { kind: "act", verb: "Did", what: summary, expect: "", tone: "ready", result: "Done", error: taskPlain(r.error), at: r.created_at,
    key: String(r.operation_ref || r.request_id || (r.created_at || "") + ":" + (r.tool || "") + ":" + summary) }
  var job = /^Job \S+ is ([\w-]+)\.$/.exec(summary)
  var jobState = r.job_state !== undefined && r.job_state !== null ? String(r.job_state) : job ? job[1] : ""
  if (r.tool === "computer_begin") { step.kind = "task"; step.verb = "Start"; step.what = "Took control of the computer" }
  else if (r.tool === "computer_finish") { step.kind = "task"; step.verb = "Finish"; step.what = "Released control" }
  else if (r.tool === "computer_exec") {
    step.kind = "exec"; step.verb = "Run"; step.what = job ? "A command (not recorded)" : summary; step.result = "Ran"
    if (/^run \[/.test(summary)) {
      // The operator's 400-character clip can cut inside an argv JSON string.
      step.what = summary.slice(4) + "…"
      var clippedShell = /^\[\s*"(?:[^"\\]*\/)?(?:bash|sh)"\s*,\s*"-l?c"\s*,\s*"((?:[^"\\]|\\[\s\S])*)/.exec(summary.slice(4))
      if (clippedShell) step.what = clippedShell[1].replace(/\\(?:u[0-9a-fA-F]{4}|["\\/bfnrt])/g, function(escape) {
        return JSON.parse('"' + escape + '"')
      }) + "…"
      try {
        var argv = JSON.parse(summary.slice(4))
        if (Array.isArray(argv) && argv.length && argv.every(function(a) { return typeof a === "string" })) {
          var program = argv[0].split("/").pop()
          var shell = (program === "bash" || program === "sh") && (argv[1] === "-c" || argv[1] === "-lc")
          step.what = shell && argv.length > 2 ? argv[2] : argv.join(" ")
        }
      } catch (e) {} // Keep the readable clipped prefix when JSON is incomplete.
    }
    if (jobState === "completed") step.result = "Finished"
    else if (["running", "pending", "queued", "started"].indexOf(jobState) !== -1) { step.result = "In progress"; step.tone = "working" }
  } else {
    var parts = summary.split(" · ")
    step.what = parts[0]; step.expect = parts.slice(1).join(" · ")
    var action = /^(click|key|type|scroll|drag|move|launch)\b\s*(.*)$/.exec(step.what)
    if (action) { step.verb = sentence(action[1]); step.what = action[2] }
    step.what = step.what.replace(/^e\d+ /, "")
    if (r.verification === "satisfied") step.result = "Saw it"
  }
  if (r.verification === "unsatisfied") { step.tone = "paused"; step.result = "Didn't see it" }
  var settledJob = ["completed", "failed", "cancelled", "canceled", "interrupted", "timed_out"].indexOf(jobState) !== -1
  if (!r.error && /^Held for approval/.test(summary)) {
    step.what = "An action awaiting approval"; step.expect = ""; step.tone = "paused"; step.result = "Waiting for approval"
  } else if (!r.error && /^Not run: /.test(summary)) {
    step.what = summary.slice(9); step.expect = ""; step.tone = "paused"; step.result = "Not run"
  } else if (r.error || ["failed", "not_started", "cancelled", "canceled", "interrupted"].indexOf(r.execution) !== -1 || ["failed", "cancelled", "canceled", "interrupted", "timed_out"].indexOf(jobState) !== -1) {
    step.tone = "attention"; step.result = "Failed"
  } else if (jobState !== "unknown" && (["running", "pending", "queued", "started"].indexOf(jobState) !== -1 || (r.execution === "running" && !settledJob))) {
    step.tone = "working"; step.result = "In progress"
  } else if ((r.dependency_state === "unresolved" && !(r.execution === "running" && settledJob)) || r.requires_reconciliation || r.execution === "unknown" || r.effect === "unknown" || jobState === "unknown") {
    step.tone = "attention"; step.result = "Unknown, don't repeat"
  }
  step.what = taskPlain(step.what); step.expect = taskPlain(step.expect)
  return step
}
function taskWarning(item) {
  var proof = taskProjection(item)
  if (Number(proof.unresolved_request_count) > 0 || (Array.isArray(proof.unresolved_request_refs) && proof.unresolved_request_refs.length)) return "Some steps may not have finished. See What it did below before running them again."
  var receipts = listOf(item, "receipts")
  for (var i = 0; i < receipts.length; i++) if (taskStep(receipts[i]).result === "Unknown, don't repeat") return "Some steps may not have finished. See What it did below before running them again."
  return ""
}
function taskRunSummary(group) {
  var items = group.items, unseen = 0, mix = {}, app = "", names = { Key: ["key press", "key presses"], Click: ["click", "clicks"], Type: ["typing", "typing"], Scroll: ["scroll", "scrolls"] }
  items.forEach(function(s) {
    if (s.tone === "paused") unseen++
    mix[s.verb] = (mix[s.verb] || 0) + 1
    var match = / (?:in|into) ([\w.-]+)$/.exec(s.what)
    if (!app && match) app = match[1].split(".").pop()
  })
  var text = group.kind === "exec" ? "Ran " + items.length + " commands" : "Did " + items.length + " steps in " + (app || "the app")
  if (group.kind !== "exec") text += " (" + Object.keys(mix).map(function(verb) { var n = mix[verb], noun = names[verb] || ["other step", "other steps"]; return n + " " + noun[n > 1 ? 1 : 0] }).join(", ") + ")"
  return text + (unseen ? ", " + unseen + " not seen" : "")
}
function taskRuns(item, live) {
  var steps = listOf(item, "receipts").slice().reverse().map(taskStep), groups = []
  steps.forEach(function(s, i) {
    var fold = s.kind !== "task" && s.tone !== "attention" && !(live && i === steps.length - 1)
    var last = groups[groups.length - 1]
    if (last && last.eligible && fold && last.kind === s.kind) last.items.push(s)
    else groups.push({ key: s.key, kind: s.kind, eligible: fold, items: [s] })
  })
  groups.forEach(function(g) { g.fold = g.eligible && g.items.length >= 3; g.summary = taskRunSummary(g) })
  return groups
}

// A file size as a person reads it, in decimal units counted from the bytes (1 MB is
// 1,000,000 bytes): "900 bytes", "1.6 MB", "250 MB". ibara's size_text (operator_files.rs)
// words the refusals the same way.
function bytesLabel(value) {
  var n = Number(value)
  if (!isFinite(n) || n < 0) return "size unknown"
  n = Math.floor(n)
  if (n < 1000) return n === 1 ? "1 byte" : n + " bytes"
  var units = ["KB", "MB", "GB", "TB"], unit = 0
  n /= 1000
  // 999.6 KB reads "1 MB", not "1000 KB".
  while (Math.round(n) >= 1000 && unit + 1 < units.length) {
    n /= 1000
    unit++
  }
  // One decimal below 10, else whole numbers: "1.6 MB", "10 KB", "250 MB".
  var tenths = Math.round(n * 10)
  return (tenths < 100 ? tenths / 10 : Math.round(n)) + " " + units[unit]
}

// The health report a computer gives over the pairing route (operator-health), as short lines.
// How busy it is comes from its one-minute load for each processor it has.
function healthLines(result) {
  result = asObject(result)
  var lines = []
  var load = Array.isArray(result.load) ? result.load[0] : result.load
  var cpus = Number(result.cpus)
  if (typeof load === "number" && isFinite(load) && load >= 0) {
    var share = load / (cpus >= 1 ? cpus : 1)
    // A computer that doesn't say how many processors it has is busy only when the load is low
    // enough to say for one processor; otherwise the line is left out.
    if (cpus >= 1 || share < 0.5) lines.push("Busy: " + (share < 0.5 ? "light" : share < 1 ? "moderate" : "heavy"))
  }
  var memory = asObject(result.memory), disk = asObject(result.disk)
  if (isFinite(Number(memory.used_mb)) && Number(memory.total_mb) > 0)
    lines.push("Memory " + (Number(memory.used_mb) / 1024).toFixed(1) + " GB used of " + (Number(memory.total_mb) / 1024).toFixed(1) + " GB")
  if (isFinite(Number(disk.used_gb)) && Number(disk.total_gb) > 0)
    lines.push("Disk " + Math.round(Number(disk.used_gb)) + " GB used of " + Math.round(Number(disk.total_gb)) + " GB")
  if (typeof result.uptime_s === "number" && isFinite(result.uptime_s) && result.uptime_s >= 0)
    lines.push("On for " + spelledDuration(result.uptime_s * 1000))
  return lines
}

// Previews use their own 2 MiB frame envelope; every other response keeps 512 KiB. Replies
// from ibarad arrive already parsed (the service bounds their line length); text is parsed here.
function parseEnvelope(text, limit) {
  if (text && typeof text === "object" && !Array.isArray(text)) return text
  var raw = String(text || "").trim()
  if (raw.length > (limit || 512 * 1024)) throw new Error("Response too large")
  var parsed = JSON.parse(raw)
  if (!parsed || typeof parsed !== "object") throw new Error("Invalid envelope")
  return parsed
}

function accessDenied(envelope) {
  var code = String(asObject(envelope.error).code || "")
  return envelope.connection === "unauthorized" || ["UNAUTHORIZED", "PERMISSION_DENIED", "AUTH_REQUIRED", "GRANT_REVOKED"].indexOf(code) !== -1
}

function evidenceLines(detail) {
  detail = asObject(detail)
  var task = asObject(detail.task || detail)
  var projection = asObject(detail.completion_projection || task.completion_projection || task.verification)
  var lines = ["Owner: " + String(task.principal || "unknown"), "Visibility: " + String(task.visibility || task.sharing_scope || "private (default)")]
  var legacy = task.contract_version !== "3.0" && !Object.keys(projection).length
  if (legacy) lines.push("Legacy / unverified: historical completion is not current evidence.")
  if (task.completion_outcome) lines.push("Desktop outcome: " + task.completion_outcome)
  if (projection.verified_complete !== undefined) lines.push("Current verified completion: " + (projection.verified_complete === true ? "satisfied" : "incomplete"))
  if (projection.criteria && !Array.isArray(projection.criteria)) lines.push("Required criteria: " + Number(projection.criteria.satisfied || 0) + " / " + Number(projection.criteria.required || 0) + " satisfied")
  if (projection.delivery && !Array.isArray(projection.delivery)) lines.push("Required deliveries: " + Number(projection.delivery.verified || 0) + " / " + Number(projection.delivery.required || 0) + " verified")
  if (Array.isArray(projection.unresolved_request_refs) && projection.unresolved_request_refs.length) lines.push("Unresolved requests: " + projection.unresolved_request_refs.join(", ") + ". Check What it did before running anything that depends on them.")
  var receipts = listOf(detail, "receipts")
  receipts.forEach(function(receipt) {
    lines.push("Request " + String(receipt.request_id || receipt.operation_ref || "unknown") + ": execution " + String(receipt.execution || "unknown") + "; effect " + String(receipt.effect || "unknown") + "; verification " + String(receipt.verification || "unknown"))
    if (receipt.requires_reconciliation || receipt.execution === "unknown" || receipt.effect === "unknown") lines.push("This request may not have finished. Don't run it again until What it did shows its result.")
  })
  if (!receipts.length) lines.push("Execution: no receipt loaded.")
  var criteria = Array.isArray(detail.criteria) ? detail.criteria : (Array.isArray(projection.criteria) ? projection.criteria : [])
  criteria.forEach(function(criterion) {
    criterion = asObject(criterion)
    var policy = asObject(criterion.policy)
    lines.push("Criterion " + String(criterion.id || criterion.criterion_id || "unknown") + ": " + String(criterion.state || "unverified") + " · " + (criterion.required === false ? "optional" : "required"))
    if (criterion.description) lines.push(String(criterion.description))
    lines.push("Proof policy: " + String(policy.kind || "unavailable") + (policy.assessor ? " · required assessor: " + String(policy.assessor) : ""))
    if (policy.predicate) lines.push("Required check: " + JSON.stringify(policy.predicate))
    if (policy.max_age_seconds !== undefined) lines.push("Freshness limit: " + String(policy.max_age_seconds) + " seconds")
    var claims = Array.isArray(criterion.claims) ? criterion.claims : []
    claims.forEach(function(claim) {
      lines.push("Stored claim: " + String(claim.claim || "unknown") + " · evidence: " + (Array.isArray(claim.evidence_refs) && claim.evidence_refs.length ? claim.evidence_refs.join(", ") : "none"))
      if (claim.assessment) {
        var assessment = asObject(claim.assessment)
        lines.push("Assessment by " + String(assessment.assessor || "unknown assessor") + ": " + String(assessment.reason || "reason unavailable"))
      }
    })
    if (!claims.length) lines.push("No stored criterion claim.")
    var evidence = Array.isArray(criterion.evidence) ? criterion.evidence : []
    evidence.forEach(function(item) {
      var assessmentSource = item.source === "agent_assessment" || item.source === "human_assessment"
      var name = assessmentSource ? "Assessment evidence " : item.kind === "check" ? "Check " : "Evidence "
      var fact = item.kind === "check" && !assessmentSource ? "outcome " + String(item.outcome || "unknown") : String(assessmentSource ? item.source : item.kind || "missing")
      lines.push(name + String(item.ref || "unknown") + ": " + fact + " · " + (item.expired === true ? "expired" : item.expired === false ? "retained" : "retention unknown"))
      if (item.source) lines.push("Source: " + String(item.source))
      if (item.checked_at) lines.push("Checked at: " + String(item.checked_at))
      if (item.summary) lines.push(String(item.summary))
    })
    if (!evidence.length) lines.push("No supporting evidence detail loaded.")
  })
  if (!criteria.length) lines.push("Checks and assessments: no current criterion proof loaded.")
  var deliveries = Array.isArray(detail.deliveries) ? detail.deliveries : (Array.isArray(projection.deliveries) ? projection.deliveries : [])
  deliveries.forEach(function(delivery) { lines.push("Required delivery: " + String(delivery.host_id || delivery.destination_host || delivery.host || "unknown host") + " / " + String(delivery.destination_path || delivery.path || "unknown path") + " · " + String(delivery.state || "pending") + (delivery.revision ? " · revision " + delivery.revision : "")) })
  if (!deliveries.length) lines.push("Required delivery: not established by this view; a collected copy alone is not proof.")
  if (projection.cleanup) lines.push("Cleanup: " + String(projection.cleanup))
  else if (projection.cleanup_settled !== undefined) lines.push("Cleanup: " + (projection.cleanup_settled ? "settled" : "unsettled"))
  else lines.push("Task cleanup: no current proof loaded.")
  if (detail.receipt_next_cursor) lines.push("More receipts available; this is a partial history.")
  return lines
}

function procedureLines(record) {
  record = asObject(record)
  var definition = asObject(record.definition)
  var lines = [String(definition.title || record.procedure_ref || "Procedure"), "Status: " + String(record.status || "candidate"), "Revision: " + String(record.content_sha256 || record.source_sha256 || "unknown")]
  lines.push("Applicability: " + String(record.applicability_state || "not established by this view"))
  if (Array.isArray(record.applicability_reasons)) lines = lines.concat(record.applicability_reasons.map(String))
  if (record.expiry_state) lines.push("Evidence expiry: " + String(record.expiry_state))
  if (record.contradiction_state) lines.push("Contradictions: " + String(record.contradiction_state))
  if (record.fresh_verification_required === true) lines.push("Fresh verification: required for every use.")
  if (record.evidence_redacted === true) lines.push("Supporting evidence: redacted; the shared guidance does not grant evidence access.")
  lines.push("Guidance is not authority. Verify prerequisites and each use with fresh evidence.")
  return lines.concat(Array.isArray(definition.steps) ? definition.steps.map(String) : [])
}

// ---- first run: the tailnet, adding computers, requests to use this one and agents.

// A tailnet computer's short name, as Tailscale gives it, and how the console writes it.
function tailnetNode(value) {
  return /^[A-Za-z0-9][A-Za-z0-9_-]{0,62}$/.test(String(value || ""))
}
function computerName(node) {
  var text = String(node || "").trim()
  return text ? text.charAt(0).toUpperCase() + text.slice(1) : ""
}
// A pairing code is six digits, written "482 913" on both screens.
function pairCode(value) {
  var digits = String(value || "").replace(/\s+/g, "")
  return /^[0-9]{6}$/.test(digits) ? digits.slice(0, 3) + " " + digits.slice(3) : ""
}
function requestId(value) {
  return /^[A-Za-z0-9_.:-]{1,128}$/.test(String(value || ""))
}

// Each view keeps only well-formed fields, so a malformed reply shows as nothing, never as a
// broken row or a link to somewhere unexpected.
var TAILSCALE_STATES = ["running", "not_installed", "stopped", "logged_out"]
var IBARA_STATES = ["ready", "not_installed", "offline", "unknown"]
function tailnetView(data) {
  data = asObject(data)
  var tailscale = asObject(data.tailscale)
  var loginUrl = String(tailscale.login_url || "")
  var computers = []
  var list = Array.isArray(data.computers) ? data.computers : []
  for (var i = 0; i < list.length; i++) {
    var c = asObject(list[i])
    if (!tailnetNode(c.node)) continue
    computers.push({
      node: String(c.node), online: c.online === true, owner: clip(c.owner, 128), same_owner: c.same_owner === true,
      is_self: c.is_self === true, ibara: IBARA_STATES.indexOf(c.ibara) !== -1 ? c.ibara : "unknown",
      paired: c.paired === true, computer_id: typeof c.computer_id === "string" ? c.computer_id : "",
      label: clip(c.label, 128)
    })
  }
  return {
    tailscale: {
      state: TAILSCALE_STATES.indexOf(tailscale.state) !== -1 ? tailscale.state : "running",
      login: clip(tailscale.login, 128), self_node: tailnetNode(tailscale.self_node) ? String(tailscale.self_node) : "",
      login_url: /^https:\/\/[^\s"']+$/.test(loginUrl) ? loginUrl : ""
    },
    computers: computers
  }
}

var PAIR_STATES = ["waiting", "paired", "declined", "expired", "failed", "canceled"]
function pairingView(data) {
  data = asObject(data)
  if (!requestId(data.request_id) || PAIR_STATES.indexOf(data.state) === -1) return null
  return {
    request_id: String(data.request_id), code: pairCode(data.code), state: data.state,
    mode: ["own_computer", "needs_approval", "invite"].indexOf(data.mode) !== -1 ? data.mode : "",
    computer_id: typeof data.computer_id === "string" ? data.computer_id : "",
    label: clip(data.label, 128), message: clip(data.message, 300)
  }
}

// Sharing this computer with a friend (`invites`, `invite-create`): each invite's level, when it
// ends (0: until revoked), who used it, and the code (only right after it is made).
var SHARE_LEVELS = ["watch", "use_with_approval", "take_control"]
function shareLevelLabel(level) {
  return level === "watch" ? "Watch" : level === "use_with_approval" ? "Use with Approval" : level === "take_control" ? "Join" : ""
}
function inviteView(value) {
  var i = asObject(value)
  if (!requestId(i.id) || SHARE_LEVELS.indexOf(i.level) === -1) return null
  var ms = function(v) { return typeof v === "number" && isFinite(v) ? v : 0 }
  var used = i.used && typeof i.used === "object" ? { at: ms(i.used.at), login: clip(i.used.login, 128), computer: clip(i.used.computer, 64) } : null
  return { id: String(i.id), level: i.level, created_at: ms(i.created_at), expires_at: ms(i.expires_at), used: used,
    code: /^[2-9A-Z]{4}-[2-9A-Z]{4}$/.test(String(i.code || "")) ? String(i.code) : "" }
}
function invitesView(data) {
  data = asObject(data)
  var list = listOf(data, "invites"), out = []
  for (var i = 0; i < list.length && out.length < 50; i++) { var view = inviteView(list[i]); if (view) out.push(view) }
  var t = asObject(data.tailscale), url = String(t.share_url || "")
  var tailscale = /^https:\/\/login\.tailscale\.com\/admin\/machines\/[0-9.]{7,15}$/.test(url) ? { address: clip(t.address, 64), share_url: url } : null
  return { invites: out, tailscale: tailscale }
}

function pairRequestsView(data) {
  var list = listOf(data, "requests"), out = []
  for (var i = 0; i < list.length; i++) {
    var r = asObject(list[i])
    var code = pairCode(r.code)
    if (!requestId(r.request_id) || !code || !tailnetNode(r.from_computer)) continue
    out.push({ request_id: String(r.request_id), from_owner: clip(r.from_owner, 128), from_computer: String(r.from_computer),
      code: code, expires_at: typeof r.expires_at === "number" && isFinite(r.expires_at) ? r.expires_at : 0 })
  }
  return out
}

// The prompt that connects an agent, and whether an agent has begun a task through this computer.
// Its lines stay as ibara wrote them: the agent copies its block into its instructions file.
function connectPromptView(data) {
  data = asObject(data)
  return { prompt: String(data.prompt || "").replace(/\r/g, "").trim().slice(0, 4000), first_task_done: data.first_task_done === true }
}

function listOf(value, key) {
  if (Array.isArray(value)) return value
  var obj = asObject(value)
  if (Array.isArray(obj[key])) return obj[key]
  return []
}

// Fleet state for one computer. Precedence: attention > locked > offline > human > working > paused > ready.
// Owner names come from the controller: operator:<id>, agent:<principal>:<task_ref>, human (agents
// paused with nobody holding the viewer) or none. A computer whose self-repair needs a person
// (repair.needs_person) needs attention, and so does one where an agent waits for your approval
// or your answer (`waiting`, from fleet-attention), unless it is offline: an answer can't reach it.
// A computer that answers with its screen locked (`locked`) is "locked": agents can't use it and
// Join is how a person unlocks it.
var ATTENTION_TASK_STATES = ["waiting_for_human", "interrupted", "blocked"]
// Preview notes that are routine while a frame is on its way; any other note without a frame needs a look.
var ROUTINE_FRAME_NOTES = /^(Preview pending|Preview unavailable|Waiting for an authorized preview\.|Not previewed: |Preview released |Controller restarted|This computer has no display yet\.|ibara dropped a picture |BUDGET_EXCEEDED\b)/

function fleetState(session) {
  var state = computerState(session)
  return state !== "offline" && state !== "attention" && waitingCount(session) > 0 ? "attention" : state
}
// How many approvals and questions wait for you on this computer.
function waitingCount(session) {
  var waiting = asObject(asObject(session).waiting)
  return (Number(waiting.approvals) || 0) + (Number(waiting.questions) || 0)
}
// The computer's own state, without what waits for your answer there: the red dot and its
// problem toasts are about the computer, never about an approval that has its own toast.
function computerState(session) {
  session = asObject(session)
  var connection = String(session.connection || "")
  var task = asObject(session.active_task)
  var note = String(session.frame_error || "")
  if (connection === "unauthorized" || connection === "failed" || connection === "unverified" ||
      (session.trust_state !== undefined && session.trust_state !== "verified") || session.observation === "denied") return "attention"
  // Once a person holds control, the lock is theirs to type through: "You have control", not a problem.
  if (session.locked === true && connection !== "offline" && String(session.owner_name || "").indexOf("operator:") !== 0) return "locked"
  if (ATTENTION_TASK_STATES.indexOf(String(task.state || "")) !== -1) return "attention"
  if (connection === "ready" && !session.frame && note && !ROUTINE_FRAME_NOTES.test(note)) return "attention"
  if (connection === "offline") return "offline"
  if (session.needs_person) return "attention"
  // No status answer yet (startup or a fresh pairing): neither offline nor a problem.
  if (session.owner_name === undefined || session.owner_name === null) return "connecting"
  var owner = String(session.owner_name)
  if (owner.indexOf("operator:") === 0) return "human"
  if (owner.indexOf("agent:") === 0) return "working"
  if (owner === "human" || session.paused === true) return "paused"
  return "ready"
}
// What you can do about this computer's agents: "resume" a pause a person made, or ibara's own
// pause after a restart when the computer's Resume agents after a restart setting is off
// (system_wait "resume_off"; otherwise ibara's pause ends by itself), "pause" while an agent
// works or the computer is free, or "". From the computer's own state: an approval or a
// question waiting there (Needs Attention) never hides Pause Agents or Resume.
function pauseAction(session) {
  session = asObject(session)
  var state = computerState(session)
  if (state === "paused") return session.pause_origin === "system" && session.system_wait !== "resume_off" ? "" : "resume"
  return state === "working" || state === "ready" ? "pause" : ""
}

function fleetStateLabel(state) {
  return ({ attention: "Needs Attention", offline: "Offline", locked: "Locked", connecting: "Connecting", human: "You have control", working: "Agent working", paused: "Paused", ready: "Ready" })[state] || "Offline"
}

function fleetActor(session) {
  session = asObject(session)
  var owner = String(session.owner_name || "")
  if (owner.indexOf("operator:") === 0) {
    var who = owner.substring(9)
    return session.holds_control === true || (who && who === String(session.operator_principal || "")) ? "you" : "someone on " + who
  }
  if (owner.indexOf("agent:") === 0) return "agent from " + owner.split(":")[1]
  return ""
}

function fleetCounts(computers) {
  var counts = { total: 0, attention: 0, offline: 0, locked: 0, connecting: 0, human: 0, working: 0, paused: 0, ready: 0 }
  var list = Array.isArray(computers) ? computers : []
  for (var i = 0; i < list.length; i++) {
    if (!list[i]) continue
    counts.total += 1
    counts[fleetState(list[i])] += 1
  }
  counts.needs_attention = counts.attention + counts.offline + counts.locked
  counts.in_use = counts.human + counts.working + counts.paused
  return counts
}

function durationLabel(ms) {
  var minutes = Math.floor(Math.max(0, Number(ms) || 0) / 60000)
  if (minutes < 1) return "under 1 min"
  if (minutes < 60) return minutes + " min"
  var hours = Math.floor(minutes / 60)
  return hours < 24 ? hours + " h" : Math.floor(hours / 24) + " d"
}
// The same, in words: "under a minute", "1 minute", "5 hours", "3 days".
function spelledDuration(ms) {
  var minutes = Math.floor(Math.max(0, Number(ms) || 0) / 60000)
  if (minutes < 1) return "under a minute"
  var hours = Math.floor(minutes / 60), days = Math.floor(hours / 24)
  var count = days ? days : hours ? hours : minutes, unit = days ? "day" : hours ? "hour" : "minute"
  return count + " " + unit + (count === 1 ? "" : "s")
}

// One plain line: the live task and how long it has run, else who holds it, else Ready (the same
// word as the state tag).
// Without nowMs the line has no age, so it stays stable between refreshes.
function activityLine(session, nowMs) {
  if (asObject(asObject(session).ibara_update).state === "running") return "Updating ibara…"
  session = asObject(session)
  var state = fleetState(session)
  var task = asObject(session.active_task)
  var title = clip(task.title, 80)
  var started = nowMs ? isoMs(String(task.started_at || "")) : NaN
  var age = isFinite(started) ? " · " + durationLabel(nowMs - started) : ""
  if (state === "offline") {
    var power = ({ restart: "Restarting…", shutdown: "Turned off", sleep: "Asleep", waking: "Waking…" })[session.power_state]
    if (power) return power
    return session.frame ? "No reply · last frame shown" : "No reply"
  }
  if (state === "locked") return "Screen locked · Join to unlock"
  if (state === "human" && session.locked === true) return "Screen locked · type its password in the viewer"
  if (state === "connecting") return "Connecting…"
  if (state === "attention") {
    // Only what waits for your answer: the task, then what it waits for.
    if (computerState(session) !== "attention") {
      var waiting = asObject(session.waiting), asks = waitingCount(session)
      var words = asks > 1 ? asks + " wait for your answer" : Number(waiting.approvals) > 0 ? "waiting for your approval" : "waiting for your answer"
      return title ? title + " · " + words : words.charAt(0).toUpperCase() + words.slice(1)
    }
    // The sentence itself shows over the card's picture and above the computer's tabs.
    if (session.needs_person) {
      var fix = fixDescription(asObject(session.needs_person).fix)
      return fix ? "Needs you · Fix It will " + fix.charAt(0).toLowerCase() + fix.slice(1) : "Needs you · restart it, or check it there"
    }
    if (ATTENTION_TASK_STATES.indexOf(String(task.state || "")) !== -1)
      return (title || "Task") + " · " + (task.state === "waiting_for_human" ? "waiting for you" : String(task.state))
    if (session.connection === "unauthorized" || session.observation === "denied") return "Access not confirmed"
    if (session.trust_state !== undefined && session.trust_state !== "verified") return "Not paired · pair it again from Add Computer"
    return clip(session.frame_error || "Needs attention · open it to see why", 80)
  }
  // Omarchy's own update: while it runs, and once it waits for a restart.
  var omarchy = asObject(session.omarchy_update)
  if (omarchy.state === "running") return "Updating Omarchy…"
  if (omarchy.state === "done" && restartNeeded(session)) return "Updated · restart to finish"
  var jobs = Math.max(0, Number(task.active_jobs) || 0)
  var uncertain = Math.min(jobs, Math.max(0, Number(task.uncertain_jobs) || 0))
  if (state === "working" && jobs > 0) return (title || "Work") + " · " + (jobs - uncertain) + " running" +
    (uncertain ? " · " + uncertain + " awaiting process confirmation" : "") +
    (task.connection === "disconnected" ? " · agent disconnected; work retained" : "") + age
  if (title) return title + age
  var actor = fleetActor(session)
  if (state === "human") return actor === "you" ? "You have control" : "Controlled by " + actor
  if (state === "working") return "Agent working"
  // A pause ibara made itself (after a restart) ends by itself unless the computer's Resume agents
  // after a restart setting is off; a person's pause waits for Resume.
  if (state === "paused" && session.pause_origin === "system")
    return session.system_wait === "resume_off" ? "Paused after a restart · Resume agents after a restart is off" : "Paused after a restart · resuming by itself"
  if (state === "paused") return "Paused · agents stopped"
  return "Ready"
}

function decorateSession(session) {
  if (!session || typeof session !== "object") return session
  return Object.assign({}, session, { fleet_state: fleetState(session), actor: fleetActor(session), activity: activityLine(session, 0) })
}

// "Who can use it" on the Screen tab, from the computer's own access table (operator-access):
// each paired identity with at least one permission it has or can ask for, you first, with its
// rule for every permission in one fixed order (watch, files, control, agents, admin), so the
// rail shows them as short marks in columns. The local owner is the computer's own person and is
// not listed. Names read as on the Access tab: "You (riley)", "sam's laptop", "codex@relay".
var PERMISSION_MARKS = [
  { key: "watch", label: "Watch" }, { key: "files", label: "Files" }, { key: "control", label: "Join" },
  { key: "agents", label: "Agent Tasks" }, { key: "administer", label: "Administer" }]
function whoCanUse(access, ownPrincipal) {
  var rows = listOf(asObject(access), "rows"), own = String(ownPrincipal || ""), out = []
  for (var i = 0; i < rows.length; i++) {
    var row = asObject(rows[i]), subject = String(row.subject || "")
    if (!subject || subject === "owner" || row.paired === false) continue
    var capabilities = asObject(row.capabilities), marks = [], any = false
    for (var p = 0; p < PERMISSION_MARKS.length; p++) {
      var rule = String(capabilities[PERMISSION_MARKS[p].key] || "deny")
      if (rule === "allow" || rule === "ask") any = true
      else rule = "deny"
      marks.push({ key: PERMISSION_MARKS[p].key, label: PERMISSION_MARKS[p].label, rule: rule })
    }
    if (!any) continue
    var you = !!own && row.kind === "computer" && subject === own
    var label = you ? "You (" + subject + ")" : row.owner ? String(row.owner) + "'s " + String(row.computer_name || subject) : subject
    out.push({ subject: subject, label: label, you: you, agent: row.kind === "agent", marks: marks })
  }
  out.sort(function(a, b) { return (b.you ? 1 : 0) - (a.you ? 1 : 0) })
  return out
}
// Who can use it in one line: "3 people, 1 agent". A paired computer stands for its person.
function peopleAndAgents(rows) {
  var list = Array.isArray(rows) ? rows : [], agents = 0
  for (var i = 0; i < list.length; i++) if (list[i] && list[i].agent) agents += 1
  var people = list.length - agents, parts = []
  if (people) parts.push(people + (people === 1 ? " person" : " people"))
  if (agents) parts.push(agents + (agents === 1 ? " agent" : " agents"))
  return parts.join(", ")
}

// ---- everyday features over the pairing route (release wave 2)

// A time ibarad reports as ISO text or epoch milliseconds, as epoch milliseconds (NaN if neither).
function timeMs(value) {
  if (typeof value === "number" && isFinite(value) && value > 0) return value
  return isoMs(String(value || ""))
}

// What a computer's self-repair could not fix and a person can (repair.needs_person). Fix It runs
// `fix`; a value the console doesn't know shows the sentence without the button.
var REPAIR_FIXES = ["retry_cleanup", "reconnect_display", "restart_viewer", "restart_ibara"]
function needsPersonView(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  var fix = REPAIR_FIXES.indexOf(value.fix) !== -1 ? String(value.fix) : ""
  var message = clip(value.message, 240)
  if (!message && !fix) return null
  return { code: /^[a-z_]{1,40}$/.test(String(value.code || "")) ? String(value.code) : "", message: message || "This computer needs you.", fix: fix }
}
// What Fix It does, for its tooltip.
function fixDescription(fix) {
  return ({ retry_cleanup: "Retry cleanup there", reconnect_display: "Add its screen again", restart_viewer: "Restart screen sharing there", restart_ibara: "Restart ibara there" })[fix] || ""
}
// An error with a known fix offers Fix It beside its message. Errors arrive as a code and a
// message that often starts with the core's own code ("CONTROL_UNSETTLED: …").
function fixForError(code, message) {
  var text = String(code || "") + " " + String(message || "")
  if (/CONTROL_UNSETTLED|work (is )?unsettled|hand ?back (did not|didn't) finish/i.test(text)) return "retry_cleanup"
  if (/viewer[^.]*(not ready|unavailable|faulted|failed|stopped)|screen sharing[^.]*(stopped|failed|unavailable)/i.test(text)) return "restart_viewer"
  if (/no display|display[^.]*(missing|unavailable|disappeared)|lost its screen/i.test(text)) return "reconnect_display"
  return ""
}

// Wake-on-network details a computer reports (operator-health / operator-status `wake`).
function wakeView(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  var mac = String(value.mac || "")
  if (!/^[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){5}$/.test(mac)) return null
  return { mac: mac, kind: value.kind === "wifi" ? "wifi" : "ethernet", from_off: value.from_off === true, subnet: clip(value.subnet, 64), ifname: clip(value.ifname, 32) }
}

// Omarchy's own update there (operator-status `omarchy_update`): the last or current run, or null.
// finished_at is epoch ms (0 while it runs); message says why a failed one failed.
// Only packaged versions participate in the stable release counter. Development
// and legacy unqualified identities are not evidence of an outdated package.
function releaseBehind(installed, available) {
  var packaged = /^[0-9]+\.[0-9]+\.[0-9]+-[0-9]+$/
  return packaged.test(String(installed || "")) && packaged.test(String(available || "")) && versionCompare(installed, available) < 0
}
function versionCompare(a, b) {
  var left = String(a || "").match(/[0-9]+|[a-zA-Z]+/g) || [], right = String(b || "").match(/[0-9]+|[a-zA-Z]+/g) || []
  for (var i = 0; i < Math.max(left.length, right.length); i++) {
    var x = left[i] || "0", y = right[i] || "0"
    if (/^[0-9]+$/.test(x) && /^[0-9]+$/.test(y)) { if (Number(x) !== Number(y)) return Number(x) > Number(y) ? 1 : -1 }
    else if (x !== y) return x > y ? 1 : -1
  }
  return 0
}
function ibaraUpdateView(value) {
  if (!value || typeof value !== "object" || ["running", "done", "failed"].indexOf(value.state) === -1) return null
  return { state: value.state, version: clip(value.version, 64), from: clip(value.from, 64), started_at: timeMs(value.started_at), finished_at: timeMs(value.finished_at), shell: clip(value.shell, 32), message: clip(value.message, 240) }
}
function releaseNotes(release, since) {
  if (!release) return []
  var history = Array.isArray(release.history) ? release.history.filter(function(r) { return r && r.version && versionCompare(r.version, since) > 0 }) : []
  var rows = []
  for (var i = 0; i < history.length; i++) rows = rows.concat(["ibara " + history[i].version], history[i].notes || [])
  if (!history.some(function(r) { return r.version === release.version })) rows = rows.concat(["ibara " + release.version], release.notes || [])
  return rows
}
function omarchyUpdateView(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  if (["running", "done", "failed"].indexOf(value.state) === -1) return null
  var finished = timeMs(value.finished_at)
  return { state: String(value.state), finished_at: isFinite(finished) ? finished : 0, restart_needed: value.restart_needed === true, message: clip(value.message, 240) }
}
// Omarchy's update finished and the computer needs a restart to finish it (its kernel changed).
function restartNeeded(session) {
  var update = asObject(asObject(session).omarchy_update)
  return update.restart_needed === true && update.state !== "running"
}

// fleet-attention: approvals to answer, agents' questions and computers that need a person,
// oldest first. A malformed item is dropped rather than shown half. An approval's `summary` is
// what a person reads, `facts` its Details as labeled lines and `request` the request behind it,
// exactly as sent, for Copy Request; see approvalWords. A question's `options` are the answers
// its agent offered, exactly as sent (an answer must match one); none means any short answer.
function attentionView(data) {
  var list = listOf(data, "items"), out = []
  for (var i = 0; i < list.length && out.length < 100; i++) {
    var item = asObject(list[i])
    if (!requestId(item.computer_id) || !requestId(item.ref) || !/^[a-z_.]{1,40}$/.test(String(item.kind || ""))) continue
    var at = timeMs(item.at)
    // A login request (kind `login`) is answered on the sharing computer's console: see loginRequestView.
    var login = item.kind === "login" ? loginRequestView(item.details) : null
    if (item.kind === "login" && !login) continue
    var words = item.kind === "approval" ? approvalWords(item) : { summary: clip(item.summary, 400), facts: [], request: "", described: true }
    var options = item.kind === "question" && Array.isArray(item.options)
      ? item.options.filter(function(o) { return typeof o === "string" && o.trim() !== "" && o.length <= 256 }).slice(0, 20) : []
    var asked = approvalAsks(item)
    out.push({ computer_id: String(item.computer_id), label: clip(item.label, 128), ref: String(item.ref), kind: String(item.kind),
      summary: words.summary, facts: words.facts, request: words.request, described: words.described, options: options, at: isFinite(at) ? at : 0, unreachable: item.unreachable === true,
      agent: login ? login.agent : asked.agent, task_ref: requestId(item.task_ref) ? String(item.task_ref) : "", effect: asked.effect, stopAsking: asked.stopAsking, login: login })
  }
  out.sort(function(a, b) { return a.at - b.at || (a.ref < b.ref ? -1 : a.ref > b.ref ? 1 : 0) })
  return out
}

// ---- login sharing. A site is one registrable domain, lowercase ("irs.gov", "sos.ok.gov").
var LOGIN_SITE = /^(?=.{1,253}$)[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/
var LOGIN_RULES = ["allow", "ask", "deny"]
function loginSite(value) {
  var site = String(value || "").trim().toLowerCase()
  return LOGIN_SITE.test(site) ? site : ""
}
// "a", "a and b", "a, b and c".
function listWords(names) {
  var list = Array.isArray(names) ? names : []
  return list.length < 2 ? list.join("") : list.slice(0, -1).join(", ") + " and " + list[list.length - 1]
}
// A login request's details (attention kind `login`): who asks, for which task, the sites (each
// with the site it signs in through, `via`), the page, whether the agent is from one of your own
// computers and the sharing computer's name. Null without a site to share.
function loginRequestView(details) {
  var d = asObject(details), task = asObject(d.task), sites = [], seen = {}
  var list = Array.isArray(d.sites) ? d.sites : []
  for (var i = 0; i < list.length && sites.length < 20; i++) {
    var entry = asObject(list[i]), site = loginSite(entry.site)
    if (!site || seen[site]) continue
    seen[site] = true
    var via = loginSite(entry.via)
    sites.push({ site: site, via: via !== site ? via : "", state: clip(entry.state, 40), method: clip(entry.method, 40) })
  }
  if (!sites.length) return null
  var agent = typeof d.agent === "string" && /^[a-z0-9@_.-]{1,100}$/.test(d.agent) ? d.agent : ""
  return { agent: agent, principal: clip(d.principal, 100), taskRef: requestId(task.task_ref) ? String(task.task_ref) : "", goal: clip(task.goal, 200),
    sites: sites, page: clip(d.page, 300), own: d.own === true, sourceLabel: clip(d.source_label, 128),
    assistance: d.assistance && d.assistance.version === 1 ? d.assistance : null }
}
// Two-site sign-ins, as a person reads them: "irs.gov signs in through id.me".
function loginThrough(sites) {
  return (Array.isArray(sites) ? sites : []).filter(function(s) { return s && s.via }).map(function(s) { return s.site + " signs in through " + s.via })
}
// `login-settings`: this console's login sharing, its browsers, the All Computers rules, each
// computer's standing and the sites that rejected a shared login lately.
function loginSettingsView(data) {
  var d = asObject(data), browsers = [], rules = {}, computers = {}, rejected = []
  var list = Array.isArray(d.browsers) ? d.browsers : []
  for (var i = 0; i < list.length && browsers.length < 10; i++) {
    var b = asObject(list[i])
    if (!/^[a-z0-9_-]{1,40}$/.test(String(b.browser || ""))) continue
    var profiles = (Array.isArray(b.profiles) ? b.profiles : []).filter(function(p) { return typeof p === "string" && p.trim() !== "" && p.length <= 100 }).slice(0, 20)
    browsers.push({ browser: String(b.browser), name: clip(b.name, 60) || String(b.browser), profiles: profiles, supported: b.supported === true, reason: clip(b.reason, 200) })
  }
  var all = asObject(d.all_rules)
  for (var key in all) { var site = loginSite(key); if (site && LOGIN_RULES.indexOf(all[key]) !== -1) rules[site] = all[key] }
  var byId = asObject(d.computers)
  for (var id in byId) {
    if (!requestId(id)) continue
    var c = asObject(byId[id])
    computers[id] = { configured: c.configured === true, sourceElsewhere: clip(c.source_elsewhere, 128), sitesAllowed: Math.max(0, Math.floor(Number(c.sites_allowed) || 0)), older: c.older === true }
  }
  var gone = Array.isArray(d.rejected) ? d.rejected : []
  for (var r = 0; r < gone.length && rejected.length < 20; r++) {
    var entry = asObject(gone[r]), at = Number(entry.at_ms)
    if (requestId(entry.computer) && loginSite(entry.site) && isFinite(at) && at > 0) rejected.push({ computer: String(entry.computer), site: loginSite(entry.site), at: at })
  }
  return { enabled: d.enabled === true, decided: d.decided === true, browser: String(d.browser || ""), profile: clip(d.profile, 100), label: clip(d.label, 128),
    siteCount: Math.max(Object.keys(rules).length, Math.floor(Number(d.site_count) || 0)),
    ruleRows: (Array.isArray(d.rule_rows) ? d.rule_rows : Object.keys(rules).map(function(site) { return {site: site, computer: "all", rule: rules[site]} })).filter(function(row) { return row && loginSite(row.site) && (row.computer === "all" || computers[row.computer]) && LOGIN_RULES.indexOf(row.rule) !== -1 }),
    connected: d.connected === true, installable: d.installable === true, browsers: browsers, allRules: rules, computers: computers,
    targetRole: d.target_role === true, rejected: rejected }
}
// The browser's own name ("Brave"), for the words that name it; "your browser" when unknown.
function loginBrowserName(settings, browser) {
  var list = settings && Array.isArray(settings.browsers) ? settings.browsers : [], id = String(browser || (settings ? settings.browser : "") || "")
  for (var i = 0; i < list.length; i++) if (list[i].browser === id) return list[i].name
  return "your browser"
}
// One answer to a login request, for `login-answer --decisions`: every site the person left
// ticked gets the choice (share, share_all, never), and every other site Don't Share.
function loginDecisions(sites, unticked, choice) {
  var out = {}, off = unticked && typeof unticked === "object" ? unticked : {}
  var list = Array.isArray(sites) ? sites : []
  for (var i = 0; i < list.length; i++) if (!off[list[i].site]) out[list[i].site] = choice
  return out
}
// What the person reads once an answer went (`login-answer` → sites: [{site, outcome}]):
//   note: what happened, a message that leaves by itself;
//   waiting: the sites waiting for the browser to open, said until dismissed;
//   unknown: sites whose write couldn't be confirmed, said until dismissed;
//   signedOut: sites to sign in to in the browser before Retry (the request stays open);
//   off: sharing was off.
function loginAnswerWords(outcomes, browser, computer) {
  var by = { shared: [], declined: [], denied: [], signed_out_there: [], waiting_for_browser: [], sharing_off: [], unknown: [] }
  var list = Array.isArray(outcomes) ? outcomes : []
  for (var i = 0; i < list.length; i++) {
    var o = asObject(list[i]), site = loginSite(o.site)
    if (site && by[o.outcome]) by[o.outcome].push(site)
  }
  var name = String(browser || "your browser"), where = clip(computer, 128) || "that computer", notes = []
  if (by.shared.length) notes.push("Shared your login for " + listWords(by.shared) + " with " + where + ".")
  if (by.denied.length) notes.push(listWords(by.denied) + (by.denied.length === 1 ? " is" : " are") + " never shared now, on any computer.")
  if (by.declined.length) notes.push("Didn't share " + listWords(by.declined) + ".")
  return {
    note: notes.join(" "),
    waiting: by.waiting_for_browser.length ? "Open " + name + " to share your login for " + listWords(by.waiting_for_browser) + ". It goes to " + where + " once " + name + " runs." : "",
    unknown: loginUnknownWords(by.unknown),
    signedOut: by.signed_out_there,
    off: by.sharing_off.length > 0
  }
}
// A write ibara couldn't confirm (outcome `unknown`).
function loginUnknownWords(sites) {
  var list = Array.isArray(sites) ? sites : []
  return list.length ? "Couldn't confirm the login for " + listWords(list) + " was written. Try Share again or use Join." : ""
}
// "You're not signed in to sos.ok.gov in Brave. Sign in there, then choose Retry."
function loginSignedOutWords(sites, browser) {
  var list = Array.isArray(sites) ? sites : []
  if (!list.length) return ""
  return "You're not signed in to " + listWords(list) + " in " + String(browser || "your browser") + ". Sign in there, then choose Retry."
}
// `login-rows --computer ID`: one row per site with a rule there or for All Computers.
var LOGIN_RESULTS = ["worked", "site_rejected"]
function loginRowsView(data) {
  var d = asObject(data), rows = [], denied = []
  var list = Array.isArray(d.rows) ? d.rows : []
  for (var i = 0; i < list.length && rows.length < 500; i++) {
    var r = asObject(list[i]), site = loginSite(r.site)
    if (!site || LOGIN_RULES.indexOf(r.rule) === -1) continue
    var shared = Number(r.last_shared_ms), resultAt = Number(r.last_result_at_ms)
    rows.push({ site: site, rule: String(r.rule), ownRule: LOGIN_RULES.indexOf(r.own_rule) !== -1 ? String(r.own_rule) : "",
      allRule: LOGIN_RULES.indexOf(r.all_rule) !== -1 ? String(r.all_rule) : "", firstGoal: clip(r.first_goal, 200),
      lastShared: isFinite(shared) && shared > 0 ? shared : 0, lastResult: LOGIN_RESULTS.indexOf(r.last_result) !== -1 ? String(r.last_result) : "",
      lastResultAt: LOGIN_RESULTS.indexOf(r.last_result) !== -1 && isFinite(resultAt) && resultAt > 0 ? resultAt : 0 })
  }
  var deniedList = Array.isArray(d.denied_all) ? d.denied_all : []
  for (var j = 0; j < deniedList.length && denied.length < 500; j++) { var s = loginSite(deniedList[j]); if (s) denied.push(s) }
  rows.sort(function(a, b) { return a.site < b.site ? -1 : a.site > b.site ? 1 : 0 })
  return { rows: rows, deniedAll: denied }
}
// Sites that rejected a shared login and haven't had their toast yet: each entry once (by
// computer, site and time), and none from before `since` (what came before this console started).
function loginRejectedFresh(rejected, seen, since) {
  var done = seen && typeof seen === "object" ? seen : {}, out = []
  var list = Array.isArray(rejected) ? rejected : []
  for (var i = 0; i < list.length; i++) {
    var r = list[i], key = r.computer + "|" + r.site + "|" + r.at
    if (!done[key] && r.at >= Number(since || 0)) out.push({ key: key, computer: r.computer, site: r.site, at: r.at })
  }
  return out
}

// Words that never show, only in the request Copy Request copies: JSON, window addresses,
// process ids and agent tool names.
var MACHINE_WORDS = /[{}]|\b0x[0-9a-f]{4,}\b|\bpids?\b|\b(computer|browser)_[a-z]+\b/i
// ibara refuses to raise an approval whose details are over 12,000 characters; this is room to spare.
var REQUEST_CHARS = 20000

// An approval as a person reads it. A computer on an older ibara words some approvals as code
// ("Approve computer_act send step: … {"pid": …}"); those read as plain words, and Copy Request
// copies the computer's own text. `described` is false for them.
function approvalWords(item) {
  var summary = clip(item.summary, 400), structured = item.details && typeof item.details === "object" && !Array.isArray(item.details)
  var facts = structured ? approvalFacts(item.details, clip(item.label, 128)) : []
  var request = structured ? JSON.stringify(item.details, null, 2) : ""
  if (summary && !MACHINE_WORDS.test(summary)) return { summary: summary, facts: facts, request: request.substring(0, REQUEST_CHARS), described: true }
  var raw = String(item.summary || "").trim()
  return { summary: "An agent is waiting for your approval. What it wants to do is under Details.", facts: facts,
    request: (raw + (raw && request ? "\n\n" : "") + request).substring(0, REQUEST_CHARS), described: false }
}

// An approval's Details as a person reads them, one labeled line each: who asks, what it wants
// to do, where (the app and the computer), the page, the window's title and the task. A value
// that reads as code is left out; the request itself is only copied.
function approvalFacts(details, computer) {
  var place = asObject(details.where), task = asObject(details.task), facts = []
  var add = function(label, value) {
    var text = clip(value, 200)
    if (text && !MACHINE_WORDS.test(text)) facts.push({ label: label, value: text })
  }
  var app = clip(place.app, 60)
  add("Who", typeof details.agent === "string" ? details.agent : "")
  add("What", approvalWhat(details))
  add("Where", app && computer ? app + " on " + computer : app || computer)
  add("Page", place.page)
  add("Window title", place.window_title)
  add("Task", task.goal)
  if (details.target_changed === true) add("Note", "What it acts on changed since you approved it, so it asks again.")
  return facts
}
// What the request does, in words: "Press Return, which sends something", "Take control",
// "Stop asking you before it sends, spends money or deletes". Empty when it can't be said.
function approvalWhat(details) {
  var request = asObject(details.request)
  if (request.op === "stop_asking") {
    var kinds = (Array.isArray(request.kinds) ? request.kinds : []).filter(function(k) { return !!KIND_VERBS[k] }).map(function(k) { return KIND_VERBS[k] })
    return "Stop asking you before it " + (kinds.length > 1 ? kinds.slice(0, -1).join(", ") + " or " + kinds[kinds.length - 1] : kinds[0] || "acts")
  }
  if (request.op === "take_control" || details.capability === "control") return "Take control"
  var action = asObject(asObject(request.step).action)
  var doing = action.kind === "key" && typeof action.keys === "string" && action.keys ? "Press " + clip(action.keys, 40)
    : action.kind === "type" ? "Type text" : action.kind === "click" ? "Click" : ""
  var why = ({ send: "sends something", spend: "spends money", destructive: "deletes or overwrites something" })[details.effect] || ""
  if (doing && why) return doing + ", which " + why
  return doing || (why ? why.charAt(0).toUpperCase() + why.slice(1) : "")
}

// What an approval's request lets a person do besides Approve and Deny. An agent's send, spend
// or delete step (`effect`) may be answered Always Allow; an agent's request to stop asking
// (`stopAsking`) is answered Allow or Not Now. `agent` is who asks, e.g. codex@lumen.
var ASK_FIRST_KINDS = ["send", "spend", "destructive"]
function approvalAsks(item) {
  var details = item && item.details && typeof item.details === "object" ? item.details : {}
  var request = details.request && typeof details.request === "object" ? details.request : {}
  var agent = typeof details.agent === "string" && /^[a-z0-9@_.-]{1,100}$/.test(details.agent) ? details.agent : ""
  var stopAsking = item && item.kind === "approval" && !!agent && request.op === "stop_asking"
  var effect = item && item.kind === "approval" && !!agent && !stopAsking && ASK_FIRST_KINDS.indexOf(details.effect) !== -1 ? details.effect : ""
  return { agent: agent, effect: effect, stopAsking: stopAsking }
}

// What a person reads once Always Allow or Allow changed an agent's rules on a computer, and
// where to undo it. `allowed` is the computer's {agent, kinds}; `always` is Always Allow.
var KIND_VERBS = { send: "sends", spend: "spends money", destructive: "deletes" }
function allowedNotice(label, allowed, always) {
  var agent = allowed && typeof allowed.agent === "string" ? clip(allowed.agent, 100) : "The agent"
  var kinds = (allowed && Array.isArray(allowed.kinds) ? allowed.kinds : []).filter(function(k) { return !!KIND_VERBS[k] }).map(function(k) { return KIND_VERBS[k] })
  var doing = kinds.length > 1 ? kinds.slice(0, -1).join(", ") + " and " + kinds[kinds.length - 1] : kinds[0] || "acts"
  var where = clip(label, 128) || "that computer"
  var undo = " To undo it, open " + where + "'s Access tab and choose " + agent + "."
  return always ? "Approved on " + where + ". From now on, " + agent + " " + doing + " there without asking you." + undo
    : agent + " now " + doing + " on " + where + " without asking you." + undo
}

// What Ask before agents send, spend or delete now means, after "… is now Off." `key` is
// agents_ask_first (Settings, every computer) or ask_first (one computer: on, off or same,
// whose label says what Settings chose). Empty when there is nothing more to say.
function askFirstMeaning(key, value, setting) {
  var off = key === "agents_ask_first" ? value === "false" : value === "off" || (value === "same" && /\(off\)$/.test(choiceLabel(setting, value)))
  if (key === "agents_ask_first") return off ? " Your own computers' agents now send, spend and delete without asking you, except where a computer or an agent is set to ask. Agents from someone else's computer still ask." : " Agents on your computers ask you first again, except where a computer or an agent is set not to."
  return off ? " Your own computers' agents now send, spend and delete here without asking you, except one set to ask under Access. Agents from someone else's computer still ask." : " Agents here ask you first, except one set not to under Access."
}
function choiceLabel(setting, value) {
  var choices = setting && Array.isArray(setting.choices) ? setting.choices : []
  for (var i = 0; i < choices.length; i++) if (choices[i] && choices[i].value === value) return String(choices[i].label || "")
  return ""
}

// A desktop notification's text for an approval: its words, or, when the computer gave none
// a person can read, where to look (a notification has no Details).
function requestWho(item, session) {
  var who = item && item.agent ? item.agent : "An agent"
  var task = session && session.active_task
  if (who === "An agent" && task && item.task_ref && task.task_ref === item.task_ref && task.principal)
    who = task.principal
  return clip(who, 100)
}
function requestHeading(item, label, session) {
  return requestWho(item, session) + " on " + clip(label || (item && item.label) || "Unknown computer", 128)
}
function requestPopupText(item) {
  if (!item) return "Waiting for your answer"
  if (item.login) return "Use your logins for " + listWords(item.login.sites.map(function(s) { return s.site }))
  if (item.kind === "approval") {
    var facts = Array.isArray(item.facts) ? item.facts : []
    for (var i = 0; i < facts.length; i++) if (facts[i].label === "What") return clip(facts[i].value, 160)
    if (!item.described) return "Needs your approval · Open for details"
  }
  return clip(item.summary, 160)
}
function noticeEscape(value) {
  return String(value || "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
}
function requestNoticeBody(item, label, session, what) {
  return noticeEscape(clip(what, 200)) + "\n" + noticeEscape(requestHeading(item, label, session))
}
function approvalNoticeBody(item, label, session) {
  var facts = item && Array.isArray(item.facts) ? item.facts : []
  var what = "", page = ""
  for (var i = 0; i < facts.length; i++) {
    if (facts[i].label === "What") what = facts[i].value
    if (facts[i].label === "Page") page = facts[i].value
  }
  if (!what && item && item.described) what = item.summary
  if (!what) what = "Approval requested. Open the ibara console to see what it wants to do."
  return requestNoticeBody(item, label, session, what + (page ? " on " + page : ""))
}

// Settings as ibarad returns them (`settings get`, `operator-settings get`): sections of settings,
// each with its type, current value and default. Choices may be plain ids or {value, label}.
var SETTING_TYPES = ["bool", "choice", "number", "text", "shortcut"]
function settingText(value) {
  if (value === true) return "true"
  if (value === false) return "false"
  if (value === null || value === undefined) return ""
  return String(value)
}
function settingView(value) {
  var s = asObject(value)
  if (!/^[a-z][a-z0-9_.-]{0,63}$/.test(String(s.key || "")) || SETTING_TYPES.indexOf(s.type) === -1) return null
  var choices = []
  if (Array.isArray(s.choices)) for (var i = 0; i < s.choices.length && choices.length < 40; i++) {
    var c = s.choices[i]
    if (c && typeof c === "object") { if (c.value !== undefined && c.value !== null) choices.push({ value: settingText(c.value), label: clip(c.label || c.value, 60) }) }
    else if (c !== undefined && c !== null) choices.push({ value: settingText(c), label: clip(c, 60) })
  }
  if (s.type === "choice" && !choices.length) return null
  var view = { key: String(s.key), title: clip(s.title || s.key, 80), help: clip(s.help, 600), type: s.type, choices: choices,
    value: s.value, default: s.default, scope: String(s.scope || "") }
  if (typeof s.min === "number" && isFinite(s.min)) view.min = s.min
  if (typeof s.max === "number" && isFinite(s.max)) view.max = s.max
  return view
}
function settingsSections(data) {
  var list = listOf(data, "sections"), out = []
  for (var i = 0; i < list.length && out.length < 20; i++) {
    var section = asObject(list[i]), settings = []
    var rows = Array.isArray(section.settings) ? section.settings : []
    for (var j = 0; j < rows.length && settings.length < 60; j++) { var s = settingView(rows[j]); if (s) settings.push(s) }
    if (!/^[a-z][a-z0-9_.-]{0,63}$/.test(String(section.id || "")) || !settings.length) continue
    out.push({ id: String(section.id), title: clip(section.title || section.id, 80), settings: settings })
  }
  return out
}
// A set or reset reply folded into the list shown: one setting, or {sections:[…]} replacing
// those sections, or {settings:[…]}.
function settingsAfter(sections, data) {
  data = asObject(data)
  var replaced = Array.isArray(data.sections) ? settingsSections(data) : []
  var singles = Array.isArray(data.settings) ? data.settings : (data.key !== undefined ? [data] : [])
  var changed = ({})
  for (var i = 0; i < singles.length; i++) { var s = settingView(singles[i]); if (s) changed[s.key] = s }
  return (Array.isArray(sections) ? sections : []).map(function(section) {
    for (var r = 0; r < replaced.length; r++) if (replaced[r].id === section.id) return replaced[r]
    return { id: section.id, title: section.title, settings: section.settings.map(function(setting) { return changed[setting.key] || setting }) }
  })
}
// One setting's current value from a sections list, else `fallback`.
function settingValue(sections, key, fallback) {
  var list = Array.isArray(sections) ? sections : []
  for (var i = 0; i < list.length; i++) for (var j = 0; j < list[i].settings.length; j++)
    if (list[i].settings[j].key === key) { var v = list[i].settings[j].value; return v === undefined || v === null ? fallback : v }
  return fallback
}
function findSetting(sections, key) {
  var list = Array.isArray(sections) ? sections : []
  for (var i = 0; i < list.length; i++) for (var j = 0; j < list[i].settings.length; j++) if (list[i].settings[j].key === key) return list[i].settings[j]
  return null
}
// Starting this computer without its disk password (`unattended-boot`), as one more switch on the
// console's Settings page, under This Computer. No section when the disk is not encrypted or was
// set up outside ibara (nothing to switch), or when the answer is not one ibara knows. While
// Omarchy signs in automatically (or the lock is on), a second switch locks the screen at sign-in
// (`unattended-boot-lock`); starting without the password needs it then.
var UNATTENDED_BOOT_STATES = ["off", "waiting", "on", "changed"]
function unattendedBootSections(data) {
  data = asObject(data)
  var state = String(data.state || "")
  if (UNATTENDED_BOOT_STATES.indexOf(state) === -1) return []
  var help = clip(data.help, 200)
  if (data.note) help += (help ? " " : "") + clip(data.note, 200)
  var settings = [{
    key: "unattended_boot", title: "Start Without the Disk Password", help: "Start this computer without typing the disk password.", details: help, type: "bool", choices: [],
    value: state !== "off", default: false, scope: "console", available: data.available === true }]
  if ((typeof data.automatic_sign_in === "string" && data.automatic_sign_in) || data.lock_at_sign_in === true)
    settings.push({ key: "lock_at_sign_in", title: "Lock the Screen at Sign-In", help: "Lock the screen after automatic sign-in.", details: clip(data.lock_help, 200), type: "bool", choices: [],
      value: data.lock_at_sign_in === true, default: false, scope: "console", available: true })
  return [{ id: "this_computer", title: "This Computer", settings: settings }]
}

// "While you were away" (`away`): per computer, what happened since this console last looked.
// Core sends up to 100 events per computer, oldest first; the newest 50 are kept, and `total`
// counts every one that came, so "+N earlier" is right.
function awayView(data) {
  data = asObject(data)
  var list = listOf(data, "computers"), out = []
  for (var i = 0; i < list.length && out.length < 100; i++) {
    var c = asObject(list[i]), events = []
    var rows = Array.isArray(c.events) ? c.events.slice(-100) : []
    for (var j = 0; j < rows.length; j++) {
      var e = asObject(rows[j]), at = timeMs(e.at)
      if (!e.summary) continue
      events.push({ at: isFinite(at) ? at : 0, kind: clip(e.kind, 40), actor: clip(e.actor, 64), summary: clip(e.summary, 200) })
    }
    if (!requestId(c.computer_id) || !events.length) continue
    events.sort(function(a, b) { return a.at - b.at })
    out.push({ computer_id: String(c.computer_id), label: clip(c.label, 128), events: events.slice(-50), total: events.length })
  }
  var since = timeMs(data.since)
  return { since: isFinite(since) ? since : 0, computers: out }
}

// A While you were away event's short tag and the state color it takes (Tokens.stateColor).
function awayEventTag(kind, summary) {
  var k = String(kind || ""), failed = /\b(fail|failed|couldn't|could not|error|refused|denied)\b/i.test(String(summary || ""))
  if (k === "task_began") return { label: "Started", tone: "human" }
  if (k === "task_finished") return failed ? { label: "Ended", tone: "attention" } : { label: "Finished", tone: "ready" }
  if (k === "attention.raised") return { label: "Asked", tone: "paused" }
  if (k === "attention.answered") return { label: "Answered", tone: "working" }
  if (k === "attention.expired") return { label: "Expired", tone: "offline" }
  if (k.indexOf("window") === 0) return { label: "Window", tone: "working" }
  if (k.indexOf("login") !== -1) return { label: "Login", tone: failed ? "attention" : "working" }
  return { label: sentence(k.replace(/[._]/g, " ")).split(" ")[0] || "Event", tone: failed ? "attention" : "offline" }
}

// Apply Theme to Fleet (`theme-fleet`): the theme and what happened on each computer.
var THEME_STATES = ["applied", "offline", "failed"]
function themeResultsView(data) {
  data = asObject(data)
  var list = listOf(data, "results"), out = []
  for (var i = 0; i < list.length && out.length < 200; i++) {
    var r = asObject(list[i])
    if (!requestId(r.computer_id) || THEME_STATES.indexOf(r.state) === -1) continue
    out.push({ computer_id: String(r.computer_id), label: clip(r.label, 128), state: r.state, message: clip(r.message, 240) })
  }
  return { theme: clip(data.theme, 80), results: out }
}

// ---- what turns the bar red, and the toast that says why. The bar counts approvals, agents'
// questions and computers that need a person, else shows a red dot for a computer that is
// offline or needs attention; each has a standing toast in the console on every page, and a
// toast put away takes its part of the red with it. A computer counts as offline or needing
// attention only once that has lasted PROBLEM_AFTER_MS, so an update or a restart never raises it.
var PROBLEM_AFTER_MS = 60000

// Computers that need a person, one each: fleet-attention's items other than approvals, login
// requests and questions (its self-repair couldn't fix something), then a computer whose own status says so
// while it is on. `hidden` maps a computer to the message put away there; it shows again once the
// message changes. → [{ computer_id, label, kind, message, fix }], `fix` a known Fix It or "".
function needsYouView(items, computers, hidden) {
  var seen = {}, out = [], put = hidden && typeof hidden === "object" ? hidden : {}
  var add = function(id, label, kind, message, fix) {
    if (!id || seen[id] || !message || put[id] === message) return
    seen[id] = true
    out.push({ computer_id: id, label: label, kind: kind, unsupported: kind === "unsupported", message: message, fix: fixDescription(fix) ? String(fix) : "" })
  }
  var list = Array.isArray(items) ? items : []
  for (var i = 0; i < list.length; i++) {
    var item = asObject(list[i])
    if (item.kind === "approval" || item.kind === "question" || item.kind === "login") continue
    add(String(item.computer_id || ""), clip(item.label, 128), item.ref && item.kind !== "repair" ? "unsupported" : String(item.kind || ""), item.ref && item.kind !== "repair" ? "This needs a newer ibara." : clip(item.summary, 240) || "This computer needs you.", item.kind === "repair" ? item.ref : "")
  }
  var rows = Array.isArray(computers) ? computers : []
  for (var j = 0; j < rows.length; j++) {
    var c = asObject(rows[j]), need = asObject(c.needs_person)
    if (need.message && computerState(c) !== "offline") add(String(c.computer_id || ""), clip(c.label, 128), "repair", String(need.message), need.fix)
  }
  return out
}

// When each computer that is offline, locked or needs attention became so: `since` from before,
// kept while the problem lasts (offline then needing attention is one problem), dropped once it ends.
function problemSince(computers, since, nowMs) {
  var before = since && typeof since === "object" ? since : {}, out = {}
  var rows = Array.isArray(computers) ? computers : []
  for (var i = 0; i < rows.length; i++) {
    var c = asObject(rows[i]), id = String(c.computer_id || ""), state = computerState(c)
    if (id && (state === "offline" || state === "locked" || state === "attention")) out[id] = before[id] || nowMs
  }
  return out
}

// What the toast says about a computer that is offline, locked or needs attention: what is wrong,
// then what to do. `wake`: whether ibara can wake it.
function problemWords(session, label) {
  session = asObject(session)
  var name = String(label || "This computer"), wake = !!session.wake
  if (computerState(session) === "offline") {
    var off = session.power_state === "shutdown" || session.power_state === "sleep"
    var heading = ({ shutdown: name + " is turned off", sleep: name + " is asleep", restart: name + " hasn't come back from its restart" })[session.power_state] || name + " isn't answering"
    var body = off ? (wake ? "Choose Wake to turn it on." : "ibara has no way to wake it from here. Turn it on there.")
      : "Check that it's on and online" + (wake ? ", or choose Wake" : "") + ". ibara keeps trying and clears this once it answers."
    return { heading: heading, body: body, wake: wake }
  }
  if (computerState(session) === "locked")
    return { heading: name + "'s screen is locked", body: "Its agents can't use it until it's unlocked. Choose Join on its card to unlock it.", wake: false }
  var connection = String(session.connection || ""), task = asObject(session.active_task)
  var why
  if (session.trust_state !== undefined && session.trust_state !== "verified") why = "It isn't paired with this computer anymore. Pair it again from Add Computer."
  else if (connection === "unauthorized" || session.observation === "denied") why = "ibara couldn't confirm that you may use it. Open it to see why; its owner can check its Access tab."
  else if (ATTENTION_TASK_STATES.indexOf(String(task.state || "")) !== -1)
    why = (clip(task.title, 80) || "Its agent task") + (task.state === "waiting_for_human" ? " is waiting for you." : task.state === "interrupted" ? " was interrupted." : " is blocked.") + " Open its Activity tab to see why."
  else {
    var note = clip(session.frame_error, 160)
    why = (note ? note + (/[.!?…]$/.test(note) ? "" : ".") : "Something there needs a look.") + " Open it to see more."
  }
  return { heading: name + " needs attention", body: why, wake: false }
}

// The computers offline, locked or needing attention for PROBLEM_AFTER_MS or longer, oldest first, each
// with its words. One that needs a person has its own toast (needsYouView) and is left out;
// `hidden` maps a computer to the signature put away there (it shows again once that changes).
// → [{ computer_id, label, state, heading, body, wake, signature, since }]
function lastingProblems(computers, since, nowMs, hidden) {
  var began = since && typeof since === "object" ? since : {}, put = hidden && typeof hidden === "object" ? hidden : {}, out = []
  var rows = Array.isArray(computers) ? computers : []
  for (var i = 0; i < rows.length; i++) {
    var c = asObject(rows[i]), id = String(c.computer_id || ""), state = computerState(c)
    if (!id || (state !== "offline" && state !== "locked" && state !== "attention") || !began[id] || nowMs - began[id] < PROBLEM_AFTER_MS) continue
    if (asObject(c.needs_person).message && state !== "offline") continue
    var words = problemWords(c, clip(c.label, 128) || id)
    var signature = state + "|" + words.heading + "|" + words.body
    if (put[id] === signature) continue
    out.push({ computer_id: id, label: clip(c.label, 128), state: state, heading: words.heading, body: words.body, wake: words.wake, signature: signature, since: began[id] })
  }
  out.sort(function(a, b) { return a.since - b.since || (a.computer_id < b.computer_id ? -1 : 1) })
  return out
}

// ---- toasts. Every console message is a toast over the page's bottom-right corner, and nothing
// takes the page's room. Standing toasts last as long as their condition. In the order they are
// added when several start at once: approvals, login requests and agents' questions (oldest
// first) and requests to use this computer, which wait for an answer on every page; computers that
// need a person and computers offline or needing attention (every page: whatever makes the bar
// red); sites that rejected a shared login; the files on their way to the open computer; What's
// New; the card asking whether agents may use your logins; a theme run; While you were away; and,
// on the fleet until an agent has begun a task, the pointer to Connect an Agent. While ibara isn't
// running on this computer (`serviceStopped`), that is the one standing toast: nothing else can be
// answered or trusted until it runs again.
//   state: { route, computerId, approvals, logins, questions, pairRequests, needs: needsYouView,
//     problems: lastingProblems, loginRejected: [{ key }], drop: the open computer's drop or null,
//     whatsNew, loginSetup, themeRun, awayCount, awayHidden, connectPrompt, firstTaskDone,
//     connectHintDone, serviceStopped }
//   → [{ key, kind, ref, tone }], tone "approval", "request", "error" or "note".
function standingToasts(state) {
  var s = state || {}, out = []
  if (s.serviceStopped) return [{ key: "stopped", kind: "stopped", ref: "", tone: "error" }]
  var approvals = Array.isArray(s.approvals) ? s.approvals : []
  for (var i = 0; i < approvals.length; i++)
    if (approvals[i] && approvals[i].ref) out.push({ key: "approval:" + approvals[i].ref, kind: "approval", ref: String(approvals[i].ref), tone: "approval" })
  var logins = Array.isArray(s.logins) ? s.logins : []
  for (var l = 0; l < logins.length; l++)
    if (logins[l] && logins[l].ref) out.push({ key: "login:" + logins[l].ref, kind: "login", ref: String(logins[l].ref), tone: "approval" })
  var questions = Array.isArray(s.questions) ? s.questions : []
  for (var q = 0; q < questions.length; q++)
    if (questions[q] && questions[q].ref) out.push({ key: "question:" + questions[q].ref, kind: "question", ref: String(questions[q].ref), tone: "approval" })
  var requests = Array.isArray(s.pairRequests) ? s.pairRequests : []
  for (var j = 0; j < requests.length; j++)
    if (requests[j] && requests[j].request_id) out.push({ key: "pair:" + requests[j].request_id, kind: "pair", ref: String(requests[j].request_id), tone: "request" })
  var needs = Array.isArray(s.needs) ? s.needs : []
  for (var n = 0; n < needs.length; n++)
    // Machine repair remains on the fleet card and status badge, never a repeated overlay.
    if (needs[n] && needs[n].kind !== "repair" && needs[n].computer_id) out.push({ key: "need:" + needs[n].computer_id, kind: "need", ref: String(needs[n].computer_id), tone: "error" })
  var problems = Array.isArray(s.problems) ? s.problems : []
  for (var p = 0; p < problems.length; p++)
    if (problems[p] && problems[p].computer_id) out.push({ key: "problem:" + problems[p].computer_id, kind: "problem", ref: String(problems[p].computer_id), tone: "error" })
  var rejected = Array.isArray(s.loginRejected) ? s.loginRejected : []
  for (var r = 0; r < rejected.length; r++)
    if (rejected[r] && rejected[r].key) out.push({ key: "login-rejected:" + rejected[r].key, kind: "login-rejected", ref: String(rejected[r].key), tone: "error" })
  var id = String(s.computerId || "")
  if (s.route === "computer" && id) {
    var drop = s.drop
    if (drop && Array.isArray(drop.files)) {
      var failed = drop.state === "done" && Array.isArray(drop.failed) && drop.failed.length > 0
      out.push({ key: "drop:" + id, kind: "drop", ref: id, tone: failed ? "error" : "note" })
    }
  }
  if (s.whatsNew) out.push({ key: "news", kind: "news", ref: "", tone: "note" })
  if (s.loginSetup) out.push({ key: "login-setup", kind: "login-setup", ref: "", tone: "note" })
  var run = s.themeRun
  if (run) {
    var broken = run.state === "failed" || (Array.isArray(run.results) && run.results.some(function(r) { return r.state === "failed" }))
    out.push({ key: "theme", kind: "theme", ref: "", tone: broken ? "error" : "note" })
  }
  if (Number(s.awayCount || 0) > 0 && !s.awayHidden) out.push({ key: "away", kind: "away", ref: "", tone: "note" })
  if (s.route === "fleet" && s.connectPrompt && !s.firstTaskDone && !s.connectHintDone) out.push({ key: "connect", kind: "connect", ref: "", tone: "note" })
  return out
}

// Which toasts show while the stack is closed. Whatever waits for an answer (an approval, a login
// request, an agent's question, a request to use this computer, whether agents may use your
// logins, ibara not running here) always shows; the newest of the rest fill the room left of
// `maxShown`, and older ones wait behind "+N more".
//   kinds: each row's kind, oldest first → { shown: [bool per row], hidden: rows behind "+N more" }
var ANSWER_KINDS = ["approval", "login", "question", "pair", "login-setup", "stopped"]
function toastLayout(kinds, maxShown) {
  var list = Array.isArray(kinds) ? kinds : [], asks = 0, shown = [], hidden = 0
  for (var i = 0; i < list.length; i++) if (ANSWER_KINDS.indexOf(list[i]) !== -1) asks += 1
  var room = Math.max(0, Number(maxShown) - asks)
  for (var j = list.length - 1; j >= 0; j--) {
    var show = ANSWER_KINDS.indexOf(list[j]) !== -1 || room-- > 0
    shown[j] = show
    if (!show) hidden += 1
  }
  return { shown: shown, hidden: hidden }
}

// Files dropped on a computer, as their toast and the fleet card's strip say it: which one is
// going, how many have gone, and what didn't. `label` names the computer.
function dropMessage(drop, label) {
  if (!drop || !Array.isArray(drop.files)) return ""
  var total = drop.files.length
  var failed = Array.isArray(drop.failed) ? drop.failed : []
  var one = total === 1 ? drop.files[0].name : total + " files"
  if (drop.state === "starting") return "Getting ready to send " + one + "…"
  if (drop.state === "sending") return "Sending " + (total > 1 ? (drop.index + 1) + " of " + total + " · " : "") + drop.current
  if (!failed.length) return "Sent " + (drop.sent === 1 && total === 1 ? drop.files[0].name : drop.sent + (drop.sent === 1 ? " file" : " files")) + " to " + label + "."
  return (drop.sent ? "Sent " + drop.sent + " of " + total + ". " : "") + failed.map(function(f) { return (f.name ? f.name + " wasn't sent: " : "") + f.message }).join(" ")
}
