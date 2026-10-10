import QtQuick
import qs.Commons
import qs.Ui
import "ServiceBridge.js" as ServiceBridge
import "StatusModel.js" as StatusModel

// The ibara mark plus one square for the fleet: a count when approvals, agents' questions or
// computers need you (clicking opens that one thing, or the fleet when there are several), else a
// dot for its worst state: red for a computer offline or needing attention for a minute or more,
// you in control purple, agents working blue, otherwise nothing. Every red here has a toast in
// the console that says why, and a toast put away there takes its red with it. While ibara isn't
// running on this computer the mark is dimmed with no square, and its tooltip says so: nothing
// it would show can be trusted or answered until ibara runs again.
BarWidget {
  id: root
  moduleName: "io.zet.ibara"

  property var ibaraService: null
  readonly property bool stopped: !!ibaraService && ibaraService.serviceStopped === true
  readonly property Tokens tokens: Tokens {}
  readonly property var computers: ibaraService && Array.isArray(ibaraService.computers) ? ibaraService.computers.filter(function(c) { return !!(c && c.computer_id) }) : []
  // Who is using the computers lights the mark, from each computer's own state: an agent waiting
  // for your approval is still at work there (the badge counts what waits for you).
  readonly property var counts: {
    var result = { attention: 0, offline: 0, connecting: 0, human: 0, mine: 0, working: 0, paused: 0, ready: 0 }
    for (var i = 0; i < computers.length; i++) {
      var state = StatusModel.computerState(computers[i])
      if (result[state] !== undefined) result[state] += 1
      if (state === "human" && tokens.actor(computers[i]) === "you") result.mine += 1
    }
    return result
  }
  // Offline or needing attention for a minute or more; a restart or an update never counts.
  readonly property var problems: !stopped && ibaraService && Array.isArray(ibaraService.problems) ? ibaraService.problems : []
  readonly property string worst: stopped ? "" : problems.length > 0 ? "attention" : counts.mine > 0 ? "human" : counts.working > 0 ? "working" : ""
  // Approvals, login requests, questions and computers that need you, read every 10 s even while nothing is open.
  readonly property var approvals: !stopped && ibaraService && Array.isArray(ibaraService.approvals) ? ibaraService.approvals.filter(function(item) { return ibaraService.requestNeedsAlert(item) }) : []
  readonly property var logins: !stopped && ibaraService && Array.isArray(ibaraService.logins) ? ibaraService.logins.filter(function(item) { return ibaraService.requestNeedsAlert(item) }) : []
  readonly property var questions: !stopped && ibaraService && Array.isArray(ibaraService.questions) ? ibaraService.questions.filter(function(item) { return ibaraService.requestNeedsAlert(item) }) : []
  readonly property var needsYou: !stopped && ibaraService && Array.isArray(ibaraService.needsYou) ? ibaraService.needsYou : []
  readonly property int needsCount: approvals.length + logins.length + questions.length + needsYou.length
  // The same counts and words as the fleet header, listing only what is not zero.
  readonly property string summary: {
    if (!ibaraService) return "Connecting"
    if (stopped) return "ibara isn't running on this computer"
    var needs = []
    if (ibaraService.updateAvailable) needs.push("ibara " + ibaraService.updateAvailable + " available")
    if (approvals.length) needs.push(approvals.length === 1 ? "1 approval waiting" : approvals.length + " approvals waiting")
    if (logins.length) needs.push(logins.length === 1 ? "1 login request waiting" : logins.length + " login requests waiting")
    if (questions.length) needs.push(questions.length === 1 ? "1 question waiting" : questions.length + " questions waiting")
    if (needsYou.length) needs.push(needsYou.length === 1 ? needsYou[0].label + " needs you" : needsYou.length + " computers need you")
    if (problems.length) needs.push(problems.length === 1 ? problems[0].heading : problems.length + " computers need attention")
    if (!computers.length) return needs.concat(["No computers"]).join(" · ")
    var rows = tokens.fleetCountRows(computers).filter(function(row) { return row.count > 0 && row.state !== "ready" })
    if (!rows.length) return needs.concat([counts.ready ? "Ready for Work " + counts.ready : "Connecting"]).join(" · ")
    return needs.concat(rows.map(function(row) { return row.label + " " + row.count })).join(" · ")
  }
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false
  readonly property real openPanelIndicatorWidth: mark.width

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function resolveService() {
    var next = ServiceBridge.current()
    if (next !== ibaraService) ibaraService = next
    if (ibaraService && typeof ibaraService.applySettings === "function") ibaraService.applySettings(settings)
    injectPanel()
  }

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("service" in target) target.service = root.ibaraService
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  function open() { if (panelLoader.item) panelLoader.item.open() }
  function close() { if (panelLoader.item) panelLoader.item.close() }
  function toggle() { if (panelLoader.item) panelLoader.item.toggle() }
  function summon(payload) { if (root.bar && root.bar.shell) { root.close(); root.bar.shell.summon("io.zet.ibara", JSON.stringify(payload)) } }
  // What needs you, opened directly: one computer (with its one approval under the keyboard),
  // else the fleet, where every approval and every computer that needs you shows first. With
  // nothing to answer, the red dot opens what it is about in the same way.
  function openNeeds() {
    var ids = []
    approvals.concat(logins, questions, needsYou).forEach(function(item) { if (ids.indexOf(item.computer_id) === -1) ids.push(item.computer_id) })
    if (!ids.length) problems.forEach(function(item) { if (ids.indexOf(item.computer_id) === -1) ids.push(item.computer_id) })
    if (ids.length !== 1) { summon({ route: "fleet" }); return }
    var payload = { route: "computer", computerId: ids[0] }
    if (approvals.length === 1) { payload.focus = "approval"; payload.ref = approvals[0].ref }
    summon(payload)
  }
  function closeForPopoutSwitch() { if (panelLoader.item) panelLoader.item.closeForPopoutSwitch() }

  onBarChanged: injectPanel()
  onSettingsChanged: resolveService()
  onIbaraServiceChanged: injectPanel()
  Component.onCompleted: resolveService()

  Timer {
    interval: 250
    repeat: true
    running: !root.ibaraService
    triggeredOnStart: true
    onTriggered: root.resolveService()
  }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("QuickPanel.qml")
    visible: false
    onStatusChanged: if (status === Loader.Error) console.warn("io.zet.ibara panel failed to load")
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "ibara"
    labelVisible: false
    fixedWidth: root.vertical ? -1 : Style.bar.iconSlot
    fixedHeight: root.vertical ? Style.bar.iconSlot : -1
    // One accent flash behind the mark when something new needs you.
    Rectangle {
      id: flash
      anchors.centerIn: mark
      width: mark.width + Style.space(8)
      height: width
      radius: 0
      color: Color.accent
      opacity: 0
      Accessible.ignored: true
      NumberAnimation on opacity { id: flashAnim; running: false; from: 0.45; to: 0; duration: 300; easing.type: Easing.OutCubic }
    }
    IbaraMark {
      id: mark
      anchors.centerIn: parent
      width: Style.bar.iconCanvas + Style.space(2)
      height: width
      color: button.foreground
    }
    Rectangle {
      visible: root.worst !== "" && root.needsCount === 0
      width: Style.space(5)
      height: width
      radius: 0
      anchors.right: mark.right
      anchors.bottom: mark.bottom
      anchors.rightMargin: -Style.space(1)
      anchors.bottomMargin: -Style.space(1)
      color: root.worst === "attention" ? button.activeColor : root.tokens.stateColor(root.worst)
      border.width: root.bar && root.bar.transparent ? 0 : 1
      border.color: root.bar ? root.bar.background : Color.bar.background
      Accessible.ignored: true
      SequentialAnimation on opacity {
        running: root.worst === "working"
        loops: Animation.Infinite
        alwaysRunToEnd: true
        NumberAnimation { to: 0.35; duration: 1000; easing.type: Easing.InOutSine }
        NumberAnimation { to: 1; duration: 1000; easing.type: Easing.InOutSine }
      }
    }
    // The count of what needs you, in place of the dot.
    Rectangle {
      visible: root.needsCount > 0
      width: Math.max(height, countText.implicitWidth + Style.space(6))
      height: Style.space(12)
      radius: 0
      anchors.right: mark.right
      anchors.rightMargin: -Style.space(4)
      anchors.bottom: mark.bottom
      anchors.bottomMargin: -Style.space(2)
      color: button.activeColor
      border.width: root.bar && root.bar.transparent ? 0 : 1
      border.color: root.bar ? root.bar.background : Color.bar.background
      Accessible.ignored: true
      scale: 1
      NumberAnimation on scale { id: popAnim; running: false; from: 1.3; to: 1; duration: 200; easing.type: Easing.OutCubic }
      Text {
        id: countText
        anchors.centerIn: parent
        text: root.needsCount > 9 ? "9+" : String(root.needsCount)
        textFormat: Text.PlainText
        color: root.bar ? root.bar.background : Color.bar.background
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        font.bold: true
      }
    }
    Accessible.role: Accessible.Button
    Accessible.name: "ibara · " + root.summary + (root.needsCount > 0 || root.problems.length > 0 ? ". Press to open what needs you; right-click for the console." : ". Press for the fleet summary; right-click for the console.")
    Accessible.onPressAction: if (root.needsCount > 0 || root.problems.length > 0) root.openNeeds(); else root.toggle()
    tooltipText: "ibara · " + root.summary
    dimmed: !root.ibaraService || root.stopped
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.MiddleButton && root.ibaraService) root.ibaraService.refresh()
      else if (buttonCode === Qt.RightButton) root.summon({})
      else if (root.needsCount > 0 || root.problems.length > 0) root.openNeeds()
      else root.toggle()
    }
  }
  property int lastNeeds: 0
  onNeedsCountChanged: {
    if (needsCount > lastNeeds) { flashAnim.restart(); popAnim.restart() }
    lastNeeds = needsCount
  }
}
