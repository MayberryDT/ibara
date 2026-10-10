import QtQuick
import qs.Commons

// Apply Theme to Fleet, as a toast: while it goes, then this computer's theme, how many computers
// have it and which didn't take it. When every computer took it, the toast leaves by itself;
// otherwise it stays until dismissed, with Apply Again and Details, which lists what happened on
// each computer (applied, offline or failed, with why).
Toast {
  id: root
  property var service: null
  property bool open: false
  readonly property Tokens tokens: Tokens {}
  readonly property var run: service ? service.themeRun : null
  readonly property var results: run ? run.results : []
  readonly property var applied: results.filter(function(r) { return r.state === "applied" })
  readonly property var problems: results.filter(function(r) { return r.state !== "applied" })
  readonly property bool applying: !!run && run.state === "applying"
  readonly property bool failed: !!run && (run.state === "failed" || problems.some(function(r) { return r.state === "failed" }))
  readonly property bool clean: !!run && run.state === "done" && problems.length === 0
  readonly property bool canApplyAgain: !!run && !applying && (run.state === "failed" || problems.length > 0)
  readonly property bool hasDetails: results.length > 0 && !clean
  readonly property string heading: {
    if (!run) return ""
    if (applying) return "Applying this computer's theme to your fleet…"
    if (run.state === "failed") return "The theme wasn't applied." + (run.message ? " " + run.message : "")
    var theme = run.theme || "This computer's theme"
    if (!results.length) return theme + ": there are no other computers to apply it to."
    if (applied.length === results.length) return theme + " is on all " + results.length + " computers."
    // Up to three names; Details says why for each.
    var names = problems.slice(0, 3).map(function(r) { return String(r.label) })
    var more = problems.length - names.length
    var list = more > 0 ? names.join(", ") + " and " + more + " more"
      : names.length > 1 ? names.slice(0, -1).join(", ") + " and " + names[names.length - 1] : names[0]
    return theme + " is on " + applied.length + " of " + results.length + " computers. Not on " + list + "."
  }

  function closeDetails() { if (!open) return false; open = false; return true }

  edge: failed ? Color.urgent : Color.accent
  tinted: failed
  dismissable: !applying
  life: clean ? 5000 : 0
  holding: open
  firstControl: canApplyAgain ? applyAgain : hasDetails ? detailsButton : null
  dismissName: "Dismiss the theme results"
  Accessible.name: heading

  Copy { width: parent.width; text: root.heading; color: root.tokens.textTint(root.failed ? root.tokens.attentionColor : root.applying ? root.tokens.workingColor : root.clean ? root.tokens.readyColor : root.tokens.pausedColor); font.pixelSize: Style.font.bodySmall }
  // Every computer and what happened there, the ones that need a look first.
  Flow {
    visible: root.open && root.results.length > 0
    width: parent.width
    spacing: Style.space(12)
    Repeater {
      model: root.open ? root.problems.concat(root.applied) : []
      delegate: Row {
        required property var modelData
        spacing: Style.space(6)
        StateMarker {
          tokens: root.tokens
          fleetState: modelData.state === "applied" ? "ready" : modelData.state === "offline" ? "offline" : "attention"
          anchors.verticalCenter: parent.verticalCenter
        }
        Copy {
          textFormat: Text.StyledText
          text: root.tokens.ink(modelData.label + " · ", root.tokens.dim) + root.tokens.ink(modelData.state === "applied" ? "applied" : modelData.state === "offline" ? "offline" : "failed", root.tokens.textTint(root.tokens.statusColor(modelData.state)))
          font.pixelSize: Style.font.bodySmall
        }
      }
    }
  }
  Repeater {
    model: root.open ? root.problems.filter(function(r) { return r.message }) : []
    delegate: Copy {
      required property var modelData
      width: parent ? parent.width : 0
      textFormat: Text.StyledText
      text: root.tokens.labeled(modelData.label + ": " + modelData.message, root.tokens.textTint(root.tokens.statusColor(modelData.state)))
      font.pixelSize: Style.font.bodySmall
    }
  }
  Row {
    // From the run itself, not the buttons' own visible: a hidden row reports its buttons hidden too.
    visible: root.canApplyAgain || root.hasDetails
    spacing: Style.space(8)
    ActionButton {
      id: applyAgain
      visible: root.canApplyAgain
      size: "small"
      label: "Apply Again"
      tooltipText: "Apply this computer's theme to every computer that is on"
      onClicked: if (root.service) root.service.applyThemeToFleet()
    }
    ActionButton {
      id: detailsButton
      visible: root.hasDetails
      size: "small"
      role: "quiet"
      selected: root.open
      label: root.open ? "Hide Details" : "Details"
      Accessible.name: root.open ? "Hide what happened on each computer" : "Show what happened on each computer"
      onClicked: root.open = !root.open
    }
  }
}
