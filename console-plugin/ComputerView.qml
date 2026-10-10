import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Inside one computer: the grouped fleet list for switching on the left, the computer's
// header actions and its tabs on the right. The tabs sit right under the header: its repair,
// approvals and files on their way are toasts (Toasts.qml). Tab order: header, list, tabs, tab
// content.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  property string tab: "screen"
  property string searchText: ""
  // The header is as tall as its tallest line plus a hairline margin, as on the fleet.
  readonly property real headerHeight: Math.ceil(Math.max(headName.implicitHeight, headActions.implicitHeight)) + Style.space(6)
  readonly property real listWidth: Style.space(220)
  readonly property Tokens tokens: Tokens {}
  readonly property string fleetState: tokens.stateOf(computer)
  readonly property string computerLabel: tokens.label(computer)
  readonly property bool holding: !!host && !!computerId && host.holds(computerId)
  readonly property bool viewerOpen: holding && host.viewerOpen(computerId)
  // T, V and H from the console (Console.handleKey): the header's Join, Open or Close
  // Viewer and Hand Back, each with its key after its name. The button takes the keyboard, so a
  // confirmation attaches to it.
  function controlKey(control) {
    var button = control === "take" ? takeControlButton : control === "viewer" ? viewerButton : control === "handback" ? handBackButton : null
    if (!button || !button.visible) return false
    button.forceActiveFocus()
    button.clicked()
    return true
  }
  // Renaming turns the title into a field: Enter saves, Escape cancels. The name is this
  // machine's own label for the computer; the computer itself is unchanged.
  property bool renaming: false
  readonly property bool canRename: !!computer && computer.trust_state === "verified"
  readonly property string controlReason: host ? host.controlBlockedReason(computer) : "Unavailable"
  readonly property var tabs: [
    { id: "screen", label: "Screen" }, { id: "windows", label: "Windows" }, { id: "logins", label: "Logins" }, { id: "activity", label: "Activity" }, { id: "files", label: "Files" },
    { id: "access", label: "Access" }, { id: "system", label: "System" }, { id: "settings", label: "Settings" }
  ]
  // A pause a person made waits for Resume, and so does ibara's own pause after a restart when
  // the computer's Resume agents after a restart setting is off; otherwise ibara's ends by itself.
  // Both follow the computer's own state, so an approval waiting there never hides them.
  readonly property bool canResume: StatusModel.pauseAction(computer) === "resume"
  readonly property bool canPause: StatusModel.pauseAction(computer) === "pause"
  readonly property bool pauseBusy: !!service && !!service.busy["pause:" + computerId]
  readonly property var computers: service && Array.isArray(service.computers) ? service.computers.filter(function(c) { return !!(c && c.computer_id) }) : []
  readonly property var listed: {
    var term = searchText.trim().toLocaleLowerCase()
    var rows = computers.filter(function(c) { return !term || tokens.label(c).toLocaleLowerCase().indexOf(term) !== -1 })
    return tokens.sortComputers(rows, "attention")
  }
  readonly property var groupCounts: {
    var counts = { attention: 0, use: 0, ready: 0 }
    for (var i = 0; i < listed.length; i++) counts[tokens.groupOf(tokens.stateOf(listed[i]))] += 1
    return counts
  }

  function computerById(id) {
    for (var i = 0; i < computers.length; ++i) if (String(computers[i].computer_id) === id) return computers[i]
    return null
  }
  function focusSearch() { search.focusInput() }
  function focusDefault() {
    // The bar opened one approval: the keyboard lands on its Approve, in its toast.
    var pending = host ? host.pendingFocus : null
    var wanted = pending ? pending.ref : ""
    if (host) host.pendingFocus = null
    if (wanted && (pending.kind === "request" ? host.focusRequest(wanted) : host.focusApproval(wanted))) return
    var index = indexOfComputer(computerId)
    if (index >= 0) list.currentIndex = index
    list.forceActiveFocus()
  }
  function reload() { var page = currentTab(); if (page && typeof page.reload === "function") page.reload() }
  function startRename() {
    if (!canRename || !service) return
    renameField.text = computerLabel
    renaming = true
    renameField.focusInput()
    renameField.input.selectAll()
  }
  function endRename() {
    renaming = false
    renameButton.forceActiveFocus()
  }
  function saveRename() {
    var name = renameField.text.trim()
    if (!name || name === computerLabel) { endRename(); return }
    if (service && service.renameComputer(computerId, name)) endRename()
  }
  function currentTab() {
    var page = tab === "windows" ? windowsPage : tab === "logins" ? loginsPage : tab === "activity" ? activityPage : tab === "files" ? filesPage : tab === "access" ? accessPage : tab === "system" ? systemPage : tab === "settings" ? settingsPage : screenPage
    return page.item
  }
  function indexOfComputer(id) {
    for (var i = 0; i < listIds.count; i++) if (String(listIds.get(i).computerId) === id) return i
    return -1
  }
  function openAt(index) {
    if (index < 0 || index >= listIds.count || !host) return
    host.showComputer(String(listIds.get(index).computerId), host.tab)
  }

  ListModel { id: listIds }
  function syncList() {
    var rows = listed
    var same = listIds.count === rows.length
    for (var n = 0; same && n < rows.length; ++n)
      if (String(listIds.get(n).computerId) !== String(rows[n].computer_id) || listIds.get(n).group !== tokens.groupOf(tokens.stateOf(rows[n]))) same = false
    if (same) return
    var current = list.currentIndex >= 0 && list.currentIndex < listIds.count ? String(listIds.get(list.currentIndex).computerId) : computerId
    listIds.clear()
    for (var j = 0; j < rows.length; ++j) listIds.append({ computerId: String(rows[j].computer_id), group: tokens.groupOf(tokens.stateOf(rows[j])) })
    list.currentIndex = indexOfComputer(current)
  }
  // Thumbnails on screen plus the open computer; the service previews the open one at selected quality.
  function publishVisibility() {
    if (!service || typeof service.setVisibleComputerIds !== "function" || !visible) return
    var ids = computerId ? [computerId] : []
    for (var i = 0; i < listIds.count; i++) {
      var item = list.itemAtIndex(i)
      if (!item) continue
      if (item.y + item.height > list.contentY && item.y < list.contentY + list.height) {
        var id = String(listIds.get(i).computerId)
        if (ids.indexOf(id) === -1) ids.push(id)
      }
    }
    service.setVisibleComputerIds(ids)
  }
  onListedChanged: { syncList(); Qt.callLater(root.publishVisibility) }
  onVisibleChanged: publishVisibility()
  onComputerIdChanged: {
    renaming = false
    var index = indexOfComputer(computerId)
    if (index >= 0) { list.currentIndex = index; list.positionViewAtIndex(index, ListView.Contain) }
    Qt.callLater(root.publishVisibility)
  }
  Component.onCompleted: syncList()

  // A tab is built the first time it is shown and kept while this view exists. It stays
  // hidden until built, so it sees the same hidden-to-shown change that starts its reads.
  // The latch is set from handlers (a binding on `active` would re-enter itself while loading)
  // and only once creation has settled, so a passing initial tab never builds a page.
  component TabPage: Loader {
    property string page
    property string current
    property bool settled: false
    anchors.fill: parent
    active: false
    visible: current === page && status === Loader.Ready
    onCurrentChanged: if (settled && current === page) active = true
    Component.onCompleted: { settled = true; if (current === page) active = true }
  }

  // ---- top bar: fleet on the left, this computer on the right
  Item {
    id: header
    width: parent.width
    height: root.headerHeight
    Item {
      id: fleetHeader
      width: root.listWidth
      height: parent.height
      IbaraMark {
        id: mark
        width: Style.space(22)
        height: width
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        color: Color.accent
      }
      ActionButton {
        id: fleetLink
        anchors.left: mark.right
        anchors.leftMargin: Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        glyph: "‹"
        label: "Fleet"
        role: "quiet"
        tooltipText: "Back to the fleet (Escape)"
        Accessible.name: "Back to the fleet"
        onClicked: if (root.host) root.host.showFleet()
      }
      Copy {
        anchors.right: parent.right
        anchors.rightMargin: Style.space(14)
        anchors.verticalCenter: parent.verticalCenter
        text: String(root.computers.length)
        dimmed: true
      }
    }
    Rectangle { x: root.listWidth; width: 1; height: parent.height; radius: 0; color: root.tokens.rule }
    Item {
      anchors.left: fleetHeader.right
      anchors.leftMargin: Style.space(18)
      anchors.right: parent.right
      height: parent.height
      StateMarker {
        id: headMarker
        tokens: root.tokens
        fleetState: root.fleetState
        size: Style.space(11)
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
      }
      Copy {
        id: headName
        visible: !root.renaming
        anchors.left: headMarker.right
        anchors.leftMargin: Style.space(10)
        anchors.verticalCenter: parent.verticalCenter
        width: Math.min(implicitWidth, parent.width * 0.3)
        text: root.computerLabel
        font.pixelSize: Style.font.heading + Style.space(4)
        font.bold: true
        wrapMode: Text.NoWrap
        elide: Text.ElideRight
      }
      FieldInput {
        id: renameField
        visible: root.renaming
        anchors.left: headMarker.right
        anchors.leftMargin: Style.space(10)
        anchors.verticalCenter: parent.verticalCenter
        width: Math.min(Style.space(340), parent.width * 0.3)
        maximumLength: 128
        fontSize: Style.font.heading
        accessibleName: "New name for " + root.computerLabel
        hint: "Enter"
        onAccepted: root.saveRename()
        Keys.onEscapePressed: function(event) { root.endRename(); event.accepted = true }
      }
      ActionButton {
        id: renameButton
        visible: root.canRename
        anchors.left: root.renaming ? renameField.right : headName.right
        anchors.leftMargin: Style.space(6)
        anchors.verticalCenter: parent.verticalCenter
        label: root.renaming ? "Save" : "Rename"
        role: "quiet"
        size: "small"
        blocked: !root.service || !!root.service.mutating
        disabledReason: blocked && root.service ? "Wait for the current action to finish." : ""
        // While renaming, Save has no tooltip: one under it would cover the tabs. Enter and
        // Escape are said to screen readers instead.
        tooltipText: disabledReason || (root.renaming ? "" : "Rename " + root.computerLabel + " on this computer")
        Accessible.description: disabledReason || (root.renaming ? "Enter saves the name; Escape cancels" : "")
        Accessible.name: root.renaming ? "Save name" : "Rename " + root.computerLabel
        onClicked: {
          if (blocked) return
          if (root.renaming) root.saveRename()
          else root.startRename()
        }
      }
      // Marks this computer for the wall's Favorites filter; the card itself shows only a star.
      ActionButton {
        id: favoriteButton
        readonly property bool favorite: !!root.host && !!root.host.favorites[root.computerId]
        anchors.left: renameButton.visible ? renameButton.right : headName.right
        anchors.leftMargin: Style.space(2)
        anchors.verticalCenter: parent.verticalCenter
        glyph: favorite ? "★" : "☆"
        label: "Favorite"
        role: "quiet"
        size: "small"
        selected: favorite
        tooltipText: favorite ? "Remove " + root.computerLabel + " from the Favorites filter" : "Add " + root.computerLabel + " to the Favorites filter"
        Accessible.name: favorite ? "Remove from favorites" : "Add to favorites"
        onClicked: if (root.host) root.host.toggleFavorite(root.computerId)
      }
      Copy {
        anchors.left: favoriteButton.right
        anchors.leftMargin: Style.space(14)
        anchors.right: headActions.left
        anchors.rightMargin: Style.space(12)
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.StyledText
        text: {
          if (root.service && root.service.connectingOn(root.computerId)) return "Connecting…"
          var who = root.tokens.actor(root.computer)
          var what = root.tokens.activity(root.computer, root.service ? root.service.nowMs : 0)
          var words = root.tokens.activityMarkup(root.computer, root.service ? root.service.nowMs : 0)
          return who && what && what.toLowerCase().indexOf(who.toLowerCase()) !== 0 ? root.tokens.ink(who + " · ", root.tokens.dim) + words : (what ? words : root.tokens.ink(who || root.tokens.stateLabel(root.fleetState, root.computer), root.tokens.textTint(root.tokens.stateColor(root.fleetState))))
        }
        wrapMode: Text.NoWrap
        elide: Text.ElideRight
      }
      // Secondary actions, then the one primary action last: Join, or, while you hold
      // control, Open Viewer then Hand Back in its place. Close stays apart, past a rule, so it is
      // never read as one of them.
      Row {
        id: headActions
        anchors.right: closeRule.left
        anchors.rightMargin: Style.space(12)
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(8)
        ActionButton {
          visible: !!root.host && !!root.host.shell && root.host.shell.standalone === true
          label: "Requests (" + (root.service ? root.service.requestItems.length : 0) + ")"
          Accessible.name: "Open pending requests"
          onClicked: if (root.host) root.host.openRequests()
        }
        ActionButton {
          visible: root.canResume
          label: root.pauseBusy ? "Resuming…" : "Resume"
          blocked: root.pauseBusy
          tooltipText: "Let the agents on " + root.computerLabel + " work again"
          onClicked: if (!blocked && root.service) root.service.resumeAgents(root.computerId)
        }
        ActionButton {
          visible: root.canPause
          label: root.pauseBusy ? "Pausing…" : "Pause Agents"
          blocked: root.pauseBusy
          tooltipText: "Stop the agents on " + root.computerLabel + " until you choose Resume"
          onClicked: if (!blocked && root.service) root.service.pauseAgents(root.computerId)
        }
        ActionButton {
          visible: root.fleetState === "offline" && !!root.computer && !!root.computer.wake
          label: root.service && root.service.busy["wake:" + root.computerId] ? "Waking…" : "Wake"
          role: "primary"
          blocked: !!root.service && !!root.service.busy["wake:" + root.computerId]
          onClicked: if (!blocked) root.service.wake(root.computerId)
        }
        ActionButton {
          label: "Send File"
          onClicked: if (root.host) root.host.setTab("files")
        }
        TakeControlButton {
          id: takeControlButton
          service: root.service
          host: root.host
          computerId: root.computerId
          visible: !root.holding || connecting
          keyHint: " (T)"
          Accessible.name: "Join"
        }
        ActionButton {
          id: viewerButton
          visible: root.holding && !(root.service && root.service.connectingOn(root.computerId))
          label: (root.viewerOpen ? "Close Viewer" : "Open Viewer") + " (V)"
          blocked: !root.viewerOpen && root.controlReason !== ""
          disabledReason: blocked ? root.controlReason : ""
          tooltipText: root.viewerOpen ? "Close the viewer; " + root.computerLabel + " stays yours until Hand Back" : ""
          Accessible.name: root.viewerOpen ? "Close Viewer" : "Open Viewer"
          onClicked: {
            if (!root.host) return
            if (blocked) root.host.notify(root.controlReason, false)
            else root.host.toggleViewer(root.computerId)
          }
        }
        ActionButton {
          id: handBackButton
          visible: root.holding
          label: "Hand Back (H)"
          role: "primary"
          blocked: !!(root.service && root.service.mutating)
          disabledReason: blocked ? "Wait for the current action to finish." : ""
          tooltipText: ""
          Accessible.name: "Hand Back"
          onClicked: {
            if (!root.host) return
            if (blocked) root.host.notify(disabledReason, false)
            else root.host.handBack(root.computerId)
          }
        }
      }
      Rectangle {
        id: closeRule
        anchors.right: closeButton.left
        anchors.rightMargin: Style.space(10)
        anchors.verticalCenter: parent.verticalCenter
        width: 1
        height: closeButton.height
        radius: 0
        color: root.tokens.rule
      }
      ActionButton {
        id: closeButton
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        label: "✕"
        role: "quiet"
        Accessible.name: "Close console"
        tooltipText: "Close"
        onClicked: if (root.host) root.host.requestClose()
      }
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // ---- left: the fleet list
  Item {
    id: side
    y: root.headerHeight
    width: root.listWidth
    height: parent.height - y
    FieldInput {
      id: search
      y: Style.space(8)
      width: parent.width - Style.space(14)
      placeholder: "Find a computer"
      accessibleName: "Find a computer"
      hint: "/"
      onTextChanged: root.searchText = text
      onAccepted: root.focusDefault()
    }
    ActionButton {
      id: addButton
      anchors.bottom: parent.bottom
      width: parent.width - Style.space(14)
      glyph: "+"
      label: "Add Computer"
      onClicked: if (root.host) root.host.showAdd()
    }
    ListView {
      id: list
      anchors.top: search.bottom
      anchors.topMargin: Style.space(8)
      anchors.bottom: addButton.top
      anchors.bottomMargin: Style.space(10)
      width: parent.width - Style.space(14)
      clip: true
      activeFocusOnTab: true
      keyNavigationEnabled: true
      boundsBehavior: Flickable.StopAtBounds
      model: listIds
      currentIndex: -1
      highlightFollowsCurrentItem: false
      section.property: "group"
      section.delegate: Item {
        required property string section
        width: list.width
        height: Style.space(24)
        Copy {
          anchors.left: parent.left
          anchors.leftMargin: Style.space(4)
          anchors.bottom: parent.bottom
          anchors.bottomMargin: Style.space(5)
          eyebrow: true
          text: root.tokens.groupLabel(section) + " · " + Number(root.groupCounts[section] || 0)
        }
      }
      onContentYChanged: root.publishVisibility()
      onHeightChanged: root.publishVisibility()
      Keys.onReturnPressed: root.openAt(currentIndex)
      Keys.onEnterPressed: root.openAt(currentIndex)
      Keys.onSpacePressed: root.openAt(currentIndex)
      delegate: Rectangle {
        id: row
        required property int index
        required property string computerId
        readonly property var computer: root.computerById(computerId)
        readonly property string fleetState: root.tokens.stateOf(computer)
        readonly property bool open: computerId === root.computerId
        readonly property bool cursor: list.activeFocus && list.currentIndex === index
        width: list.width
        height: Style.space(40)
        radius: 0
        color: open ? Qt.alpha(Color.accent, 0.14) : rowHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
        border.width: cursor || open ? Math.max(1, Style.space(cursor ? 2 : 1)) : 0
        border.color: cursor ? Color.accent : Qt.alpha(Color.accent, 0.6)
        Accessible.role: Accessible.Button
        Accessible.name: root.tokens.label(computer) + ", " + root.tokens.stateLabel(fleetState, computer)
        HoverHandler { id: rowHover }
        MouseArea { anchors.fill: parent; onClicked: { list.currentIndex = row.index; root.openAt(row.index) } }
        // A small picture, then the name with its state's mark and, under it, what it is doing.
        Rectangle {
          id: thumb
          x: Style.space(4)
          anchors.verticalCenter: parent.verticalCenter
          width: Style.space(44)
          height: Style.space(25)
          radius: 0
          color: Qt.alpha(Color.popups.text, 0.08)
          clip: true
          SteadyPreview {
            anchors.fill: parent
            opacity: row.fleetState === "offline" ? 0.38 : 1
            incoming: root.tokens.tileFrameSource(row.computer, root.service && root.service.denied)
            drop: root.tokens.dropFrame(row.computer)
            onShownSizeSettled: (width, height) => { if (root.service) root.service.notePreviewSize("tile", width, height) }
          }
        }
        StateMarker {
          id: rowMarker
          tokens: root.tokens
          anchors.left: thumb.right
          anchors.leftMargin: Style.space(8)
          anchors.verticalCenter: rowName.verticalCenter
          fleetState: row.fleetState
        }
        Copy {
          id: rowName
          anchors.left: rowMarker.right
          anchors.leftMargin: Style.space(5)
          anchors.right: parent.right
          anchors.rightMargin: Style.space(6)
          y: Style.space(5)
          text: root.tokens.label(row.computer)
          font.bold: true
          wrapMode: Text.NoWrap
          elide: Text.ElideRight
          color: row.fleetState === "offline" ? Qt.alpha(Color.popups.text, 0.6) : Color.popups.text
        }
        Copy {
          anchors.left: rowMarker.left
          anchors.right: parent.right
          anchors.rightMargin: Style.space(6)
          anchors.top: rowName.bottom
          anchors.topMargin: Style.space(1)
          textFormat: Text.StyledText
          text: root.tokens.activityMarkup(row.computer, root.service ? root.service.nowMs : 0)
          Accessible.name: root.tokens.activity(row.computer, root.service ? root.service.nowMs : 0)
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.NoWrap
          elide: Text.ElideRight
        }
      }
    }
    Rectangle { anchors.right: parent.right; width: 1; height: parent.height; radius: 0; color: root.tokens.rule }
  }

  // ---- right: tabs right under the header, then the open tab
  Item {
    id: main
    x: root.listWidth + Style.space(18)
    y: root.headerHeight
    width: parent.width - x
    height: parent.height - y

    FocusScope {
      id: tabBar
      width: parent.width
      height: Style.space(34)
      activeFocusOnTab: true
      Accessible.role: Accessible.PageTabList
      Accessible.name: "Computer tabs"
      function step(delta) {
        var ids = root.tabs.map(function(t) { return t.id })
        var next = Math.max(0, Math.min(ids.length - 1, ids.indexOf(root.tab) + delta))
        if (root.host) root.host.setTab(ids[next])
      }
      Keys.onLeftPressed: step(-1)
      Keys.onRightPressed: step(1)
      Row {
        height: parent.height
        spacing: Style.space(4)
        Repeater {
          model: root.tabs
          delegate: Item {
            required property var modelData
            readonly property bool current: root.tab === modelData.id
            width: tabLabel.implicitWidth + Style.space(28)
            height: parent.height
            Accessible.role: Accessible.PageTab
            Accessible.name: modelData.label
            Accessible.selected: current
            Rectangle {
              anchors.fill: parent
              anchors.topMargin: Style.space(3)
              anchors.bottomMargin: Style.space(3)
              radius: 0
              color: tabHover.hovered ? Qt.alpha(Color.popups.text, 0.06) : "transparent"
              border.width: parent.current && tabBar.activeFocus ? Math.max(2, Style.space(2)) : 0
              border.color: Color.accent
            }
            Copy {
              id: tabLabel
              anchors.centerIn: parent
              text: modelData.label
              font.bold: parent.current
              color: parent.current ? Color.popups.text : Qt.alpha(Color.popups.text, 0.66)
            }
            Rectangle {
              visible: parent.current
              anchors.bottom: parent.bottom
              width: parent.width
              height: Math.max(2, Style.space(2))
              radius: 0
              color: Color.accent
            }
            HoverHandler { id: tabHover }
            MouseArea { anchors.fill: parent; onClicked: { if (root.host) root.host.setTab(modelData.id); tabBar.forceActiveFocus() } }
          }
        }
      }
      Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
    }

    Item {
      id: pages
      anchors.top: tabBar.bottom
      anchors.topMargin: Style.space(10)
      width: parent.width
      anchors.bottom: parent.bottom
      TabPage {
        id: screenPage
        page: "screen"
        current: root.tab
        sourceComponent: Component {
          ScreenTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
            // From 1400 px wide the rail stands beside the picture; narrower, under it.
            railBeside: root.width >= root.tokens.wideAt
          }
        }
      }
      TabPage {
        id: windowsPage
        page: "windows"
        current: root.tab
        sourceComponent: Component {
          WindowsTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: loginsPage
        page: "logins"
        current: root.tab
        sourceComponent: Component {
          LoginsTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: activityPage
        page: "activity"
        current: root.tab
        sourceComponent: Component {
          ActivityTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: filesPage
        page: "files"
        current: root.tab
        sourceComponent: Component {
          FilesTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: accessPage
        page: "access"
        current: root.tab
        sourceComponent: Component {
          AccessTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: systemPage
        page: "system"
        current: root.tab
        sourceComponent: Component {
          SystemTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
      TabPage {
        id: settingsPage
        page: "settings"
        current: root.tab
        sourceComponent: Component {
          SettingsTab {
            host: root.host
            service: root.service
            computerId: root.computerId
            computer: root.computer
          }
        }
      }
    }

    // Files dropped anywhere on this computer's side are sent to it, as on its fleet card.
    DropArea {
      id: dropArea
      anchors.fill: parent
      keys: ["text/uri-list"]
      onDropped: function(drop) {
        if (!drop.hasUrls || !root.service) return
        root.service.dropFiles(root.computerId, drop.urls.map(function(url) { return String(url) }))
        drop.acceptProposedAction()
      }
    }
    Rectangle {
      anchors.fill: parent
      visible: dropArea.containsDrag
      radius: 0
      color: Qt.alpha(Color.accent, 0.10)
      border.width: Math.max(2, Style.space(2))
      border.color: Color.accent
      Copy {
        anchors.centerIn: parent
        text: "Drop to send to " + root.computerLabel
        font.pixelSize: Style.font.heading
        font.bold: true
      }
    }
  }
}
