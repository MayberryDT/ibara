import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// The fleet wall: stable live cards directly under the toolbar.
// Nothing sits between the toolbar and the cards: approvals, requests, notes and errors are
// toasts (Toasts.qml). Presentation only; records and preview subscriptions belong to Service,
// confirmations to the console.
Item {
  id: root
  property var host: null
  property var service: null
  property string searchText: ""
  property string filter: "all"
  readonly property string sortMode: service ? String(service.setting("fleet_sort", "name")) : "name"
  // The header is one line when the title, filters, Sort and actions fit side by side; otherwise
  // the filters and Sort take a second line under it. The counts beside the title take what room
  // is left, in their order, and those that don't fit are left out. No spare padding either way.
  readonly property real barHeight: Math.ceil(Math.max(titleRow.implicitHeight, headerActions.implicitHeight)) + Style.space(6)
  readonly property real filterHeight: Style.space(30)
  readonly property real barGap: Style.space(18)
  readonly property real titleBaseWidth: mark.width + Style.space(10) + fleetTitle.implicitWidth + (totalText.visible ? Style.space(12) + totalText.implicitWidth : 0)
  readonly property real filterStripWidth: filterRow.implicitWidth + barGap + sortButton.width
  readonly property bool oneLine: titleBaseWidth + barGap + filterStripWidth + barGap + headerActions.implicitWidth <= width
  readonly property var countRows: tokens.fleetCountRows(computers)
  readonly property int countsShown: {
    var room = width - titleBaseWidth - barGap - headerActions.implicitWidth - (oneLine ? filterStripWidth + barGap : 0)
    var used = 0, n = 0
    for (var i = 0; i < countRows.length; i++) {
      // A count of 0 has no colored square.
      var w = Style.space(12) + 1 + Style.space(7) * 2 + (countRows[i].count > 0 ? Math.round(Style.space(9) * 1.6) + Style.space(7) : 0) + countMetrics.advanceWidth(countRows[i].label) + boldMetrics.advanceWidth(String(countRows[i].count))
      if (used + w > room) break
      used += w
      n += 1
    }
    return n
  }
  FontMetrics { id: countMetrics; font.family: Style.font.family; font.pixelSize: Style.font.body }
  FontMetrics { id: boldMetrics; font.family: Style.font.family; font.pixelSize: Style.font.body; font.bold: true }
  // With no computers the header is the title, Settings and close: the page's one action is Add Your First Computer.
  readonly property real headerHeight: oneLine || empty ? barHeight : barHeight + filterHeight
  readonly property Tokens tokens: Tokens {}
  readonly property var computers: service && Array.isArray(service.computers) ? service.computers.filter(function(c) { return !!(c && c.computer_id) }) : []
  readonly property var favorites: host ? host.favorites : ({})
  // Why Fleet Actions' Update ibara on All and Update Omarchy on All can't run now, or "". A
  // string, so the menu's items change only when the answer does, not with every picture.
  readonly property string updateAllBlocked: host ? host.updateAllPlan("update_ibara").blocked : "Connecting."
  // The one being sent now (update_ibara or update_omarchy), or "".
  readonly property string updateAllAction: service && service.updateAllRun ? String(service.updateAllRun.action || "") : ""
  readonly property var counts: {
    var c = service && service.fleetCounts ? service.fleetCounts : null
    if (c) return c
    var result = { total: computers.length, attention: 0, offline: 0, locked: 0, connecting: 0, human: 0, working: 0, paused: 0, ready: 0 }
    for (var i = 0; i < computers.length; i++) {
      var state = tokens.stateOf(computers[i])
      if (result[state] !== undefined) result[state] += 1
    }
    return result
  }
  readonly property int olderCount: service && service.latestRelease ? computers.filter(function(c) { return root.service.behind(c) }).length : 0
  readonly property int attentionCount: Number(counts.attention || 0) + Number(counts.offline || 0) + Number(counts.locked || 0)
  readonly property int useCount: Number(counts.human || 0) + Number(counts.working || 0) + Number(counts.paused || 0)
  readonly property var visibleComputers: {
    var term = searchText.trim().toLocaleLowerCase()
    var rows = computers.filter(function(c) {
      var id = String(c.computer_id)
      var state = tokens.stateOf(c)
      if (filter === "favorites" && !favorites[id]) return false
      if ((filter === "attention" || filter === "use" || filter === "ready") && tokens.groupOf(state) !== filter) return false
      return !term || tokens.label(c).toLocaleLowerCase().indexOf(term) !== -1
    })
    // Attention order is a snapshot chosen by the person, not a live sort on status.
    var named = tokens.sortComputers(rows, "name")
    var saved = service ? service.setting("fleet_order", []) : []
    if (sortMode !== "attention" || !Array.isArray(saved)) return named
    return named.sort(function(a, b) {
      var ai = saved.indexOf(String(a.computer_id)), bi = saved.indexOf(String(b.computer_id))
      if (ai < 0) ai = saved.length
      if (bi < 0) bi = saved.length
      return ai - bi || tokens.label(a).localeCompare(tokens.label(b))
    })
  }
  readonly property real cardGap: Style.space(10)
  // Under a card's picture: its name and its activity line.
  readonly property real cardTextHeight: Style.space(48)
  // The grid: two to five columns, each at least 300 wide.
  readonly property int gridColumns: Math.max(2, Math.min(5, Math.floor((body.width + cardGap) / Style.space(300))))
  readonly property real gridCardWidth: Math.floor((body.width + cardGap) / gridColumns) - cardGap
  // A lone card keeps its screen's shape; several cards are 16:9.
  property real singleAspect: 16 / 9
  readonly property real pictureAspect: visibleComputers.length === 1 ? singleAspect : 16 / 9
  // Up to four cards grow to use the wall: the number of columns that gives the widest cards
  // with every card in view. Never narrower than the grid's cards; otherwise the grid.
  readonly property var largeLayout: {
    var n = visibleComputers.length
    if (n < 1 || n > 4 || body.width <= 0 || body.height <= 0) return null
    var best = null
    for (var c = 1; c <= n; c++) {
      var byWidth = (body.width + cardGap) / c - cardGap
      var byHeight = (body.height / Math.ceil(n / c) - cardTextHeight - cardGap) * pictureAspect
      var w = Math.floor(Math.min(byWidth, byHeight))
      if (!best || w > best.width) best = { columns: c, width: w }
    }
    return best.width > gridCardWidth ? best : null
  }
  readonly property bool large: largeLayout !== null
  readonly property int columns: large ? largeLayout.columns : gridColumns
  readonly property bool loaded: !!service && service.directoryLoaded === true
  readonly property bool empty: computers.length === 0
  // ibara itself is not installed or not set up yet (this plugin came first, from the marketplace).
  readonly property string missing: service ? service.ibaraMissing : ""
  onMissingChanged: if (visible && missing) Qt.callLater(focusDefault)
  // ibara is installed here but isn't running: the wall greys its cards at their last known state.
  readonly property bool stopped: !!service && service.serviceStopped === true
  readonly property string filterWords: ({ attention: "Needs Attention", use: "In Use", ready: "Ready", favorites: "Favorites" })[filter] || ""
  // Connect an Agent, in the toolbar once ibara can give the prompt; it opens in a card attached to it.
  readonly property Item connectButton: connectAgent

  function computerById(id) {
    for (var i = 0; i < computers.length; ++i) if (String(computers[i].computer_id) === id) return computers[i]
    return null
  }
  function focusSearch() { search.focusInput() }
  // T, V and H from the console act on the card with the keyboard (Console.handleKey).
  function controlKey(control) { return !!grid.currentItem && grid.currentItem.pressControl(control) }
  function focusDefault() {
    if (visibleComputers.length) {
      if (grid.currentIndex < 0) grid.currentIndex = 0
      grid.forceActiveFocus()
    } else if (empty && missing && installButton.visible) installButton.forceActiveFocus()
    else if (empty && loaded) firstAdd.forceActiveFocus()
    else if (!empty) search.focusInput()
  }
  function reload() {
    if (!service || !visible) return
    service.loadPairRequests()
    service.loadConnectPrompt()
    // Which card is this computer: its menu offers Share This Computer.
    if (!service.tailnet) service.loadTailnet()
  }
  // The first read of an empty fleet, or its first computer, moves the keyboard to what the page offers now.
  onLoadedChanged: if (visible && loaded) Qt.callLater(focusDefault)
  onEmptyChanged: if (visible && loaded) Qt.callLater(focusDefault)

  // The wall's order changes in place: a computer leaving is removed, and one that moves is moved,
  // so the cards glide to their new places (each card's glide, in the grid's delegate).
  ListModel { id: wallIds }
  function syncWallIds() {
    var ids = visibleComputers.map(function(c) { return String(c.computer_id) })
    var same = wallIds.count === ids.length
    for (var n = 0; same && n < ids.length; ++n) if (String(wallIds.get(n).computerId) !== ids[n]) same = false
    if (same) return
    var current = grid.currentIndex >= 0 && grid.currentIndex < wallIds.count ? String(wallIds.get(grid.currentIndex).computerId) : ""
    for (var r = wallIds.count - 1; r >= 0; --r) if (ids.indexOf(String(wallIds.get(r).computerId)) === -1) wallIds.remove(r)
    for (var j = 0; j < ids.length; ++j) {
      if (j < wallIds.count && String(wallIds.get(j).computerId) === ids[j]) continue
      var at = -1
      for (var k = j + 1; k < wallIds.count; ++k) if (String(wallIds.get(k).computerId) === ids[j]) { at = k; break }
      if (at !== -1) wallIds.move(at, j, 1)
      else wallIds.insert(j, { computerId: ids[j] })
    }
    // Keep the keyboard position on the same computer when the list really changes.
    grid.currentIndex = current && ids.indexOf(current) !== -1 ? ids.indexOf(current) : (ids.length ? 0 : -1)
  }
  // Pause Agents and Resume as a sweep: when cards turn from working to paused (or back) together,
  // their marks change one after another, left to right, 150 ms apart. Changes that arrive within
  // 120 ms of each other are one sweep; a later one follows on after it.
  property var sweepQueue: []
  property double sweepFreeAt: 0
  function noteStateChange(cell) {
    var from = cell.shownState, to = cell.fleetState
    if ((from === "working" && to === "paused") || (from === "paused" && to === "working")) {
      if (sweepQueue.indexOf(cell.computerId) === -1) sweepQueue = sweepQueue.concat([cell.computerId])
      if (!sweepWindow.running) sweepWindow.start()
    } else cell.showState(0)
  }
  Timer {
    id: sweepWindow
    interval: 120
    onTriggered: {
      var order = root.sweepQueue.map(function(id) {
        for (var i = 0; i < wallIds.count; ++i) if (String(wallIds.get(i).computerId) === id) return i
        return -1
      }).filter(function(i) { return i !== -1 }).sort(function(a, b) { return a - b })
      root.sweepQueue = []
      var start = Math.max(0, root.sweepFreeAt - Date.now())
      for (var n = 0; n < order.length; ++n) {
        var cell = grid.itemAtIndex(order[n])
        if (cell) cell.showState(start + n * 150)
      }
      root.sweepFreeAt = Date.now() + start + order.length * 150
    }
  }
  // The service previews at most 20 of these, in order, so list the most visible rows first;
  // a sliver of a row scrolled mostly out of view must not take a slot from a whole row.
  function publishVisibility() {
    if (!service || typeof service.setVisibleComputerIds !== "function") return
    if (!visible) return
    var top = grid.contentY, bottom = grid.contentY + grid.height, rows = []
    var firstRow = Math.max(0, Math.floor(top / grid.cellHeight))
    var lastRow = Math.ceil(bottom / grid.cellHeight)
    for (var r = firstRow; r < lastRow; r++) {
      var shown = Math.min(bottom, (r + 1) * grid.cellHeight) - Math.max(top, r * grid.cellHeight)
      rows.push({ row: r, shown: shown })
    }
    rows.sort(function(a, b) { return b.shown - a.shown || a.row - b.row })
    var ids = []
    for (var k = 0; k < rows.length; k++)
      for (var i = rows[k].row * columns; i < Math.min(visibleComputers.length, (rows[k].row + 1) * columns); i++)
        ids.push(String(visibleComputers[i].computer_id))
    service.setVisibleComputerIds(ids, large)
  }
  // After the card size and columns follow the new list, never before: syncWallIds lays the grid
  // out at once, and a card still gliding or fading in keeps the place it was given then, so the
  // old size's places would leave the new cards stacked on each other.
  onVisibleComputersChanged: { Qt.callLater(syncWallIds); publishVisibility() }
  onVisibleChanged: { publishVisibility(); reload() }
  onWidthChanged: publishVisibility()
  onColumnsChanged: publishVisibility()
  onLargeChanged: publishVisibility()
  Component.onCompleted: { syncWallIds(); Qt.callLater(root.publishVisibility) }
  // A request lasts only while the other computer waits; an agent's first task can start any time.
  Timer { interval: 10000; repeat: true; running: root.visible && !!root.service; onTriggered: { root.service.loadPairRequests(); if (!root.service.firstTaskDone) root.service.loadConnectPrompt() } }

  // ---- top bar: title and counts, filters and Sort, then search, Connect an Agent, Add Computer and close
  Item {
    id: header
    width: parent.width
    height: root.headerHeight
    IbaraMark {
      id: mark
      width: Style.space(22)
      height: width
      anchors.left: parent.left
      y: (root.barHeight - height) / 2
      color: Color.accent
    }
    Row {
      id: titleRow
      anchors.left: mark.right
      anchors.leftMargin: Style.space(10)
      y: (root.barHeight - height) / 2
      spacing: Style.space(12)
      Copy { id: fleetTitle; text: "Fleet"; font.pixelSize: Style.font.heading + Style.space(4); font.bold: true; anchors.verticalCenter: parent.verticalCenter }
      Copy { id: totalText; visible: !root.empty; text: root.computers.length === 1 ? "1 computer" : root.computers.length + " computers"; dimmed: true; anchors.verticalCenter: parent.verticalCenter }
      // One label form for every count, label first: "In Use by You 1". A count of 0 has no square.
      // Each count rolls to its new number; the rows stay put while their number is all that changes.
      Repeater {
        model: root.countRows.length
        delegate: Row {
          readonly property var row: root.countRows[index] || ({ state: "ready", count: 0, label: "" })
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.space(7)
          visible: !root.empty && index < root.countsShown
          Rectangle { width: 1; height: Style.space(16); radius: 0; color: root.tokens.rule; anchors.verticalCenter: parent.verticalCenter }
          StateMarker { visible: row.count > 0; tokens: root.tokens; fleetState: row.state; anchors.verticalCenter: parent.verticalCenter }
          Copy { text: row.label; dimmed: true; anchors.verticalCenter: parent.verticalCenter }
          SlideText { text: String(row.count); bold: true; anchors.verticalCenter: parent.verticalCenter }
          Accessible.role: Accessible.StaticText
          Accessible.name: row.label + " " + row.count
        }
      }
    }
    // Filters and Sort: beside the title on one line, or a line of their own under it.
    // The selected filter's underline sits on the header's rule.
    Item {
      id: filters
      visible: !root.empty
      x: root.oneLine ? titleRow.x + titleRow.implicitWidth + root.barGap : 0
      y: root.oneLine ? 0 : root.barHeight
      width: root.oneLine ? headerActions.x - root.barGap - x : parent.width
      height: root.oneLine ? root.barHeight : root.filterHeight
      Row {
        id: filterRow
        spacing: Style.space(2)
        height: parent.height
        Repeater {
          model: [
            { id: "all", label: "All " + root.computers.length },
            { id: "attention", label: "Needs Attention " + root.attentionCount },
            { id: "use", label: "In Use " + root.useCount },
            { id: "ready", label: "Ready " + Number(root.counts.ready || 0) },
            { id: "favorites", label: "Favorites" }
          ]
          delegate: ActionButton {
            height: filterRow.height
            label: modelData.label
            role: "quiet"
            selected: root.filter === modelData.id
            onClicked: { root.filter = modelData.id; root.focusDefault() }
            Rectangle {
              visible: parent.selected
              anchors.bottom: parent.bottom
              width: parent.width
              height: Math.max(2, Style.space(2))
              radius: 0
              color: Color.accent
            }
          }
        }
      }
      // Sort is a short menu of the two orders. Its button keeps the width of its longest label,
      // so choosing an order never moves it.
      ActionMenu {
        id: sortButton
        readonly property var orders: [{ id: "attention", label: "Attention First" }, { id: "name", label: "Name" }]
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        width: Math.max(implicitWidth, Math.ceil(countMetrics.advanceWidth("Sort: Attention First ▾")) + button.padding * 2)
        label: "Sort: " + (root.sortMode === "attention" ? "Attention First" : "Name") + " ▾"
        accessibleName: "Sort: " + (root.sortMode === "attention" ? "attention first" : "by name")
        tooltipText: "Choose a remembered order. Attention First sorts once; status changes stay in place."
        items: orders.map(function(order) { return { id: order.id, label: order.label, selected: root.sortMode === order.id } })
        onTriggered: id => {
          if (root.service) root.service.saveFleetOrder(id, tokens.sortComputers(root.computers, id).map(function(c) { return String(c.computer_id) }))
        }
      }
    }
    Row {
      id: headerActions
      anchors.right: parent.right
      y: (root.barHeight - height) / 2
      spacing: Style.space(8)
      FieldInput {
        id: search
        visible: !root.empty
        width: Style.space(220)
        placeholder: "Find a computer"
        accessibleName: "Find a computer"
        hint: "/"
        onTextChanged: root.searchText = text
        onAccepted: root.focusDefault()
      }
        ActionButton {
          visible: !!root.host && !!root.host.shell && root.host.shell.standalone === true
          label: "Requests (" + (root.service ? root.service.requestItems.length : 0) + ")"
          Accessible.name: "Open pending requests"
          onClicked: if (root.host) root.host.openRequests()
        }
      ActionButton {
        id: connectAgent
        visible: !!root.service && root.service.connectPrompt !== ""
        label: "Connect an Agent"
        tooltipText: root.host && root.host.connectOpen ? "" : "The prompt that lets an AI agent use your computers"
        Accessible.name: "Connect an Agent: show the prompt to give an agent"
        onClicked: if (root.host) root.host.openConnect(connectAgent)
      }
      ActionButton {
        visible: !root.empty
        glyph: "+"
        label: "Add Computer"
        onClicked: if (root.host) root.host.showAdd()
      }
      ActionMenu {
        id: fleetActions
        visible: !root.empty
        label: "Fleet Actions ▾"
        accessibleName: "Fleet actions"
        tooltipText: "Actions for every computer"
        items: [
          { id: "theme", label: root.service && root.service.busy["theme-fleet"] ? "Applying Theme…" : "Apply Theme to Fleet",
            blocked: !!root.service && !!root.service.busy["theme-fleet"], reason: "ibara is applying it now." },
          { id: "update_ibara", label: root.updateAllAction === "update_ibara" ? "Updating ibara…" : (root.service && root.service.latestRelease && root.service.latestRelease.version ? "Update ibara on All to " + root.service.latestRelease.version + " (" + root.olderCount + " behind)…" : "Update ibara on All…"),
            blocked: root.updateAllBlocked !== "", reason: root.updateAllBlocked },
          { id: "update_omarchy", label: root.updateAllAction === "update_omarchy" ? "Updating Omarchy…" : "Update Omarchy on All…",
            blocked: (root.host ? root.host.updateAllPlan("update_omarchy").blocked : "Connecting.") !== "", reason: (root.host ? root.host.updateAllPlan("update_omarchy").blocked : "Connecting.") }
        ].concat(root.service && root.service.loginSettings ? [
          // Sync Logins…: every site Allowed on any computer becomes Allowed for All Computers, after a confirmation.
          { id: "sync_logins", label: root.service.busy["login-sync"] ? "Syncing Logins…" : "Sync Logins…",
            blocked: !root.service.loginSettings.enabled || !!root.service.busy["login-sync"],
            reason: !root.service.loginSettings.enabled ? "Turn on login sharing in Settings under Logins first." : "ibara is syncing them now." }] : [])
        onTriggered: id => {
          if (id === "theme" && root.service) root.service.applyThemeToFleet()
          else if (id === "sync_logins" && root.host) root.host.planSyncLogins(fleetActions.button)
          else if (root.host) root.host.confirmUpdateAll(id, fleetActions.button)
        }
      }
      ActionButton {
        glyph: "⚙"
        label: "Settings"
        role: "quiet"
        tooltipText: "ibara's settings on this computer (Ctrl+,)"
        onClicked: if (root.host) root.host.showSettings()
      }
      ActionButton {
        label: "✕"
        role: "quiet"
        Accessible.name: "Close console"
        tooltipText: "Close (Escape)"
        onClicked: if (root.host) root.host.requestClose()
      }
    }
    Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
  }

  // ---- body: the cards start right under the toolbar. Messages are toasts over the page.
  Item {
    id: body
    y: root.headerHeight + Style.space(8)
    width: parent.width
    height: parent.height - y

    // Nothing matches the search or filter: said in the wall's own space, where the cards would
    // be, with the way back to every card. Not a toast: it is what the wall shows, not news.
    Column {
      id: noMatch
      z: 1
      visible: root.computers.length > 0 && root.visibleComputers.length === 0
      anchors.centerIn: parent
      anchors.verticalCenterOffset: -Style.space(40)
      width: Math.min(parent.width, Style.space(520))
      spacing: Style.space(16)
      Copy {
        width: parent.width
        horizontalAlignment: Text.AlignHCenter
        text: root.searchText.trim() ? "No computers match “" + root.searchText.trim() + "”" + (root.filter === "all" ? "." : " in " + root.filterWords + ".")
          : root.filter === "attention" ? "No computers need attention."
          : root.filter === "use" ? "No computers are in use."
          : root.filter === "ready" ? "No computers are ready."
          : root.filter === "favorites" ? "No favorites yet. Open a computer and choose Favorite." : "No computers match."
        dimmed: true
      }
      ActionButton {
        id: showAllButton
        anchors.horizontalCenter: parent.horizontalCenter
        label: "Show All Computers"
        onClicked: { search.text = ""; root.filter = "all"; root.focusDefault() }
      }
    }

    // No computers yet: one line and one button, in the middle of the page.
    Item {
      visible: root.empty
      anchors.fill: parent
      Column {
        anchors.centerIn: parent
        anchors.verticalCenterOffset: -Style.space(40)
        width: Math.min(parent.width, Style.space(520))
        spacing: Style.space(16)
        IbaraMark { width: Style.space(56); height: width; color: Color.accent; anchors.horizontalCenter: parent.horizontalCenter }
        Copy {
          width: parent.width
          horizontalAlignment: Text.AlignHCenter
          text: root.missing === "package" ? "ibara's bar icon is here, but ibara itself isn't installed on this computer yet."
              + (root.service.ibaraInstallCommand ? "" : " Install the ibara package, then run ibara setup in a terminal.")
            : root.missing === "setup" ? "ibara is installed. One step is left: setting up this computer, which asks for your password once."
            : root.loaded ? "Add a computer to see its screen here and let your agents use it."
            : root.service && root.service.readErrors["directory"] ? String(root.service.readErrors["directory"]) : "Loading your computers…"
          dimmed: !root.loaded && !root.missing
        }
        ActionButton {
          id: installButton
          visible: root.missing === "setup" || (root.missing === "package" && !!root.service.ibaraInstallCommand)
          anchors.horizontalCenter: parent.horizontalCenter
          label: root.missing === "setup" ? "Finish Setting Up" : "Install ibara"
          tooltipText: root.missing === "setup" ? "Runs ibara setup in a terminal" : "Runs " + root.service.ibaraInstallCommand + " in a terminal"
          role: "primary"
          onClicked: if (root.service) root.service.installIbara()
        }
        ActionButton {
          id: firstAdd
          visible: root.loaded
          anchors.horizontalCenter: parent.horizontalCenter
          glyph: "+"
          label: "Add Your First Computer"
          role: "primary"
          onClicked: if (root.host) root.host.showAdd()
        }
      }
    }

    GridView {
      id: grid
      visible: !root.empty
      // ibara isn't running here: the cards show their last known state, greyed, until it runs.
      opacity: root.stopped ? 0.45 : 1
      enabled: !root.stopped
      // One gap to the right of every card, so the last column's edge lines up with the header's.
      // Large cards sit in the middle of the wall.
      width: root.large ? root.columns * (root.largeLayout.width + root.cardGap) : parent.width + root.cardGap
      x: root.large ? Math.floor((parent.width + root.cardGap - width) / 2) : 0
      // The grid reaches up into the gap under the toolbar, with the cards at rest where they
      // were, so a card that rises under the pointer stays whole, just below the toolbar's rule.
      y: -topMargin
      topMargin: Style.space(8)
      height: parent.height + topMargin
      clip: true
      activeFocusOnTab: true
      boundsBehavior: Flickable.StopAtBounds
      cellWidth: root.large ? root.largeLayout.width + root.cardGap : Math.floor(width / root.columns)
      cellHeight: Math.round((cellWidth - root.cardGap) / root.pictureAspect) + root.cardGap + root.cardTextHeight
      model: wallIds
      currentIndex: -1
      keyNavigationEnabled: true
      highlightFollowsCurrentItem: false
      onHeightChanged: root.publishVisibility()
      onContentYChanged: root.publishVisibility()
      Keys.onReturnPressed: root.openCurrent()
      Keys.onEnterPressed: root.openCurrent()
      // No add, move or displaced transitions: while one runs, the view drops any new place it
      // gives that card, so a size change then (a filter, the window settling) would leave the
      // cards stacked. Each card fades in and glides to a new place by itself instead.
      delegate: Item {
        id: cell
        width: grid.cellWidth
        height: grid.cellHeight
        GridView.onAdd: fadeIn.start()
        NumberAnimation { id: fadeIn; target: cell; property: "opacity"; from: 0; to: 1; duration: 300; easing.type: Easing.OutCubic }
        // A new order glides: a card the view puts in a new place is drawn where it was, then slides there.
        property real placedX: NaN
        property real placedY: NaN
        function glideFrom() {
          if (!isNaN(placedX)) {
            glide.x += placedX - x
            glide.y += placedY - y
            glideAnim.restart()
          }
          placedX = x
          placedY = y
        }
        onXChanged: glideFrom()
        onYChanged: glideFrom()
        transform: Translate { id: glide }
        ParallelAnimation {
          id: glideAnim
          NumberAnimation { target: glide; property: "x"; to: 0; duration: 550; easing.type: Easing.InOutCubic }
          NumberAnimation { target: glide; property: "y"; to: 0; duration: 550; easing.type: Easing.InOutCubic }
        }
        readonly property string computerId: String(model.computerId)
        readonly property var computer: root.computerById(computerId)
        readonly property string fleetState: root.tokens.stateOf(computer)
        readonly property bool current: grid.currentIndex === index
        readonly property bool actionsFocused: actionsLoader.activeFocus
        readonly property bool focused: (current && grid.activeFocus) || actionsFocused
        // A confirmation from this card's Join or its menu keeps the button it is attached to.
        readonly property bool confirming: !!root.host && !!root.host.confirmation && root.host.confirmation.subject === computerId
        // Set by the menu itself as it opens and closes: the loader that holds it depends on this,
        // so reading it through the loader would be a binding loop.
        property bool menuOpen: false
        readonly property bool showActions: focused || hover.hovered || confirming || menuOpen
        readonly property string blockedReason: root.host ? root.host.controlBlockedReason(computer) : "Unavailable"
        readonly property bool holding: !!root.host && root.host.holds(computerId)
        readonly property bool connecting: !!root.service && root.service.connectingOn(computerId)
        readonly property bool viewerOpen: holding && root.host.viewerOpen(computerId)
        // T, V or H (Console.handleKey) on the card with the keyboard: its Join, Open or
        // Close Viewer, or Hand Back, as if chosen there.
        function pressControl(control) { return focused && !!actionsLoader.item && actionsLoader.item.press(control) }
        // The one thing this card offers without hovering: Fix It for a repair ibara couldn't
        // make, Restart once Omarchy's update needs one to finish, Resume for a person's pause, or
        // Wake for a computer that is off or asleep.
        readonly property string standing: {
          var c = computer
          if (!c) return ""
          if (fleetState === "offline") return c.wake ? "wake" : ""
          if (c.needs_person && c.needs_person.fix) return "fix"
          if (StatusModel.restartNeeded(c)) return "restart"
          if (StatusModel.pauseAction(c) === "resume") return "resume"
          return ""
        }
        // The state the card shows: a Pause Agents or Resume sweep holds it back a little (noteStateChange).
        property string shownState: ""
        function showState(delay) {
          if (delay <= 0) { sweepClock.stop(); shownState = fleetState }
          else { sweepClock.interval = delay; sweepClock.restart() }
        }
        Timer { id: sweepClock; onTriggered: cell.shownState = cell.fleetState }
        // The view places a new card just after making it; that first place doesn't glide.
        Component.onCompleted: {
          shownState = fleetState
          Qt.callLater(() => { if (isNaN(cell.placedX)) { cell.placedX = cell.x; cell.placedY = cell.y } })
        }
        onFleetStateChanged: root.noteStateChange(cell)
        // An agent's first connection lights up the card it can use, once.
        Connections {
          target: root.service
          ignoreUnknownSignals: true
          function onAgentConnected(id, name) { if (id === cell.computerId) lightAnim.restart() }
        }
        HoverHandler { id: hover }
        Rectangle {
          id: card
          anchors.fill: parent
          anchors.rightMargin: root.cardGap
          anchors.bottomMargin: root.cardGap
          radius: 0
          color: root.tokens.surface
          // A 1 px rule at rest; focus and attention draw a thicker ring over the
          // picture's edge, so the picture (and the tile size it reports) keeps its size.
          border.width: 1
          border.color: hover.hovered ? Color.accent : root.tokens.rule
          Behavior on border.color { ColorAnimation { duration: 120 } }
          // Under the pointer the card rises a little.
          transform: Translate {
            y: hover.hovered ? -Style.space(6) : 0
            Behavior on y { NumberAnimation { duration: 120; easing.type: Easing.OutCubic } }
          }
          Accessible.role: Accessible.Button
          Accessible.name: root.tokens.label(cell.computer) + (root.favorites[cell.computerId] ? ", favorite" : "") + ", " + root.tokens.stateLabel(cell.fleetState, cell.computer) + ". " + root.tokens.activity(cell.computer, root.service ? root.service.nowMs : 0)
          MouseArea {
            anchors.fill: parent
            onClicked: { grid.currentIndex = index; if (root.host) root.host.showComputer(cell.computerId) }
          }
          Rectangle {
            id: previewBox
            x: card.border.width
            y: card.border.width
            width: parent.width - card.border.width * 2
            height: Math.round(width / root.pictureAspect)
            radius: 0
            color: Qt.alpha(Color.popups.text, 0.08)
            clip: true
            SteadyPreview {
              id: preview
              anchors.fill: parent
              transformOrigin: Item.Center
              incoming: root.tokens.frameSource(cell.computer, root.service && root.service.denied)
              drop: root.tokens.dropFrame(cell.computer)
              onShownSizeSettled: (width, height) => { if (root.service) root.service.notePreviewSize(root.large ? "selected" : "tile", width, height) }
              onFrameAspectChanged: if (root.visibleComputers.length === 1 && frameAspect > 0 && Math.abs(frameAspect - root.singleAspect) > 0.01) root.singleAspect = frameAspect
              LiveVideo {
                service: root.service
                computerId: cell.computerId
                preview: preview
                shown: !!root.service && root.service.consoleOpen && root.service.visibleComputerIds.indexOf(cell.computerId) !== -1
              }
            }
            PictureMoments {
              id: moments
              anchors.fill: parent
              service: root.service
              computerId: cell.computerId
              tokens: root.tokens
              preview: preview
              fleetState: cell.fleetState
              inView: cell.y + cell.height > grid.contentY && cell.y < grid.contentY + grid.height
              onArrived: arriveAnim.restart()
            }
            Copy {
              anchors.centerIn: parent
              width: parent.width - Style.space(24)
              visible: !preview.hasFrame && !moments.arriving
              horizontalAlignment: Text.AlignHCenter
              text: cell.computer && cell.computer.frame_error ? String(cell.computer.frame_error) : cell.fleetState === "offline" ? "No picture" : "Waiting for a picture"
              dimmed: true
              font.pixelSize: Style.font.bodySmall
            }
            Rectangle {
              x: Style.space(8)
              y: Style.space(8)
              width: tagRow.implicitWidth + Style.space(14)
              height: tagRow.implicitHeight + Style.space(8)
              radius: 0
              color: Qt.alpha(Color.popups.background, 0.88)
              Row {
                id: tagRow
                anchors.centerIn: parent
                spacing: Style.space(7)
                StateMarker { tokens: root.tokens; fleetState: cell.shownState; anchors.verticalCenter: parent.verticalCenter }
                Copy {
                  anchors.verticalCenter: parent.verticalCenter
                  text: cell.connecting ? "Connecting…" : ((root.service ? root.service.turnWords(cell.computerId) : "") || root.tokens.stateLabel(cell.shownState, cell.computer)) + (cell.computer.version ? " · ibara " + cell.computer.version : "")
                  color: root.tokens.textTint(root.tokens.stateColor(cell.shownState))
                  font.pixelSize: Style.font.bodySmall
                }
              }
            }
            // A favorite shows a star; the computer view's header sets it.
            Rectangle {
              anchors.right: parent.right
              anchors.top: parent.top
              anchors.margins: Style.space(8)
              visible: !!root.favorites[cell.computerId]
              width: star.implicitWidth + Style.space(12)
              height: tagRow.implicitHeight + Style.space(8)
              radius: 0
              color: Qt.alpha(Color.popups.background, 0.88)
              Copy { id: star; anchors.centerIn: parent; text: "★"; font.pixelSize: Style.font.bodySmall; Accessible.ignored: true }
            }
            // Files dropped on the card go to this computer; the bar shows them on their way.
            DropProgress {
              id: dropBar
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.bottom: parent.bottom
              service: root.service
              computerId: cell.computerId
            }
            // Clicking the card opens the computer. Its standing action (Fix It, Resume or Wake)
            // always shows, and so do Hand Back and Open Viewer while you hold control; Take
            // Control and More show on hover or keyboard focus. The grid remembers this loader as
            // its focus, so Tab from the grid reaches the buttons and Shift+Tab returns.
            Loader {
              id: actionsLoader
              active: cell.showActions || cell.standing !== "" || cell.holding || !!(root.service && root.service.connectingOn(cell.computerId))
              anchors.left: parent.left
              anchors.bottom: dropBar.visible ? dropBar.top : parent.bottom
              anchors.margins: Style.space(8)
              sourceComponent: Rectangle {
                width: actionsRow.implicitWidth
                height: actionsRow.implicitHeight
                radius: 0
                color: Qt.alpha(Color.popups.background, 0.88)
                function press(control) {
                  var button = control === "take" ? takeControl : control === "viewer" ? viewerButton : control === "handback" ? handBack : null
                  if (!button || !button.visible) return false
                  button.forceActiveFocus()
                  button.clicked()
                  return true
                }
                Row {
                  id: actionsRow
                  spacing: Style.space(4)
                  ActionButton {
                    id: standingButton
                    visible: cell.standing !== ""
                    readonly property var kind: ({
                      fix: { busyKey: "repair:", label: "Fix It", busyLabel: "Fixing…" },
                      restart: { busyKey: "power:", label: "Restart", busyLabel: "Restarting…", tip: "Omarchy updated it; a restart finishes the update" },
                      resume: { busyKey: "pause:", label: "Resume", busyLabel: "Resuming…", tip: "Let its agents work again" },
                      wake: { busyKey: "wake:", label: "Wake", busyLabel: "Waking…", tip: "Send it the signal to turn on" }
                    })[cell.standing] || ({})
                    readonly property bool busy: !!root.service && !!kind.busyKey && !!root.service.busy[kind.busyKey + cell.computerId]
                    label: (busy ? kind.busyLabel : kind.label) || ""
                    role: "primary"
                    size: "small"
                    blocked: busy
                    tooltipText: cell.standing === "fix" && cell.computer && cell.computer.needs_person ? StatusModel.fixDescription(cell.computer.needs_person.fix) : kind.tip || ""
                    Accessible.name: label + " " + root.tokens.label(cell.computer)
                    onClicked: {
                      if (blocked || !root.service) return
                      if (cell.standing === "fix") root.service.repair(cell.computerId, cell.computer.needs_person.fix)
                      else if (cell.standing === "restart") { if (root.host) root.host.confirmPower(cell.computerId, "restart", standingButton) }
                      else if (cell.standing === "resume") root.service.resumeAgents(cell.computerId)
                      else root.service.wake(cell.computerId)
                    }
                  }
                  TakeControlButton {
                    id: takeControl
                    service: root.service
                    host: root.host
                    computerId: cell.computerId
                    visible: (!cell.holding && cell.showActions || connecting) && StatusModel.computerState(cell.computer) !== "offline"
                    keyHint: cell.focused ? " (T)" : ""
                    role: cell.standing !== "" ? "secondary" : "primary"
                    size: "small"
                    focus: !cell.holding
                    // Why it is unavailable is a toast when chosen, never a tooltip over the cards.
                    tooltipText: ""
                    Accessible.name: "Join " + root.tokens.label(cell.computer)
                  }
                  // While you hold control: Open Viewer (Close Viewer while it is open), then Hand
                  // Back in Join's place.
                  ActionButton {
                    id: viewerButton
                    visible: cell.holding && !cell.connecting && (cell.viewerOpen || StatusModel.computerState(cell.computer) !== "offline")
                    readonly property string word: cell.viewerOpen ? "Close Viewer" : "Open Viewer"
                    label: word + (cell.focused ? " (V)" : "")
                    size: "small"
                    blocked: !cell.viewerOpen && cell.blockedReason !== ""
                    disabledReason: blocked ? cell.blockedReason : ""
                    tooltipText: ""
                    Accessible.name: word + " " + root.tokens.label(cell.computer)
                    onClicked: {
                      if (!root.host) return
                      if (blocked) root.host.notify(cell.blockedReason, false)
                      else root.host.toggleViewer(cell.computerId)
                    }
                  }
                  ActionButton {
                    id: handBack
                    visible: cell.holding
                    label: "Hand Back" + (cell.focused ? " (H)" : "")
                    role: cell.standing !== "" ? "secondary" : "primary"
                    size: "small"
                    focus: cell.holding
                    blocked: !!(root.service && root.service.mutating)
                    disabledReason: blocked ? "Wait for the current action to finish." : ""
                    tooltipText: ""
                    Accessible.name: "Hand Back " + root.tokens.label(cell.computer)
                    onClicked: {
                      if (!root.host) return
                      if (blocked) root.host.notify(disabledReason, false)
                      else root.host.handBack(cell.computerId)
                    }
                  }
                  ActionMenu {
                    id: more
                    visible: cell.showActions
                    size: "small"
                    role: "secondary"
                    label: "⋯"
                    accessibleName: "More for " + root.tokens.label(cell.computer)
                    tooltipText: "More: power, pause, settings and Remove Computer"
                    items: root.cardMenu(cell.computer, cell.fleetState)
                    onTriggered: id => root.cardAction(cell.computerId, id, more.button)
                    onOpenedChanged: cell.menuOpen = opened
                  }
                }
              }
            }
            // The sentence a repair left for a person, over the picture where it can be read whole.
            Rectangle {
              visible: !!cell.computer && !!cell.computer.needs_person && cell.fleetState !== "offline"
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.bottom: actionsLoader.active ? actionsLoader.top : dropBar.visible ? dropBar.top : parent.bottom
              anchors.margins: Style.space(8)
              height: needsText.implicitHeight + Style.space(10)
              radius: 0
              color: Qt.alpha(Color.popups.background, 0.92)
              border.width: 1
              border.color: Qt.alpha(Color.urgent, 0.7)
              Copy {
                id: needsText
                x: Style.space(8)
                width: parent.width - Style.space(16)
                anchors.verticalCenter: parent.verticalCenter
                text: cell.computer && cell.computer.needs_person ? String(cell.computer.needs_person.message) : ""
                color: root.tokens.textTint(root.tokens.attentionColor)
                font.pixelSize: Style.font.bodySmall
                maximumLineCount: 3
                elide: Text.ElideRight
              }
            }
          }
          // Files dragged over the card: drop them to send them to this computer.
          DropArea {
            id: cardDrop
            anchors.fill: parent
            keys: ["text/uri-list"]
            onDropped: function(drop) {
              if (!drop.hasUrls || !root.service) return
              root.service.dropFiles(cell.computerId, drop.urls.map(function(url) { return String(url) }))
              drop.acceptProposedAction()
            }
          }
          Rectangle {
            anchors.fill: parent
            visible: cardDrop.containsDrag
            z: 5
            radius: 0
            color: Qt.alpha(Color.accent, 0.14)
            border.width: Math.max(2, Style.space(2))
            border.color: Color.accent
            Copy { anchors.centerIn: parent; text: "Drop to send to " + root.tokens.label(cell.computer); font.bold: true }
          }
          Item {
            anchors.top: previewBox.bottom
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            anchors.leftMargin: Style.space(10)
            anchors.rightMargin: Style.space(10)
            Copy {
              id: nameText
              anchors.left: parent.left
              anchors.right: actorText.left
              anchors.rightMargin: Style.space(8)
              y: Style.space(6)
              text: root.tokens.label(cell.computer)
              font.bold: true
              font.pixelSize: Style.font.title
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            Copy {
              id: actorText
              anchors.right: parent.right
              anchors.baseline: nameText.baseline
              width: Math.min(implicitWidth, parent.width * 0.5)
              text: root.tokens.actor(cell.computer)
              color: root.tokens.foreground
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            // The step the agent is on, while it works, sliding up over the last; otherwise the activity line.
            SlideText {
              readonly property var step: cell.fleetState === "working" && cell.computer && cell.computer.active_task ? cell.computer.active_task.last_step : null
              readonly property string stepText: step ? String(step.summary || "") : ""
              anchors.top: nameText.bottom
              anchors.topMargin: Style.space(3)
              width: parent.width
              rolls: stepText !== ""
              textFormat: Text.StyledText
              accessibleText: stepText || root.tokens.activity(cell.computer, root.service ? root.service.nowMs : 0)
              text: stepText ? root.tokens.stepMarkup(step, false) : root.tokens.activityMarkup(cell.computer, root.service ? root.service.nowMs : 0)
              color: root.tokens.foreground
              size: Style.font.bodySmall
            }
          }
          Rectangle {
            anchors.fill: parent
            visible: cell.focused
            radius: 0
            color: "transparent"
            border.width: Math.max(2, Style.space(2))
            border.color: Color.accent
          }
          // Needs Attention: a slow red pulse on the border, the only red motion on the wall.
          Rectangle {
            id: attentionPulse
            readonly property bool pulsing: cell.fleetState === "attention" && !cell.focused
            anchors.fill: parent
            visible: pulsing
            radius: 0
            color: "transparent"
            border.width: Style.space(3)
            border.color: Color.urgent
            SequentialAnimation on opacity {
              running: attentionPulse.pulsing && root.visible
              loops: Animation.Infinite
              alwaysRunToEnd: true
              NumberAnimation { to: 0.2; duration: 800; easing.type: Easing.InOutSine }
              NumberAnimation { to: 1; duration: 800; easing.type: Easing.InOutSine }
            }
          }
          // Connected: the card lights up once in the accent color.
          Rectangle {
            id: lit
            anchors.fill: parent
            visible: opacity > 0
            radius: 0
            color: Qt.alpha(Color.accent, 0.18)
            border.width: Style.space(3)
            border.color: Color.accent
            opacity: 0
            SequentialAnimation {
              id: lightAnim
              NumberAnimation { target: lit; property: "opacity"; from: 0; to: 1; duration: 250; easing.type: Easing.OutCubic }
              PauseAnimation { duration: 500 }
              NumberAnimation { target: lit; property: "opacity"; to: 0; duration: 900; easing.type: Easing.InOutQuad }
            }
          }
          // New to the fleet: a ring in the accent color holds while its picture comes up, then fades.
          Rectangle {
            id: arriveRing
            anchors.fill: parent
            visible: opacity > 0
            radius: 0
            color: "transparent"
            border.width: Style.space(3)
            border.color: Color.accent
            opacity: 0
            SequentialAnimation {
              id: arriveAnim
              NumberAnimation { target: arriveRing; property: "opacity"; from: 0; to: 1; duration: 250; easing.type: Easing.OutCubic }
              PauseAnimation { duration: 2200 }
              NumberAnimation { target: arriveRing; property: "opacity"; to: 0; duration: 900; easing.type: Easing.InOutQuad }
            }
          }
        }
      }
    }
  }
  // A card's More menu: pause or resume its agents, power, and its settings; this computer's
  // card also offers Share This Computer. What can't be undone asks first, beside the menu's button.
  function cardMenu(c, state) {
    if (!c) return []
    var offline = state === "offline", reason = offline ? tokens.label(c) + " isn't answering." : ""
    var items = []
    var pause = StatusModel.pauseAction(c)
    if (pause === "resume") items.push({ id: "resume", label: "Resume" })
    else if (pause === "pause") items.push({ id: "pause", label: "Pause Agents" })
    if (c.active_task_ref) items.push({ id: "force-release", label: "Stop agent & release…", danger: true, blocked: offline, reason: reason })
    if (service && !service.holdsControlOn(c.computer_id)) items.push({ id: "take-over", label: "Take over now…", danger: true, blocked: offline, reason: reason })
    if (offline && c.wake) items.push({ id: "wake", label: "Wake" })
    items.push({ id: "restart", label: "Restart…", danger: true, blocked: offline, reason: reason })
    items.push({ id: "shutdown", label: "Shut Down…", danger: true, blocked: offline, reason: reason })
    items.push({ id: "sleep", label: "Sleep…", danger: true, blocked: offline, reason: reason })
    if (service && service.thisComputerId && String(c.computer_id) === service.thisComputerId) items.push({ id: "share", label: "Share This Computer" })
    items.push({ id: "settings", label: "Settings" })
    items.push({ id: "remove", label: "Remove Computer…", danger: true })
    return items
  }
  function cardAction(id, action, anchor) {
    if (!service || !host) return
    if (action === "pause") service.pauseAgents(id)
    else if (action === "resume") service.resumeAgents(id)
    else if (action === "force-release" || action === "take-over") {
      var current = service.sessions[id]
      if (!current) return
      var epoch = current.controller_epoch, revision = current.ownership_revision
      var endpoint = current.endpoint_id, binding = current.binding_revision, authorization = current.authorization_generation
      var task = String(current.active_task_ref || ""), owner = current.owner_name
      var takeover = action === "take-over"
      host.askConfirm({ anchor: anchor, subject: id, danger: true,
        message: (takeover ? "Take control of " : "Stop the agent on ") + tokens.label(current) + "? This cancels its active task and managed commands. Unsaved task work may be lost. " + (takeover ? "Another person's control will also end." : "The computer becomes available after cleanup."),
        confirmLabel: takeover ? "Take over now" : "Stop & release",
        valid: function() { var live = root.service.sessions[id]; return !!live && live.endpoint_id === endpoint && live.binding_revision === binding && live.authorization_generation === authorization && live.controller_epoch === epoch && live.ownership_revision === revision && live.owner_name === owner && String(live.active_task_ref || "") === task },
        run: function() { if (takeover) root.service.takeControlFor(id, true); else root.service.releaseComputerTask(id, task) }
      })
    }
    else if (action === "wake") service.wake(id)
    else if (action === "settings") host.showComputer(id, "settings")
    else if (action === "share") host.showShare()
    else if (action === "remove") host.confirmRemove(id, anchor)
    else host.confirmPower(id, action, anchor)
  }
  function openCurrent() {
    if (grid.currentIndex >= 0 && grid.currentIndex < visibleComputers.length && host)
      host.showComputer(String(visibleComputers[grid.currentIndex].computer_id))
  }
}
