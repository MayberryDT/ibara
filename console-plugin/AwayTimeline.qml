import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// While you were away, as a toast: how much happened on how many computers since this console
// last looked (`away`). Details opens each computer's latest events inside it, one a row, oldest first:
// the time, a colored tag for what happened (Started, Asked, Answered, Finished…), then its words,
// where a computer's name opens it. It stays until it is read or put away: Mark All Seen hides
// them until something new happens; dismissing the toast hides it until the console opens again.
Toast {
  id: root
  property var host: null
  property var service: null
  property bool open: false
  readonly property Tokens tokens: Tokens {}
  readonly property var computers: service && service.away ? service.away.computers : []
  readonly property double since: service && service.away ? Number(service.away.since || 0) : 0
  readonly property int eventCount: computers.reduce(function(sum, c) { return sum + (c.total || c.events.length) }, 0)
  // Each computer shows its latest events; earlier ones are counted.
  readonly property int shownEvents: 4
  readonly property real timeWidth: Style.space(64)
  readonly property real tagWidth: Style.space(68)
  readonly property real nameWidth: Style.space(130)
  readonly property string heading: "While you were away" + (since > 0 ? " (since " + when(since) + ")" : "") + ": " +
    (eventCount === 1 ? "1 thing happened" : eventCount + " things happened") + " on " + (computers.length === 1 ? "1 computer." : computers.length + " computers.")

  function when(ms) {
    if (!ms) return ""
    return tokens.changedLabel(new Date(ms).toISOString(), service ? service.nowMs : Date.now())
  }
  function closeDetails() { if (!open) return false; open = false; return true }

  holding: open
  firstControl: detailsButton
  dismissName: "Dismiss While you were away until the console opens again"
  Accessible.name: heading

  Copy { width: parent.width; text: root.heading; font.pixelSize: Style.font.bodySmall }
  Repeater {
    model: root.open ? root.computers : []
    delegate: Item {
      id: computerRow
      required property var modelData
      readonly property var events: modelData.events.slice(-root.shownEvents)
      readonly property int earlier: (modelData.total || modelData.events.length) - events.length
      width: parent ? parent.width : 0
      height: Math.max(nameButton.implicitHeight, eventColumn.implicitHeight)
      ActionButton {
        id: nameButton
        width: Math.min(implicitWidth, root.nameWidth)
        size: "small"
        role: "quiet"
        label: String(computerRow.modelData.label || (root.service ? root.service.computerLabelFor(computerRow.modelData.computer_id) : ""))
        Accessible.name: "Open " + label
        tooltipText: "Open " + label
        onClicked: if (root.host) root.host.showComputer(String(computerRow.modelData.computer_id))
      }
      Column {
        id: eventColumn
        x: root.nameWidth + Style.space(8)
        width: parent.width - x
        spacing: Style.space(3)
        Copy {
          visible: computerRow.earlier > 0
          text: "+" + computerRow.earlier + " earlier"
          color: root.tokens.faint
          font.pixelSize: Style.font.caption
        }
        Repeater {
          model: computerRow.events
          delegate: Item {
            id: eventRow
            required property var modelData
            readonly property var tag: StatusModel.awayEventTag(modelData.kind, modelData.summary)
            width: eventColumn.width
            height: summaryText.implicitHeight
            Accessible.role: Accessible.StaticText
            Accessible.name: root.when(modelData.at) + ", " + tag.label + ": " + modelData.summary
            Copy {
              id: timeText
              width: root.timeWidth
              text: root.when(eventRow.modelData.at)
              color: root.tokens.dim
              font.pixelSize: Style.font.caption
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            Copy {
              x: root.timeWidth + Style.space(6)
              width: root.tagWidth
              text: eventRow.tag.label.toUpperCase()
              color: root.tokens.textTint(root.tokens.stateColor(eventRow.tag.tone))
              font.pixelSize: Style.font.caption
              font.bold: true
              wrapMode: Text.NoWrap
            }
            Copy {
              id: summaryText
              x: root.timeWidth + root.tagWidth + Style.space(12)
              width: parent.width - x
              text: eventRow.modelData.summary
              color: eventRow.tag.tone === "attention" ? root.tokens.textTint(root.tokens.attentionColor) : root.tokens.foreground
              font.pixelSize: Style.font.bodySmall
            }
          }
        }
      }
    }
  }
  Row {
    spacing: Style.space(8)
    ActionButton {
      id: detailsButton
      size: "small"
      role: "quiet"
      selected: root.open
      label: root.open ? "Hide Details" : "Details"
      Accessible.name: root.open ? "Hide what happened on each computer" : "Show what happened on each computer"
      onClicked: root.open = !root.open
    }
    ActionButton {
      size: "small"
      label: root.service && root.service.busy["away-seen"] ? "Marking…" : "Mark All Seen"
      blocked: !!root.service && !!root.service.busy["away-seen"]
      tooltipText: "Hide these until something new happens"
      onClicked: if (!blocked && root.service) root.service.markAwaySeen()
    }
  }
}
