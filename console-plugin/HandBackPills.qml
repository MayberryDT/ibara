import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Commons

// Service-owned, so hiding the console never hides Hand Back.
Item {
  id: root
  property var service: null
  readonly property var held: service ? service.heldComputerIds : []
  function screenFor(id) {
    var name = service ? service.viewerScreens[id] : ""
    return Quickshell.screens.find(function(screen) { return screen.name === name }) || Quickshell.screens[0] || null
  }
  Variants {
    model: root.held
    delegate: PanelWindow {
      id: pill
      required property string modelData
      readonly property var question: root.service.questions.find(function(q) { return q.computer_id === pill.modelData }) || null
      readonly property var options: question && Array.isArray(question.options) ? question.options : []
      readonly property bool canFinish: !!question && (options.length === 0 || (options.length === 1 && /^(done|yes|ok|okay|ready|solved|finished|continue|proceed|confirm|completed)(\b|[.!])/i.test(String(options[0]).trim())))
      readonly property bool answering: !!question && !!root.service.busy["answer:" + question.ref]
      readonly property int stackIndex: root.held.filter(function(id) { return root.screenFor(id) === pill.screen }).indexOf(modelData)
      screen: root.screenFor(modelData)
      visible: root.service.holdsControlOn(modelData)
      anchors.top: true
      margins.top: Style.space(10) + stackIndex * Style.space(46)
      exclusionMode: ExclusionMode.Ignore
      WlrLayershell.layer: WlrLayer.Overlay
      WlrLayershell.namespace: "ibara-handback"
      WlrLayershell.keyboardFocus: WlrKeyboardFocus.None
      color: "transparent"
      implicitWidth: content.implicitWidth + Style.space(16)
      implicitHeight: Style.space(38)
      Rectangle {
        anchors.fill: parent
        radius: height / 2
        color: Color.popups.background
        border.color: Color.accent
        border.width: Style.space(1)
        Row {
          id: content
          anchors.centerIn: parent
          spacing: Style.space(10)
          Copy {
            anchors.verticalCenter: parent.verticalCenter
            text: "Your Turn · " + root.service.computerLabelFor(pill.modelData)
            width: Math.min(implicitWidth, Style.space(180))
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
            font.pixelSize: Style.font.bodySmall
          }
          ActionButton {
            anchors.verticalCenter: parent.verticalCenter
            label: pill.answering ? "Sending…" : pill.canFinish ? "Done & Hand Back" : "Hand Back"
            role: "primary"
            size: "small"
            focusable: false
            blocked: root.service.mutating || pill.answering || (pill.canFinish && pill.question.unreachable === true)
            tooltipText: "Ctrl+Alt+Shift+H in the viewer · Hand Back"
            onClicked: if (!blocked) {
              if (pill.canFinish) root.service.answerAndHandBack(pill.question.ref, pill.options.length ? String(pill.options[0]) : "Done")
              else root.service.handBackFor(pill.modelData)
            }
          }
          ActionButton {
            visible: !!pill.question && !pill.canFinish
            anchors.verticalCenter: parent.verticalCenter
            label: "Open"
            size: "small"
            focusable: false
            onClicked: root.service.openRequest(pill.question)
          }
        }
      }
    }
  }
}
