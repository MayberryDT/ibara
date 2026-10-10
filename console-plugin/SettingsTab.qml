import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// The open computer's own settings (`operator-settings`), kept on that computer: its name, its
// screen when no monitor is plugged in, how often it sends pictures, recovery and waking. A change
// applies at once, and a message offers Undo.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  readonly property Tokens tokens: Tokens {}

  function reload() { if (service && visible && computerId) service.loadComputerSettings() }
  function focusDefault() { list.focusDefault() }
  function focusSearch() { list.focusSearch() }
  onVisibleChanged: reload()
  onComputerIdChanged: reload()

  // When these settings were read, on the search field's line at the right: nothing sits
  // between the tabs and the settings.
  DataAge {
    id: age
    z: 1
    anchors.right: parent.right
    y: Math.round((list.searchHeight - height) / 2)
    at: root.service && root.service.scopedReadAt["computer-settings"] || 0
    refreshing: !!root.service && root.service.readPending("computer-settings")
    nowMs: root.service ? root.service.nowMs : Date.now()
  }
  SettingsList {
    id: list
    anchors.fill: parent
    searchInset: age.visible ? age.implicitWidth + Style.space(16) : 0
    sections: root.service ? root.service.computerSettings : []
    busy: root.service ? root.service.settingsBusyFor(root.computerId) : ({})
    errors: root.service ? root.service.settingsErrorsFor(root.computerId) : ({})
    emptyColor: root.tokens.textTint(root.service && root.service.readErrors["computer-settings"] ? root.tokens.attentionColor : root.tokens.workingColor)
    emptyText: root.service && root.service.readErrors["computer-settings"] ? StatusModel.clip(root.service.readErrors["computer-settings"], 300)
      : "Loading " + root.tokens.label(root.computer) + "'s settings…"
    onChangeRequested: (key, value) => { if (root.service) root.service.changeSetting(root.computerId, key, value) }
    onResetRequested: key => { if (root.service) root.service.resetSetting(root.computerId, key) }
    onSectionResetRequested: sectionId => { if (root.service) root.service.resetSection(root.computerId, sectionId) }
  }
}
