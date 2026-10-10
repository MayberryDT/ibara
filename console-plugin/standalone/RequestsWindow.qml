import QtQuick
import Quickshell
import "plugin" as Ibara

// A regular GNOME window: dismissed notifications never hide pending work.
FloatingWindow {
  id: root
  required property var service
  signal closeRequested()
  signal requestOpened(var item)
  title: "ibara · Requests"
  visible: true
  implicitWidth: 520
  implicitHeight: 420
  color: palette.window
  SystemPalette { id: palette }
  onVisibleChanged: if (!visible) closeRequested()
  FocusScope {
    anchors.fill: parent
    anchors.margins: 16
    focus: true
    Keys.onEscapePressed: root.closeRequested()
    Column {
      anchors.fill: parent
      spacing: 12
      Row {
        width: parent.width
        Ibara.Copy { text: "Pending requests"; width: parent.width - closeButton.width - 12 }
        Ibara.ActionButton { id: closeButton; label: "Close"; onClicked: root.closeRequested() }
      }
      Ibara.Copy { visible: !root.service.requestItems.length; text: "No pending requests." }
      Flickable {
        width: parent.width
        height: parent.height - y
        contentHeight: requests.implicitHeight
        clip: true
        Column {
          id: requests
          width: parent.width
          spacing: 8
          Repeater {
            model: root.service.requestItems.map(function(item) { return item.ref })
            delegate: Ibara.ActionButton {
              required property string modelData
              readonly property var request: root.service.requestItems.filter(function(item) { return item.ref === modelData })[0] || null
              width: requests.width
              label: request ? (request.label || root.service.computerLabelFor(request.computer_id)) + ": " + request.summary : ""
              Accessible.name: "Open pending request: " + label
              onClicked: if (request) root.requestOpened(request)
            }
          }
        }
      }
    }
  }
}
