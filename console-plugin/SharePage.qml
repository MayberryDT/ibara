import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Share This Computer: a friend uses this computer from theirs with a single-use invite code.
// The person here chooses what the friend can do (never administer) and for how long; the
// friend types the code next to this computer in their own Add Computer. Tailscale must let
// their computer reach this one, which this computer's page in the Tailscale admin console does
// with one Share. Every invite is listed with Revoke; revoking a used one ends that friend's
// access at once. Escape returns to the fleet.
Item {
  id: root
  property var host: null
  property var service: null
  // The header is as tall as its tallest line plus a hairline margin, as on the fleet.
  readonly property real headerHeight: Math.ceil(Math.max(crumb.implicitHeight, backButton.implicitHeight)) + Style.space(6)
  readonly property Tokens tokens: Tokens {}
  property string level: "watch"
  property string lasts: "day"
  readonly property var levels: [
    { key: "watch", label: "Watch", text: "They see this computer's screen. Nothing else." },
    { key: "use_with_approval", label: "Use with Approval", text: "They see the screen. Files, Join and agent tasks each ask you first." },
    { key: "take_control", label: "Join", text: "They see the screen, use files and take control. Agent tasks ask you first." }
  ]
  readonly property var durations: [
    { key: "hour", label: "1 Hour" }, { key: "day", label: "1 Day" }, { key: "week", label: "1 Week" }, { key: "never", label: "Until Revoked" }
  ]
  readonly property var invites: service ? service.invites : []
  readonly property var fresh: service ? service.newInvite : null
  readonly property string busy: service ? service.inviteBusy : ""
  readonly property string readError: service && service.readErrors["invites"] ? String(service.readErrors["invites"]) : ""

  function focusDefault() { (shareButton.visible ? shareButton : levelRepeater.itemAt(0) || backButton).forceActiveFocus() }
  function reload() { if (service && visible) { service.loadInvites(); service.loadTailnet() } }
  onVisibleChanged: { if (visible && service) service.newInvite = null; reload() }
  Timer { interval: 15000; repeat: true; running: root.visible && !!root.service; onTriggered: root.service.loadInvites() }

  function pad(n) { return (n < 10 ? "0" : "") + n }
  // `Sep 28, 14:00`, in this computer's time.
  function when(ms) {
    var d = new Date(ms)
    return ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][d.getMonth()] + " " + d.getDate() + ", " + pad(d.getHours()) + ":" + pad(d.getMinutes())
  }
  function levelText(key) {
    for (var i = 0; i < levels.length; i++) if (levels[i].key === key) return levels[i].text
    return ""
  }
  function friend(invite) { return invite.used.login + "'s " + invite.used.computer }
  function revoke(invite, button) {
    if (!service || busy) return
    if (!invite.used) { service.revokeInvite(invite.id); return }
    if (!host) return
    host.askConfirm({
      anchor: button,
      message: "End access for " + friend(invite) + " now? It can't reach this computer again unless you share it again.",
      confirmLabel: "Revoke and End Access",
      danger: true,
      run: function() { root.service.revokeInvite(invite.id) }
    })
  }

  component Heading: Copy { font.pixelSize: Style.font.heading; font.bold: true }
  component Group: Rectangle {
    default property alias content: groupColumn.data
    width: parent ? parent.width : 0
    height: groupColumn.implicitHeight + Style.space(36)
    radius: 0
    color: root.tokens.surface
    border.width: 1
    border.color: root.tokens.rule
    Column {
      id: groupColumn
      x: Style.space(18)
      y: Style.space(18)
      width: parent.width - Style.space(36)
      spacing: Style.space(12)
    }
  }

  // ---- top bar: Back returns to the fleet, as Escape does; close stays apart past a rule.
  Item {
    id: header
    width: parent.width
    height: root.headerHeight
    IbaraMark {
      id: mark
      width: Style.space(22)
      height: width
      anchors.left: parent.left
      anchors.verticalCenter: parent.verticalCenter
      color: Color.accent
    }
    ActionButton {
      id: backButton
      anchors.left: mark.right
      anchors.leftMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      glyph: "‹"
      label: "Fleet"
      Accessible.name: "Back to the fleet"
      tooltipText: "Back to the fleet (Escape)"
      onClicked: if (root.host) root.host.showFleet()
    }
    Copy {
      id: crumb
      anchors.left: backButton.right
      anchors.leftMargin: Style.space(14)
      anchors.verticalCenter: parent.verticalCenter
      text: "Share This Computer"
      font.pixelSize: Style.font.heading + Style.space(4)
      font.bold: true
    }
    Rectangle {
      anchors.right: closeButton.left
      anchors.rightMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: 1
      height: closeButton.height
      radius: 0
      color: root.tokens.rule
    }
    ActionButton {
      id: closeButton
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      label: "✕"
      role: "quiet"
      Accessible.name: "Close console"
      tooltipText: "Close the console"
      onClicked: if (root.host) root.host.requestClose()
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // Wide, the invite form on the left, and who has access and the codes not used yet on the
  // right; narrower, one column. Either way the page sits in the middle of the window.
  readonly property bool wide: width >= tokens.wideAt
  // The form's lines stay within about 90 characters (a group's padding is 18 on each side).
  readonly property real formWidth: Math.min(tokens.proseWidth + Style.space(36), wide ? Math.floor((width - tokens.columnGap) / 2) : width)
  readonly property real listWidth: wide ? Math.min(tokens.columnMax, width - tokens.columnGap - formWidth) : formWidth
  readonly property var usedInvites: invites.filter(function(invite) { return !!invite.used })
  readonly property var openInvites: invites.filter(function(invite) { return !invite.used })

  // One invite: what it lets the friend do, who used it or until when the code works, and Revoke.
  component InviteRow: Rectangle {
    id: inviteRow
    required property var modelData
    readonly property var invite: modelData
    width: parent ? parent.width : 0
    height: Math.max(inviteText.implicitHeight, revokeButton.height) + Style.space(20)
    radius: 0
    color: revokeButton.activeFocus ? Qt.alpha(Color.accent, 0.08) : "transparent"
    Accessible.role: Accessible.StaticText
    Accessible.name: title.text + ", " + detail.text
    Column {
      id: inviteText
      x: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: revokeButton.x - x - Style.space(12)
      spacing: Style.space(2)
      Copy {
        id: title
        width: parent.width
        text: StatusModel.shareLevelLabel(inviteRow.invite.level) + " · " + (inviteRow.invite.used ? "Used by " + root.friend(inviteRow.invite) : "Not used yet")
        font.bold: true
      }
      Copy {
        id: detail
        width: parent.width
        text: inviteRow.invite.used
          ? (inviteRow.invite.expires_at ? "Their access ends " + root.when(inviteRow.invite.expires_at) + "." : "Their access lasts until you revoke it.")
          : (inviteRow.invite.expires_at ? "The code works until " + root.when(inviteRow.invite.expires_at) + "." : "The code works until you revoke it.")
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
    }
    ActionButton {
      id: revokeButton
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      size: "small"
      role: "danger"
      label: root.busy === inviteRow.invite.id ? "Revoking…" : "Revoke"
      blocked: root.busy !== ""
      disabledReason: blocked ? "Wait for the last change to finish." : ""
      Accessible.name: "Revoke " + title.text
      onClicked: if (!blocked) root.revoke(inviteRow.invite, revokeButton)
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  Flickable {
    id: scroll
    y: root.headerHeight + Style.space(14)
    width: parent.width
    height: parent.height - y
    clip: true
    contentWidth: width
    contentHeight: content.height + Style.space(16)
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick
    Item {
      id: content
      width: root.wide ? root.formWidth + root.tokens.columnGap + root.listWidth : root.formWidth
      x: Math.max(0, Math.round((scroll.width - width) / 2))
      height: root.wide ? Math.max(form.implicitHeight, lists.implicitHeight) : lists.y + lists.implicitHeight
      Column {
        id: form
        width: root.formWidth
        spacing: Style.space(14)
        Copy {
          width: parent.width
          text: "Let a friend use this computer from theirs. They need ibara, and a code you give them here."
          font.pixelSize: Style.font.title
        }

        // ---- Tailscale: the friend's computer must be able to reach this one.
        Group {
          Copy {
            width: parent.width
            text: root.service && root.service.shareTailscale
              ? "Your friend's computer must be able to reach this one: choose Share in Tailscale, then Share and your friend's email."
              : root.readError ? root.readError : "Tailscale isn't running on this computer, so a friend's computer can't reach it."
          }
          ActionButton {
            id: shareButton
            visible: !!root.service && !!root.service.shareTailscale
            label: "Share in Tailscale"
            tooltipText: "Opens this computer's page in the Tailscale admin console"
            onClicked: Qt.openUrlExternally(root.service.shareTailscale.share_url)
          }
        }

        // ---- the invite: what they can do, for how long.
        Group {
          Heading { text: "What they can do" }
          Flow {
            width: parent.width
            spacing: Style.space(8)
            Repeater {
              id: levelRepeater
              model: root.levels
              delegate: ActionButton {
                required property var modelData
                label: modelData.label
                selected: root.level === modelData.key
                Accessible.name: modelData.label + (selected ? ", chosen" : "")
                onClicked: root.level = modelData.key
              }
            }
          }
          Copy { width: parent.width; text: root.levelText(root.level) }
          Copy {
            width: parent.width
            text: "A friend never administers this computer: its settings, power and who can use it stay yours."
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Heading { text: "For how long" }
          Flow {
            width: parent.width
            spacing: Style.space(8)
            Repeater {
              model: root.durations
              delegate: ActionButton {
                required property var modelData
                label: modelData.label
                selected: root.lasts === modelData.key
                Accessible.name: modelData.label + (selected ? ", chosen" : "")
                onClicked: root.lasts = modelData.key
              }
            }
          }
          ActionButton {
            role: "primary"
            label: root.busy === "create" ? "Making the Code…" : "Create Invite Code"
            blocked: root.busy !== ""
            disabledReason: blocked ? "Wait for the last change to finish." : ""
            onClicked: if (!blocked) root.service.createInvite(root.level, root.lasts)
          }

          // The new code, shown this once.
          Rectangle {
            visible: !!root.fresh && root.fresh.code !== ""
            width: parent.width
            height: Math.max(bigCode.implicitHeight, codeWords.implicitHeight) + Style.space(24)
            radius: 0
            color: Qt.alpha(Color.accent, 0.10)
            border.width: 1
            border.color: Color.accent
            Accessible.role: Accessible.StaticText
            Accessible.name: "Invite code " + (root.fresh ? root.fresh.code : "")
            Copy {
              id: bigCode
              x: Style.space(18)
              anchors.verticalCenter: parent.verticalCenter
              text: root.fresh ? root.fresh.code : ""
              font.family: "monospace"
              font.bold: true
              font.pixelSize: Style.font.heading * 2 + Style.space(6)
              font.letterSpacing: Style.space(2)
              Accessible.ignored: true
            }
            Column {
              id: codeWords
              anchors.left: bigCode.right
              anchors.leftMargin: Style.space(22)
              anchors.right: parent.right
              anchors.rightMargin: Style.space(18)
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(6)
              Copy {
                width: parent.width
                text: !root.fresh ? "" : "Give this code to your friend. It works once, and their access " + (root.fresh.expires_at ? "ends " + root.when(root.fresh.expires_at) : "lasts until you revoke it") + "."
                font.bold: true
              }
              Copy {
                width: parent.width
                text: "On their computer, they choose I Have an Invite Code next to this computer in Add Computer. ibara shows the code only now."
                dimmed: true
                font.pixelSize: Style.font.bodySmall
              }
              ActionButton {
                size: "small"
                label: "Copy Code"
                onClicked: { Quickshell.clipboardText = root.fresh.code; root.service.actionNotice = "Copied. Send it to your friend." }
              }
            }
          }
        }
      }

      // ---- who has access, and the codes not used yet, each with Revoke.
      Column {
        id: lists
        x: root.wide ? root.formWidth + root.tokens.columnGap : 0
        y: root.wide ? 0 : form.implicitHeight + Style.space(22)
        width: root.listWidth
        spacing: Style.space(14)
        Heading { text: "Who has access" }
        Copy {
          visible: root.usedInvites.length === 0
          width: parent.width
          text: root.readError ? "" : root.service && root.service.invitesLoaded ? "No one yet. A friend who adds this computer with a code shows here." : "Loading invites…"
          dimmed: true
        }
        Column {
          visible: root.usedInvites.length > 0
          width: parent.width
          Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
          Repeater { model: root.usedInvites; delegate: InviteRow {} }
        }
        Item { width: 1; height: Style.space(8) }
        Heading { text: "Codes not used yet" }
        Copy {
          visible: root.openInvites.length === 0
          width: parent.width
          text: root.readError ? "" : root.service && root.service.invitesLoaded ? "None." : "Loading invites…"
          dimmed: true
        }
        Column {
          visible: root.openInvites.length > 0
          width: parent.width
          Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
          Repeater { model: root.openInvites; delegate: InviteRow {} }
        }
      }
    }
  }
}
