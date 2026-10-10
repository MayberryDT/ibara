import QtQuick
import Quickshell
import Quickshell.Io
import "plugin" as Ibara

ShellRoot {
  id: root
  property bool standalone: true
  property var operatorService: service
  property bool requestsOpen: false
  function openRequests() { requestsOpen = true; service.loadAttention() }
  property bool loaded: false
  property bool settingsReadable: true
  property var savedSettings: ({ popup_desktop_notifications: true })
  function summon(id, payload) { consoleWindow.open(payload || "{}") }
  function hide(id) { consoleWindow.close() }
  function updateEntryInline(id, settings) {
    if (!settingsReadable) {
      service.actionError = "Existing Console settings could not be read. Repair the configuration file before saving changes."
      return false
    }
    savedSettings = Object.assign({}, settings)
    preferences.setText(JSON.stringify(savedSettings))
    return true
  }
  function firstPartyServiceFor(id) { return null }
  function launchTerminal(command) { Quickshell.execDetached(["x-terminal-emulator", "-e", "sh", "-c", command]) }
  FileView {
    id: preferences
    path: (Quickshell.env("XDG_CONFIG_HOME") || Quickshell.env("HOME") + "/.config") + "/ibara/console.json"
    atomicWrites: true
    printErrors: false
    function ready() {
      if (root.loaded) return
      root.loaded = true
      service.applySettings(root.savedSettings)
      consoleWindow.open("{}")
    }
    onLoaded: {
      try {
        var value = JSON.parse(text())
        if (value && typeof value === "object" && !Array.isArray(value)) root.savedSettings = value
      } catch (error) { root.settingsReadable = false; service.actionError = "Console settings could not be read. Existing settings were preserved." }
      ready()
    }
    onLoadFailed: function(error) {
      if (error !== FileViewError.FileNotFound) {
        root.settingsReadable = false
        service.actionError = "Console settings could not be read. Existing settings were preserved."
      }
      ready()
    }
    onSaveFailed: service.actionError = "Console settings could not be saved. Check access to your ibara configuration folder."
  }
  Ibara.Service { id: service; shell: root; settings: root.savedSettings }
  Ibara.Console { id: consoleWindow; shell: root; service: service }
  HandBackWindow { service: service }
  Loader {
    active: root.requestsOpen
    sourceComponent: Component {
      RequestsWindow {
        service: root.operatorService
        onCloseRequested: root.requestsOpen = false
        onRequestOpened: function(item) { root.requestsOpen = false; service.openRequest(item) }
      }
    }
  }
  IpcHandler {
    target: "ibara"
    function open(payload: string): void { root.summon("io.zet.ibara", payload) }
  }
}
