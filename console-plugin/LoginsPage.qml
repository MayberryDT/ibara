import QtQuick
import qs.Commons
import qs.Ui
Item {
  id: root
  property var host: null
  property var service: null
  readonly property Tokens tokens: Tokens {}
  readonly property real headerHeight: header.height
  readonly property var settings: service ? service.loginSettings : null
  readonly property string elsewhere: service ? service.loginSourceElsewhere(null) : ""
  property bool optionsOpen: false
  property string selectedComputer: "overview"
  function focusDefault() { if (selectedComputer === "overview") search.focusInput(); else scoped.focusDefault() }
  function focusSearch() { focusDefault() }
  function reload() { if (service && visible) { service.loadLoginSettings(); if (selectedComputer !== "overview") scoped.reload() } }
  onVisibleChanged: reload()
  Component.onCompleted: reload()
  Row {
    id: header
    spacing: Style.space(14)
    width: parent.width - closeButton.width - Style.space(12)
    ActionButton { glyph: "‹"; label: "Settings"; onClicked: if (root.host) root.host.showSettings() }
    Copy { text: "Logins"; font.bold: true; font.pixelSize: Style.font.heading + Style.space(4); anchors.verticalCenter: parent.verticalCenter }
    ActionMenu {
      label: root.selectedComputer === "overview" ? "All Sites ▾" : root.service.computerLabelFor(root.selectedComputer) + " ▾"
      accessibleName: "Login rules for computer"
      items: {
        var out = [{ id: "overview", label: "All Sites", selected: root.selectedComputer === "overview" }]
        if (root.settings) for (var id in root.settings.computers) out.push({ id: id, label: root.service.computerLabelFor(id), selected: root.selectedComputer === id })
        return out
      }
      onTriggered: id => root.selectedComputer = id
    }
    ActionButton { visible: root.selectedComputer === "overview"; label: "Refresh"; onClicked: { root.reload(); if (root.selectedComputer !== "overview") scoped.reload() } }
  }
  ActionButton {
    id: closeButton
    anchors.right: parent.right
    y: Math.round((header.height - height) / 2)
    label: "✕"
    role: "quiet"
    Accessible.name: "Close console"
    tooltipText: "Close the console"
    onClicked: if (root.host) root.host.requestClose()
  }
  Column {
    id: setup
    y: header.height + Style.space(10)
    width: parent.width
    visible: root.selectedComputer === "overview"
    spacing: Style.space(8)
    Row {
      spacing: Style.space(10)
      FieldInput { id: search; width: Style.space(340); placeholder: "Find a site"; accessibleName: "Find a login site" }
      ButtonGroup {
        id: filter
        options: [{value: "all", label: "All"}, {value: "allow", label: "Allowed"}, {value: "ask", label: "Ask First"}, {value: "deny", label: "Denied"}]
        value: "all"
        onChanged: value => filter.value = value
      }
    }
    Row {
      spacing: Style.space(10)
      Copy { text: "Login Sharing"; font.bold: true; anchors.verticalCenter: parent.verticalCenter }
      ActionButton {
        label: root.settings && root.settings.enabled ? "Turn Off" : "Turn On"
        blocked: !root.settings || root.settings.targetRole || !!root.service.busy["login-on"]
        disabledReason: root.settings && root.settings.targetRole ? "This computer runs agents. Share from your own computer." : ""
        onClicked: if (!blocked && root.host) { if (root.settings.enabled) root.host.confirmLoginsOff(this); else root.host.turnOnLogins(this) }
      }
      Copy { text: "Share only the site logins you allow."; dimmed: true; anchors.verticalCenter: parent.verticalCenter }
    }
    ActionButton { label: root.optionsOpen ? "Hide Sharing Options" : "Sharing Options"; size: "small"; role: "quiet"; onClicked: root.optionsOpen = !root.optionsOpen }
    LoginConfiguration { visible: root.optionsOpen; width: parent.width; service: root.service; host: root.host }
    Copy {
      visible: !root.settings || root.elsewhere !== "" || root.settings.targetRole || !root.settings.enabled || !root.settings.connected
      text: !root.settings ? "Loading login sharing…" : root.elsewhere ? "Logins come from " + root.elsewhere + ". Manage them on that computer." : root.settings.targetRole ? "Share logins from your own computer." : !root.settings.enabled ? "Login sharing is off." : "Open " + root.service.loginBrowserName + " to share logins."
      dimmed: true
    }

  }
  readonly property var sites: {
    var rows = settings ? settings.ruleRows : [], query = search.text.trim().toLowerCase()
    return rows.filter(function(row) { return row.site.indexOf(query) !== -1 && (filter.value === "all" || row.rule === filter.value) })
  }
  ListView {
    id: list
    visible: root.selectedComputer === "overview"
    y: setup.y + setup.height + Style.space(10)
    width: parent.width
    height: parent.height - y
    clip: true
    model: root.sites
    boundsBehavior: Flickable.StopAtBounds
    delegate: Item {
      required property var modelData
      width: list.width
      height: Style.space(40)
      Copy {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        width: Math.max(0, actions.x - Style.space(16))
        text: modelData.site
        wrapMode: Text.NoWrap
        elide: Text.ElideRight
      }
      Row {
        id: actions
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(12)
        Copy {
          anchors.verticalCenter: parent.verticalCenter
          width: Math.min(implicitWidth, Style.space(180))
          text: modelData.computer === "all" ? "All Computers" : root.service.computerLabelFor(modelData.computer)
          wrapMode: Text.NoWrap
          elide: Text.ElideRight
          dimmed: true
        }
        ActionButton {
          visible: modelData.computer !== "all"
          anchors.verticalCenter: parent.verticalCenter
          label: "Open"
          size: "small"
          role: "quiet"
          Accessible.name: "Manage " + modelData.site + " on " + modelData.computer
          onClicked: { root.selectedComputer = modelData.computer; scoped.findSite(modelData.site) }
        }
        Copy {
          visible: modelData.computer !== "all"
          anchors.verticalCenter: parent.verticalCenter
          text: modelData.rule === "allow" ? "Allowed" : modelData.rule === "deny" ? "Denied" : "Ask First"
          color: root.tokens.textTint(root.tokens.ruleColor(modelData.rule))
        }
        RuleMenu {
          visible: modelData.computer === "all"
          anchors.verticalCenter: parent.verticalCenter
          rule: modelData.rule
          blocked: !root.settings || !root.settings.enabled || (modelData.computer !== "all" && root.settings.allRules[modelData.site] === "deny") || !!root.service.busy["login-rule:" + modelData.computer + ":" + modelData.site]
          disabledReason: root.settings && root.settings.allRules[modelData.site] === "deny" && modelData.computer !== "all" ? "Denied for All Computers. Change that rule first." : "Turn login sharing on to change rules."
          accessibleName: modelData.site + " on " + modelData.computer
          onChosen: value => root.service.setLoginRule(modelData.computer, modelData.site, value)
        }
      }
      Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; color: root.tokens.rule }
    }
    Copy { visible: list.count === 0; text: !root.settings ? "Loading logins…" : search.text || filter.value !== "all" ? "No sites match." : "No login rules yet."; dimmed: true }
  }
  LoginsTab {
    id: scoped
    visible: root.selectedComputer !== "overview"
    y: header.height + Style.space(10)
    width: parent.width
    height: parent.height - y
    service: root.service
    host: root.host
    computerId: root.selectedComputer === "overview" ? "" : root.selectedComputer
    computer: root.service ? root.service.computers.find(function(c) { return (c.computer_id || c.id) === root.selectedComputer }) || null : null
  }
}
