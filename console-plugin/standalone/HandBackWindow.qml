import QtQuick
import Quickshell
import "plugin" as Ibara

// GNOME has no layer-shell. Keep a regular, reachable Hand Back window while
// the person holds control, independent of whether the Console is open.
FloatingWindow {
  id: root
  required property var service
  title: "ibara · Hand Back"
  visible: service.heldComputerIds.length > 0
  implicitWidth: 420
  implicitHeight: Math.max(70, rows.implicitHeight + 24)
  color: palette.window
  SystemPalette { id: palette }
  onVisibleChanged: if (!visible && service.heldComputerIds.length) Qt.callLater(function() { root.visible = true })
  Column {
    id: rows
    anchors.centerIn: parent
    width: parent.width - 24
    spacing: 8
    Repeater {
      model: root.service.heldComputerIds
      delegate: Row {
        required property string modelData
        width: rows.width
        spacing: 12
        Ibara.Copy {
          anchors.verticalCenter: parent.verticalCenter
          text: "Your Turn · " + root.service.computerLabelFor(modelData)
          width: parent.width - handback.width - 12
          elide: Text.ElideRight
        }
        Ibara.ActionButton {
          id: handback
          label: "Hand Back"
          role: "primary"
          blocked: root.service.mutating
          onClicked: if (!blocked) root.service.handBackFor(modelData)
        }
      }
    }
  }
}
