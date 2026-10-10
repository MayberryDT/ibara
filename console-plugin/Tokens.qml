import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel
import "Palette.js" as Palette

// Shared presentation vocabulary: colors, fleet-state words and the per-computer
// lines every surface shows. Presentation only; Service owns the records. Every color
// comes from the Omarchy theme: Color, plus the theme's own palette (ThemePalette).
QtObject {
  id: root
  readonly property color foreground: Color.popups.text
  readonly property color background: Color.popups.background
  readonly property color accent: Color.accent
  readonly property color dim: Qt.alpha(foreground, 0.64)
  readonly property color faint: Qt.alpha(foreground, 0.44)
  readonly property color rule: Qt.alpha(foreground, 0.14)
  readonly property color surface: Qt.alpha(foreground, 0.035)
  readonly property color raised: Qt.alpha(foreground, 0.06)
  readonly property QtObject palette: Palette.get(Qt.resolvedUrl("ThemePalette.qml"))
  readonly property color attentionColor: Color.urgent
  readonly property color offlineColor: Color.muted
  readonly property color humanColor: palette ? palette.human : Color.accent
  readonly property color workingColor: palette ? palette.working : Color.accent
  readonly property color pausedColor: palette ? palette.yellow : Color.accent
  readonly property color readyColor: palette ? palette.green : Color.accent
  readonly property string fontFamily: Style.font.family
  // How long a matched pairing code shows on Add Computer before the console goes to the fleet.
  readonly property int matchMs: 1600
  readonly property var stateOrder: ["attention", "offline", "locked", "human", "working", "paused", "ready", "connecting"]

  // Page layout for the form pages and a computer's tabs: from `wideAt` wide, two columns
  // `columnGap` apart; narrower, one column at most `columnMax` wide, centered. A paragraph never
  // runs past `proseWidth`, about 90 characters of body text.
  readonly property real wideAt: Style.space(1400)
  readonly property real columnGap: Style.space(32)
  readonly property real columnMax: Style.space(900)
  readonly property FontMetrics bodyMetrics: FontMetrics { font.family: Style.font.family; font.pixelSize: Style.font.body }
  readonly property real proseWidth: Math.ceil(bodyMetrics.averageCharacterWidth * 90)

  // The selected Activity timeline's tint keeps theme colors readable as text.
  // Use raw colors for bands/marks; use this for colored words on every surface.
  function textTint(color) { return Qt.tint(color, Qt.alpha(foreground, 0.38)) }
  function ruleColor(rule) { return rule === "allow" ? readyColor : rule === "ask" ? pausedColor : attentionColor }
  function statusColor(state) {
    if (["ready", "completed", "complete", "finished", "done", "applied", "verified", "satisfied", "worked", "approved", "allow", "settled", "light", "fresh", "applicable"].indexOf(state) >= 0) return readyColor
    if (["working", "active", "running", "applying", "loading", "saving", "sharing"].indexOf(state) >= 0) return workingColor
    if (["attention", "failed", "interrupted", "cancelled", "canceled", "deny", "denied", "site_rejected", "heavy", "contradicted", "unsettled"].indexOf(state) >= 0) return attentionColor
    if (["paused", "partial", "candidate", "pending", "ask", "unknown", "expired", "moderate", "unsatisfied", "quarantined", "stale", "not_applicable"].indexOf(state) >= 0) return pausedColor
    if (["offline", "locked", "connecting", "off", "withdrawn"].indexOf(state) >= 0) return dim
    return foreground
  }
  function effectColor(effect) { return effect === "destructive" ? attentionColor : effect === "send" || effect === "spend" ? pausedColor : accent }
  function escapeText(value) {
    return String(value || "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;")
  }
  // Rich text is composed only from escaped values, never from computer-supplied markup.
  function ink(value, color) { return '<font color="' + color + '">' + escapeText(value) + '</font>' }
  function labeled(line, valueColor) {
    var text = String(line || ""), at = text.indexOf(": ")
    return at < 0 ? ink(text, valueColor || foreground) : ink(text.slice(0, at + 1), dim) + " " + ink(text.slice(at + 2), valueColor || foreground)
  }
  function detailMarkup(label, value, effect) {
    // Approval values are already structured plain words. Only their effect phrases
    // receive semantic colors; the action itself uses the request's effect.
    if (label !== "What") return ink(value, detailColor(label, effect))
    var text = String(value || ""), parts = [], last = 0
    var words = /\b(sends something|spends money|deletes or overwrites something|sends|deletes)\b/gi, match
    while ((match = words.exec(text)) !== null) {
      parts.push(ink(text.slice(last, match.index), textTint(effectColor(effect))))
      var kind = /^send/i.test(match[0]) ? "send" : /^spend/i.test(match[0]) ? "spend" : "destructive"
      parts.push(ink(match[0], textTint(effectColor(kind))))
      last = words.lastIndex
    }
    return parts.join("") + ink(text.slice(last), textTint(effectColor(effect)))
  }
  function detailColor(label, effect) {
    if (label === "What" || label === "Effect") return textTint(effectColor(effect))
    if (/^(Page|Revision|Task ref|Request|Fingerprint|Address)$/i.test(label)) return faint
    return foreground
  }

  function stateOf(computer) {
    if (!computer) return "offline"
    if (computer.fleet_state) return String(computer.fleet_state)
    return StatusModel.fleetState(computer)
  }
  function stateColor(state) {
    if (state === "attention") return attentionColor
    if (state === "offline" || state === "locked" || state === "connecting") return offlineColor
    if (state === "human") return humanColor
    if (state === "working") return workingColor
    if (state === "paused") return pausedColor
    return readyColor
  }
  // "human" covers any person in control; only your own control reads "You have control".
  function stateLabel(state, computer) {
    if (state === "human" && computer && actor(computer) !== "you") return "A person has control"
    return StatusModel.fleetStateLabel(state)
  }
  function stateRank(state) {
    var index = stateOrder.indexOf(state)
    return index < 0 ? stateOrder.length : index
  }
  // Needs attention, In use or Ready: the wall filters and the computer list groups.
  function groupOf(state) {
    if (state === "attention" || state === "offline" || state === "locked") return "attention"
    if (state === "human" || state === "working" || state === "paused") return "use"
    return "ready"
  }
  function groupLabel(group) {
    return group === "attention" ? "Needs attention" : group === "use" ? "In use" : "Ready"
  }
  // The fleet's counts in one parallel label form and a fixed order, in Title Case like the wall's
  // filters: Needs Attention (offline and locked included), In Use by You, In Use by Others (only while
  // someone on another computer holds one), In Use by Agents, Agents Paused, Ready for Work. A
  // computer still connecting is not counted.
  function fleetCountRows(computers) {
    var n = { attention: 0, you: 0, others: 0, working: 0, paused: 0, ready: 0 }
    var list = Array.isArray(computers) ? computers : []
    for (var i = 0; i < list.length; i++) {
      if (!list[i]) continue
      var state = stateOf(list[i])
      if (state === "human") n[actor(list[i]) === "you" ? "you" : "others"] += 1
      else if (state === "offline" || state === "locked") n.attention += 1
      else if (n[state] !== undefined) n[state] += 1
    }
    var rows = [
      { state: "attention", count: n.attention, label: "Needs Attention" },
      { state: "human", count: n.you, label: "In Use by You" },
      { state: "human", count: n.others, label: "In Use by Others" },
      { state: "working", count: n.working, label: "In Use by Agents" },
      { state: "paused", count: n.paused, label: "Agents Paused" },
      { state: "ready", count: n.ready, label: "Ready for Work" }
    ]
    return rows.filter(function(row) { return row.label !== "In Use by Others" || row.count > 0 })
  }
  // Primary text is the directory label; a raw computer id never stands in for it.
  function label(computer) {
    return computer && computer.label ? String(computer.label) : "Unnamed computer"
  }
  function actor(computer) {
    if (!computer) return ""
    return computer.actor !== undefined ? String(computer.actor || "") : StatusModel.fleetActor(computer)
  }
  function activity(computer, nowMs) {
    if (!computer) return ""
    return StatusModel.activityLine(computer, nowMs)
  }
  function activityMarkup(computer, nowMs) {
    var parts = activity(computer, nowMs).split(" · "), state = stateOf(computer)
    var task = computer && computer.active_task
    var title = task ? StatusModel.clip(task.title, 80) : ""
    var hasTitle = !!title && parts[0] === title
    return ink(parts[0], hasTitle ? foreground : textTint(stateColor(state))) + (parts.length > 1
      ? ink(" · ", dim) + ink(parts.slice(1).join(" · "), hasTitle && state === "attention" ? textTint(attentionColor) : dim) : "")
  }
  function technicalMarkup(value) {
    return escapeText(value).replace(/\b(?:frame|task|computer|job|op|att)_[A-Za-z0-9_-]+\b/g, function(ref) { return ink(ref, faint) })
  }
  function stepMarkup(receipt, showTime) {
    // The fleet's last_step is an already worded summary, not a receipt.
    // Color its existing action word without adding a second verb.
    if (receipt && !receipt.tool) {
      var said = StatusModel.clip(String(receipt.summary || ""), 160), space = said.indexOf(" ")
      return space < 0 ? ink(said, textTint(workingColor)) : ink(said.slice(0, space), textTint(workingColor)) + " " + technicalMarkup(said.slice(space + 1))
    }
    var step = StatusModel.taskStep(receipt), ms = StatusModel.timeMs(step.at)
    var clock = showTime && isFinite(ms) ? clockLabel(new Date(ms).toISOString()) : ""
    var kindColor = step.kind === "task" ? humanColor : step.kind === "exec" ? workingColor : accent
    var effect = receipt && receipt.effect
    var verbColor = ["send", "spend", "destructive"].indexOf(effect) >= 0 ? effectColor(effect) : kindColor
    return (clock ? ink(clock + "  ", dim) : "") + ink(step.verb, textTint(verbColor)) +
      (step.tone !== "ready" ? ink(" · " + step.result, textTint(stateColor(step.tone))) : "") + " " +
      (step.kind === "exec" ? ink(StatusModel.clip(step.what, 160), foreground) : technicalMarkup(StatusModel.clip(step.what, 160))) + (step.expect ? ink(" · " + StatusModel.clip(step.expect, 160), step.tone === "paused" ? textTint(pausedColor) : dim) : "")
  }
  function frameSource(computer, denied) {
    if (!computer || denied || computer.connection === "unauthorized" || computer.trust_state !== "verified") return ""
    var url = computer.frame && computer.frame.url
    return /^file:\/\/\/[^\0]*\/ibara\/previews\/[A-Za-z0-9_.:-]+-(tile|selected)-[0-9]+\.ppm$/.test(String(url || "")) ? String(url) : ""
  }
  // A tile-quality frame only. While the Screen tab shows, the open computer's frames are the
  // selected quality; each carries the computer's last tile frame for its small thumbnail, so
  // the thumbnail never decodes the large picture.
  function tileFrameSource(computer, denied) {
    var url = frameSource(computer, denied)
    if (!url) return ""
    if (/-tile-[0-9]+\.ppm$/.test(url)) return url
    var tile = frameSource(Object.assign({}, computer, { frame: computer.frame.tile || null }), denied)
    return /-tile-[0-9]+\.ppm$/.test(tile) ? tile : ""
  }
  function dropFrame(computer) {
    return !computer || !computer.frame || computer.connection === "unauthorized"
  }
  function sortComputers(rows, mode) {
    var list = rows.slice()
    list.sort(function(a, b) {
      if (mode !== "name") {
        var rank = stateRank(stateOf(a)) - stateRank(stateOf(b))
        if (rank !== 0) return rank
      }
      return label(a).localeCompare(label(b))
    })
    return list
  }
  function sentence(value) {
    var text = String(value || "").replace(/[_-]/g, " ").trim()
    return text ? text.charAt(0).toUpperCase() + text.slice(1) : ""
  }
  function clockLabel(iso) {
    var ms = StatusModel.isoMs(String(iso || ""))
    if (!isFinite(ms)) return ""
    var d = new Date(ms)
    return (d.getHours() < 10 ? "0" : "") + d.getHours() + ":" + (d.getMinutes() < 10 ? "0" : "") + d.getMinutes()
  }
  // Short "changed" column: time today, "Yesterday", weekday within a week, else the date.
  function changedLabel(iso, nowMs) {
    var ms = StatusModel.isoMs(String(iso || ""))
    if (!isFinite(ms)) return ""
    var then = new Date(ms), now = new Date(nowMs || Date.now())
    var dayMs = 86400000
    var startToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()
    if (ms >= startToday) return clockLabel(iso)
    if (ms >= startToday - dayMs) return "Yesterday"
    if (ms >= startToday - 6 * dayMs) return ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][then.getDay()]
    return then.getFullYear() + "-" + (then.getMonth() < 9 ? "0" : "") + (then.getMonth() + 1) + "-" + (then.getDate() < 10 ? "0" : "") + then.getDate()
  }
}
