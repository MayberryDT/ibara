import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Commons

// Owned by the service, so requests can be answered without opening the console.
// One stack on the first screen; no keyboard focus or pointer placement.
PanelWindow {
  id: root
  property var service: null
  readonly property var requests: service ? service.popupRequests : []
  visible: requests.length > 0
  screen: Quickshell.screens.length ? Quickshell.screens[0] : null
  anchors { top: true; right: true }
  margins { top: Style.space(48); right: Style.space(12) }
  exclusionMode: ExclusionMode.Ignore
  WlrLayershell.layer: WlrLayer.Overlay
  WlrLayershell.namespace: "ibara-requests"
  WlrLayershell.keyboardFocus: WlrKeyboardFocus.None
  color: "transparent"
  implicitWidth: Math.min(Style.space(520), screen ? screen.width - Style.space(24) : Style.space(520))
  implicitHeight: Math.min(stack.implicitHeight, screen ? screen.height - Style.space(80) : Style.space(600))

  Flickable {
    anchors.fill: parent
    clip: true
    contentWidth: width
    contentHeight: stack.implicitHeight
    boundsBehavior: Flickable.StopAtBounds
   Column {
    id: stack
    width: parent.width
    spacing: Style.space(8)
    Repeater {
      // Repeater converts object models to QVariant maps, including nested arrays.
      // Keep the original JS request so the shared card receives its answer options.
      model: root.requests.slice(0, 3).map(function(request) { return request.ref })
      delegate: Loader {
        id: slot
        required property string modelData
        readonly property var request: root.requests.filter(function(request) { return request.ref === slot.modelData })[0] || null
        width: stack.width
        height: item ? item.implicitHeight : 0
        sourceComponent: !request ? null : request.kind === "approval" ? approval : request.kind === "login" ? login : question
        Component {
          id: approval
          ApprovalCard {
            service: root.service
            item: slot.request
            popup: true
            compact: true
            life: 15000
            onExpired: root.service.hideRequestPopup(item.ref)
            dismissName: "Later · Keep this approval in the console"
            onDismissed: root.service.hideRequestPopup(item.ref)
            onBodyClicked: root.service.openRequest(item)
          }
        }
        Component {
          id: login
          LoginCard {
            service: root.service
            item: slot.request
            popup: true
            compact: true
            life: 15000
            onExpired: root.service.hideRequestPopup(item.ref)
            dismissName: "Not now · Keep this login request for later"
            onDismissed: {
              root.service.hideRequestPopup(item.ref)
              if (canAssist) assist("defer")
            }
            onBodyClicked: root.service.openRequest(item)
            onSettingsWanted: root.service.openRequestSettings()
          }
        }
        Component {
          id: question
          QuestionCard {
            service: root.service
            item: slot.request
            popup: true
            compact: true
            life: 15000
            onExpired: root.service.hideRequestPopup(item.ref)
            dismissName: "Later · Keep this question in the console"
            onDismissed: root.service.hideRequestPopup(item.ref)
            onBodyClicked: root.service.openRequest(item)
          }
        }
      }
    }
    ActionButton {
      visible: root.requests.length > 3
      width: parent.width
      label: "+" + (root.requests.length - 3) + " More"
      onClicked: root.service.openRequest(root.requests[3])
    }
  }
  }
}
