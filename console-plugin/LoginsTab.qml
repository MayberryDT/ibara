import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// The open computer's logins, as the sharing computer keeps them (`login-rows`): one row per site
// with a rule for it here or for All Computers. Each shows its rule in the Access tab's menu
// (Allowed, Ask First, Denied: a choice applies to this computer at once, with Undo), the task
// that first asked, when it was last shared and how that went, Share With… (other computers, or
// All Computers, fresh from your browser) and Remove (its cookies there go, and its rule, so the
// next request asks again). Read when the tab opens, every 10 s while it shows and after each change.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  readonly property Tokens tokens: Tokens {}
  readonly property string computerLabel: tokens.label(computer)
  readonly property var settings: service ? service.loginSettings : null
  readonly property var standing: settings && settings.computers[computerId] ? settings.computers[computerId] : null
  readonly property bool sharing: !!settings && settings.enabled
  readonly property string elsewhere: service ? service.loginSourceElsewhere(null, computerId) : ""
  readonly property bool current: !!service && service.loginRowsComputerId === computerId
  readonly property var rows: current ? service.loginRows.rows : []
  readonly property var deniedAll: current ? service.loginRows.deniedAll.filter(function(site) { return !root.rows.some(function(r) { return r.site === site }) }) : []
  readonly property string readError: current ? service.loginRowsError : ""
  readonly property bool reading: !!service && !!service.busy["login-rows"]
  // Why nothing here can change now, or "".
  readonly property string blockedReason: !service ? "Connecting." : service.denied ? "You don't have access." : !sharing ? "Login sharing is off on this computer." : ""
  // What the page says in place of its rows, or under them.
  readonly property string standingText: !settings ? "ibara on this computer can't share logins. Update ibara here to share logins with your computers."
    : standing && standing.older ? computerLabel + " runs an older ibara. Update ibara there to share logins with it."
    : elsewhere ? "Logins for " + computerLabel + " come from " + elsewhere + ". Change them on " + elsewhere + "'s console."
    : !sharing ? "Login sharing is off. Turn it on in the Logins page to share your logins with " + computerLabel + "."
    : ""
  // Your other computers for Share With… (a computer shared with a friend is never listed).
  readonly property var shareTargets: {
    if (!settings) return []
    var out = []
    for (var id in settings.computers) {
      if (id === computerId || !service.sessions[id]) continue
      var c = settings.computers[id]
      var name = service.computerLabelFor(id), pinned = service.loginSourceElsewhere(null, id)
      out.push({ id: id, label: name, blocked: c.older || pinned !== "",
        reason: c.older ? name + " runs an older ibara. Update ibara there first." : pinned ? name + " takes its logins from " + pinned + "." : "" })
    }
    return out.sort(function(a, b) { return a.label.localeCompare(b.label) })
  }

  function reload() { if (service && visible && computerId && settings && sharing) service.loadLoginRows(computerId) }
  onVisibleChanged: reload()
  onComputerIdChanged: { pendingRule = null; search.text = ""; filter.value = "all"; reload() }
  onSharingChanged: reload()
  function focusDefault() { search.focusInput() }
  property alias searchText: search.text
  function focusSearch() { search.focusInput() }
  function findSite(site) { filter.value = "all"; search.text = site }
  readonly property var filteredRows: rows.filter(function(row) { return row.site.indexOf(search.text.trim().toLowerCase()) !== -1 && (filter.value === "all" || row.rule === filter.value) })
  // The keyboard stays put while a menu or confirmation is open here: reading again would rebuild the rows.
  property int menusOpen: 0
  Timer {
    interval: 10000
    repeat: true
    running: root.visible && !!root.service && root.service.consoleOpen && root.sharing && root.menusOpen === 0 && !(root.host && root.host.confirmation)
    onTriggered: root.reload()
  }

  // ---- a rule changes at once; once ibara confirms it, a message offers Undo (as on the Access tab).
  property var pendingRule: null
  function ruleWord(rule) { return rule === "allow" ? "Allowed" : rule === "deny" ? "Denied" : "Ask First" }
  function choose(row, rule) {
    if (!service || blockedReason) return
    var previous = row.ownRule || "ask"
    pendingRule = { computerId: computerId, site: row.site, rule: rule, previous: previous }
    service.setLoginRule(computerId, row.site, rule)
  }
  Connections {
    target: root.service
    ignoreUnknownSignals: true
    function onLoginRuleSettled(computerId, site, rule, ok) {
      var change = root.pendingRule
      if (!change || change.computerId !== computerId || change.site !== site || change.rule !== rule) return
      root.pendingRule = null
      if (!ok || !root.host) return
      var text = site + " on " + root.computerLabel + " is now " + root.ruleWord(rule) + "."
      if (change.undoing) { root.host.notify(text, false); return }
      root.host.offerUndo(text, function() {
        root.pendingRule = { computerId: change.computerId, site: site, rule: change.previous, previous: rule, undoing: true }
        root.service.setLoginRule(change.computerId, site, change.previous)
      })
    }
  }

  Column {
    id: column
    width: parent.width
    spacing: Style.space(6)
      Row {
        width: parent.width
        spacing: Style.space(10)
        FieldInput { id: search; width: Style.space(320); placeholder: "Find a site"; accessibleName: "Find a login site" }
        ButtonGroup {
          id: filter
          options: [{value: "all", label: "All"}, {value: "allow", label: "Allowed"}, {value: "ask", label: "Ask First"}, {value: "deny", label: "Denied"}]
          value: "all"
          onChanged: value => filter.value = value
        }
        ActionButton {
          id: refreshButton
          label: "Refresh"
          blocked: !root.sharing
          disabledReason: root.blockedReason
          onClicked: if (!blocked) { root.service.loadLoginSettings(); root.reload() }
        }
        DataAge {
          anchors.verticalCenter: parent.verticalCenter
          at: root.current ? root.service.loginRowsAt : 0
          refreshing: root.reading
          nowMs: root.service ? root.service.nowMs : Date.now()
        }
      }
      Copy {
        visible: text !== ""
        width: parent.width
        text: root.standingText || (root.readError ? StatusModel.clip(root.readError, 200)
          : root.rows.length === 0 ? (root.reading || !root.current || !root.service.loginRowsAt ? "Loading…" : "No sites yet. A site shows here once an agent on " + root.computerLabel + " asks for its login, or once you share one with it.") : "")
        color: root.readError ? root.tokens.textTint(root.tokens.attentionColor) : root.standingText ? root.tokens.textTint(root.tokens.pausedColor) : root.tokens.dim
        font.pixelSize: Style.font.bodySmall
      }
      ActionButton {
        visible: !!root.settings && !root.sharing && !root.elsewhere && !!root.host
        label: "Manage Logins"
        size: "small"
        onClicked: root.host.showLogins()
      }

  }
  ListView {
    id: list
    y: column.height + Style.space(8)
    width: parent.width
    height: parent.height - y
    clip: true
    boundsBehavior: Flickable.StopAtBounds
    model: root.standingText ? [] : root.filteredRows
    delegate: Rectangle {
          id: row
          required property var modelData
          readonly property var entry: modelData
          property bool detailsOpen: false
          readonly property bool neverShared: entry.allRule === "deny"
          readonly property bool focusInside: menu.opened || shareWith.opened || removeButton.activeFocus
          width: list.width
          height: Math.max(Style.space(40), info.implicitHeight + Style.space(10))
          radius: 0
          color: focusInside ? Qt.alpha(Color.accent, 0.08) : rowHover.hovered ? Qt.alpha(Color.popups.text, 0.05) : "transparent"
          Accessible.role: Accessible.StaticText
          Accessible.name: entry.site + ", " + root.ruleWord(entry.rule)
          Component.onDestruction: { if (menu.opened) root.menusOpen--; if (shareWith.opened) root.menusOpen-- }
          HoverHandler { id: rowHover }
          Rectangle { anchors.bottom: parent.bottom; width: parent.width; height: 1; radius: 0; color: root.tokens.rule }

          Column {
            id: info
            x: Style.space(10)
            anchors.verticalCenter: parent.verticalCenter
            width: buttons.x - x - Style.space(10)
            spacing: Style.space(2)
            Copy { width: parent.width; text: row.entry.site + " · " + (row.entry.allRule && !row.entry.ownRule ? "All Computers" : root.computerLabel); font.bold: true; wrapMode: Text.NoWrap; elide: Text.ElideRight }
            Copy {
              width: parent.width
              textFormat: Text.StyledText
              text: row.neverShared ? root.tokens.ink("Never shared: ", root.tokens.dim) + root.tokens.ink("Denied", root.tokens.textTint(root.tokens.attentionColor)) + root.tokens.ink(" for All Computers", root.tokens.dim)
                : !row.entry.ownRule && row.entry.allRule ? root.tokens.ink(root.ruleWord(row.entry.allRule), root.tokens.textTint(root.tokens.ruleColor(row.entry.allRule))) + root.tokens.ink(" for All Computers", root.tokens.dim) : ""
              visible: row.detailsOpen && text !== ""
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            Copy {
              visible: row.detailsOpen && row.entry.firstGoal !== ""
              width: parent.width
              textFormat: Text.StyledText
              text: root.tokens.ink("First asked for “", root.tokens.dim) + root.tokens.ink(row.entry.firstGoal, root.tokens.foreground) + root.tokens.ink("”", root.tokens.dim)
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
            Copy {
              width: parent.width
              visible: row.detailsOpen
              readonly property string resultWhen: row.entry.lastResultAt ? " (" + root.tokens.changedLabel(new Date(row.entry.lastResultAt).toISOString(), root.service.nowMs) + ")" : ""
              textFormat: Text.StyledText
              text: root.tokens.ink(row.entry.lastShared ? "Last shared " + root.tokens.changedLabel(new Date(row.entry.lastShared).toISOString(), root.service.nowMs) : "Not shared yet", root.tokens.dim) +
                (row.entry.lastResult === "worked" ? root.tokens.ink(" · ", root.tokens.dim) + root.tokens.ink("Worked", root.tokens.textTint(root.tokens.readyColor)) + root.tokens.ink(resultWhen, root.tokens.dim)
                : row.entry.lastResult === "site_rejected" ? root.tokens.ink(" · ", root.tokens.dim) + root.tokens.ink("The site asked to sign in again", root.tokens.textTint(root.tokens.attentionColor)) + root.tokens.ink(resultWhen, root.tokens.dim) + root.tokens.ink(": use Join", root.tokens.foreground) : "")
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }
          }
          Row {
            id: buttons
            anchors.right: parent.right
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(6)
            ActionButton {
              label: row.detailsOpen ? "Less" : "Details"
              size: "small"
              role: "quiet"
              Accessible.name: "Details for " + row.entry.site
              onClicked: row.detailsOpen = !row.detailsOpen
            }
            RuleMenu {
              id: menu
              anchors.verticalCenter: parent.verticalCenter
              rule: row.entry.rule
              blocked: root.blockedReason !== "" || row.neverShared || !!root.service.busy["login-rule:" + root.computerId + ":" + row.entry.site]
              disabledReason: root.blockedReason || (row.neverShared ? "Never Share is on for " + row.entry.site + " on every computer. Change it in the Logins page." : "")
              accessibleName: row.entry.site + " on " + root.computerLabel
              onChosen: function(value) { root.choose(row.entry, value) }
              onOpenedChanged: root.menusOpen += opened ? 1 : -1
            }
            ShareWithMenu {
              id: shareWith
              anchors.verticalCenter: parent.verticalCenter
              computers: root.shareTargets
              accessibleName: "Share the login for " + row.entry.site + " with…"
              blocked: root.blockedReason !== "" || row.neverShared || !!root.service.busy["login-share:" + row.entry.site]
              disabledReason: root.blockedReason || (row.neverShared ? row.entry.site + " is never shared." : root.service.busy["login-share:" + row.entry.site] ? "ibara is sharing it now." : "")
              onShared: to => root.service.shareLoginWith(row.entry.site, to)
              onOpenedChanged: root.menusOpen += opened ? 1 : -1
            }
            ActionButton {
              id: removeButton
              anchors.verticalCenter: parent.verticalCenter
              size: "small"
              role: "danger"
              label: root.service.busy["login-remove:" + root.computerId + ":" + row.entry.site] ? "Removing…" : "Remove"
              blocked: root.blockedReason !== "" || !!root.service.busy["login-remove:" + root.computerId + ":" + row.entry.site]
              disabledReason: root.blockedReason
              tooltipText: disabledReason || "Sign " + root.computerLabel + " out of " + row.entry.site + " and remove its rule here"
              Accessible.name: "Remove the login for " + row.entry.site + " from " + root.computerLabel
              onClicked: if (!blocked && root.host) root.host.confirmRemoveLogin(root.computerId, row.entry.site, removeButton)
            }
          }
        }
    footer: Copy {
      width: list.width
      visible: root.deniedAll.length > 0
      text: root.deniedAll.length + " sites denied for All Computers. Manage them on the Logins page."
      dimmed: true
      font.pixelSize: Style.font.bodySmall
    }
    Copy { visible: list.count === 0 && root.rows.length > 0 && !root.standingText; text: "No sites match."; dimmed: true }
  }
}
