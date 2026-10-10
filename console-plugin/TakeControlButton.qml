import QtQuick

// Shared by the console, question/login cards and desktop pop-ups.
ActionButton {
  id: root
  property var service: null
  property var host: null
  property string computerId: ""
  property string keyHint: ""
  property string actionLabel: "Join"
  property var confirmation: null
  signal joinRequested()
  readonly property bool connecting: !!service && service.connectingOn(computerId)
  label: connecting ? "Connecting…" : actionLabel + keyHint
  role: "primary"
  blocked: !service || service.controlBlockedReason(computerId) !== ""
  disabledReason: service ? service.controlBlockedReason(computerId) : "This computer is not available."
  tooltipText: ""
  onHotChanged: if (hot && service) service.warmComputer(computerId)
  onActiveFocusChanged: if (activeFocus && service) service.warmComputer(computerId)
  onClicked: {
    if (!service || connecting) return
    var accepted = service.requestTakeControl(computerId, function(options) {
      if (root.host && typeof root.host.askConfirm === "function") { root.host.askConfirm(Object.assign({}, options, { anchor: root })); return }
      if (root.confirmation) { root.cancelConfirm(); return }
      root.confirmation = Object.assign({}, options, { anchor: root })
    })
    if (accepted) root.joinRequested()
  }
  function cancelConfirm() { confirmation = null; forceActiveFocus() }
  function runConfirm() {
    var pending = confirmation
    confirmation = null
    if (pending && pending.valid()) pending.run()
    else if (service) service.actionError = "Control changed. Try Join again."
    forceActiveFocus()
  }
  onComputerIdChanged: confirmation = null
  NearbyConfirm { host: root }
}
