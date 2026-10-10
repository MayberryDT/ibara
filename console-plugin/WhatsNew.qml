import QtQuick
import qs.Commons

// What's New: once after ibara updated itself, a toast that says so; What's New opens the
// release's notes inside it. Dismissing it, or letting it leave, marks the notes seen, and they
// don't show again.
Toast {
  id: root
  property var service: null
  property bool notesOpen: false
  readonly property var news: service ? service.whatsNew : null
  readonly property var notes: news && Array.isArray(news.notes) ? news.notes : []
  readonly property string heading: news ? "ibara updated to " + String(news.version || "a new version") + "." : ""

  function closeDetails() { if (!notesOpen) return false; notesOpen = false; return true }

  life: 10000
  holding: notesOpen
  firstControl: notesButton.visible ? notesButton : null
  dismissName: "Dismiss the note about ibara's update"
  Accessible.name: heading

  Copy { width: parent.width; text: root.heading; font.pixelSize: Style.font.bodySmall }
  Repeater {
    model: root.notesOpen ? root.notes : []
    delegate: Copy {
      required property var modelData
      width: parent ? parent.width : 0
      text: "•  " + String(modelData)
      font.pixelSize: Style.font.bodySmall
    }
  }
  ActionButton {
    id: notesButton
    visible: root.notes.length > 0
    size: "small"
    role: "quiet"
    selected: root.notesOpen
    label: root.notesOpen ? "Hide What's New" : "What's New"
    Accessible.name: root.notesOpen ? "Hide what's new in this version" : "Show what's new in this version"
    onClicked: root.notesOpen = !root.notesOpen
  }
}
