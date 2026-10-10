import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// This console's own settings (`settings`): notifications, where files from other computers are
// saved, and how often the fleet's pictures refresh. A change applies at once, and a message
// offers Undo. Each computer's own settings are on its Settings tab. Escape returns to the fleet.
// Under This Computer, Start without the disk password (`unattended-boot`) changes in a terminal,
// because it needs root and the disk password; Lock the screen at sign-in changes at once.
// Logins stays one summary row; Manage Logins opens its own page.
Item {
  id: root
  property var host: null
  property var service: null
  // The header is as tall as its tallest line plus a hairline margin, as on the fleet.
  readonly property real headerHeight: Math.ceil(Math.max(crumb.implicitHeight, backButton.implicitHeight)) + Style.space(6)
  readonly property Tokens tokens: Tokens {}
  readonly property var logins: service ? service.loginSettings : null
  readonly property var loginSections: [{ id: "logins", title: "Logins", settings: [{
    key: "login_summary", title: "Logins", type: "summary", value: "", "default": "", help: ""
  }] }]

  function focusDefault() { list.focusDefault() }
  function focusSearch() { list.focusSearch() }
  function reload() { if (service && visible) { service.loadConsoleSettings(); service.loadUnattendedBoot(); service.loadLoginSettings() } }
  function isThisComputer(key) { return key === "unattended_boot" || key === "lock_at_sign_in" }
  function setThisComputer(key, on) { return key === "lock_at_sign_in" ? service.setSignInLock(on) : service.setUnattendedBoot(on) }
  onVisibleChanged: reload()

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
      text: "Settings"
      font.pixelSize: Style.font.heading + Style.space(4)
      font.bold: true
    }
    // Which settings these are, beside the title: nothing sits between the header and the list.
    Copy {
      anchors.left: crumb.right
      anchors.leftMargin: Style.space(14)
      anchors.right: closeRule.left
      anchors.rightMargin: Style.space(14)
      anchors.baseline: crumb.baseline
      text: "For This Console"
      dimmed: true
      wrapMode: Text.NoWrap
      elide: Text.ElideRight
    }
    Rectangle {
      id: closeRule
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

  SettingsList {
    id: list
    y: root.headerHeight + Style.space(12)
    height: parent.height - y
    width: parent.width
    centered: true
    sections: root.service ? root.service.requestSettingsSections.concat(root.service.consoleSettings.map(function(section) { return Object.assign({}, section, { settings: section.settings.filter(function(s) { return s.key !== "approval_notifications" }) }) }), root.service.unattendedBootSections, root.loginSections) : []
    busy: root.service ? Object.assign({}, root.service.settingsBusyFor(""), root.service.busy["login-on"] ? { login_sharing: root.service.busy["login-on"] } : {}) : ({})
    errors: root.service ? root.service.settingsErrorsFor("") : ({})
    // Live Video: each computer that can't stream says why, and so does a desktop that can't play it.
    notes: root.service ? {
      live_video: [root.service.videoPlayerWorks ? "" : "Video can't play here: install qt6-multimedia and qt6-multimedia-ffmpeg.", root.service.videoReasons].filter(function(line) { return line !== "" }).join("\n")

    } : ({})
    extras: ({ login_summary: loginSummary })
    emptyText: root.service && root.service.consoleSettingsError ? root.service.consoleSettingsError : "Loading settings…"
    onChangeRequested: (key, value) => {
      if (!root.service) return
      if (root.isThisComputer(key)) root.setThisComputer(key, value === "true")
      else root.service.changeSetting("", key, value)
    }
    onResetRequested: key => {
      if (!root.service) return
      if (root.isThisComputer(key)) root.setThisComputer(key, false)
      else root.service.resetSetting("", key)
    }
    onSectionResetRequested: sectionId => {
      if (!root.service) return
      if (sectionId === "this_computer") root.service.resetThisComputer()
      else root.service.resetSection("", sectionId)
    }
  }

  Component {
    id: loginSummary
    Row {
      function focusDefault() { manageButton.forceActiveFocus() }
      spacing: Style.space(10)
      Copy { anchors.verticalCenter: parent.verticalCenter; text: "Logins"; font.bold: true }
      Copy { anchors.verticalCenter: parent.verticalCenter; text: root.logins ? root.logins.siteCount + " sites" : "Loading…"; dimmed: true }
      ActionButton { id: manageButton; label: "Manage Logins"; size: "small"; onClicked: if (root.host) root.host.showLogins() }
    }
  }
}
