import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel
Item {
  id: root
  property var service: null
  property var host: null
  readonly property var settings: service ? service.loginSettings : null
  readonly property Tokens tokens: Tokens {}
  implicitHeight: extras.implicitHeight
    Column {
      width: root.width
      id: extras
      spacing: Style.space(8)
      readonly property var s: root.settings
      readonly property var pick: root.service ? root.service.loginPick : null
      readonly property var choices: {
        var out = [], list = s ? s.browsers : []
        for (var i = 0; i < list.length; i++) for (var j = 0; j < list[i].profiles.length; j++)
          if (list[i].supported) out.push({ id: list[i].browser + "\n" + list[i].profiles[j], browser: list[i].browser, profile: list[i].profiles[j], label: list[i].name + " · " + list[i].profiles[j],
            selected: !!extras.pick && extras.pick.browser === list[i].browser && extras.pick.profile === list[i].profiles[j] })
        return out
      }
      readonly property string pickLabel: pick ? root.service.loginBrowserName + " · " + pick.profile : ""
      Item { width: 1; height: Style.space(2) }
      Row {
        spacing: Style.space(8)
        Copy {
          anchors.verticalCenter: parent.verticalCenter
          text: !extras.s ? "" : extras.s.enabled ? "Logins come from " + root.service.loginBrowserName + " · " + extras.s.profile + " on this computer."
            : extras.choices.length ? "Logins would come from" : "ibara found no Chromium, Google Chrome or Brave on this computer."
          color: root.tokens.textTint(extras.s && extras.s.enabled ? root.tokens.readyColor : root.tokens.pausedColor)
          font.pixelSize: Style.font.bodySmall
        }
        ActionMenu {
          visible: !!extras.s && !extras.s.enabled && extras.choices.length > 0
          anchors.verticalCenter: parent.verticalCenter
          size: "small"
          role: "secondary"
          label: extras.pickLabel + " ▾"
          accessibleName: "Share logins from: " + extras.pickLabel
          items: extras.choices
          onTriggered: id => { var parts = id.split("\n"); root.service.chooseLoginBrowser(parts[0], parts[1]) }
        }
      }
      Copy {
        visible: !!extras.s && extras.s.enabled
        width: parent.width
        text: "Turn sharing off before choosing another browser or profile."
        dimmed: true
        font.pixelSize: Style.font.bodySmall
      }
      // Never Share a site before any agent asks: Denied for All Computers.
      Row {
        spacing: Style.space(8)
        FieldInput {
          id: neverField
          width: Math.min(Style.space(260), extras.width - neverButton.width - Style.space(8))
          placeholder: "chase.com"
          accessibleName: "A site to never share"
          onAccepted: neverButton.clicked()
          onEdited: neverError.text = ""
        }
        ActionButton {
          id: neverButton
          anchors.verticalCenter: parent.verticalCenter
          label: "Never Share"
          role: "danger"
          size: "small"
          blocked: !extras.s || !extras.s.enabled || neverField.text.trim() === ""
          disabledReason: extras.s && !extras.s.enabled ? "Turn login sharing on to change its rules." : neverField.text.trim() === "" ? "Type a site, like chase.com." : ""
          tooltipText: disabledReason || "Deny this site on every computer, so no agent is ever given its login"
          onClicked: {
            if (blocked) return
            var site = StatusModel.loginSite(neverField.text)
            if (!site) { neverError.text = "Type one site, like chase.com, without https:// or a path."; return }
            if (root.service.setLoginRule("all", site, "deny")) { neverField.text = ""; neverError.text = "" }
          }
        }
      }
      Copy { id: neverError; visible: text !== ""; width: parent.width; color: root.tokens.textTint(root.tokens.attentionColor); font.pixelSize: Style.font.bodySmall }
    }
}
