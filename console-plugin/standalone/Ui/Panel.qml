import QtQuick

Item {
  id: root
  property string moduleName: ""
  property bool manageIpc: false
  property bool opened: false
  readonly property QtObject controller: QtObject {
    function show() { root.opened = true }
    function hide() { root.opened = false }
  }
}
