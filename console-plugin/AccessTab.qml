import QtQuick
import qs.Commons
import qs.Ui
import "StatusModel.js" as StatusModel

// Who can use this computer and what each may do. Each cell is a compact choice of Allowed, Ask
// First or Denied that applies as soon as it is chosen, with Undo in a message. Removing your own
// Administer permission, removing access and removing a pairing ask first, right beside the click.
// Each agent and computer also has Ask before it sends, spends or deletes: its own rules for
// those kinds of step. Ask First on an agent or its computer wins over Allowed; a kind neither
// sets follows this computer's Settings tab for agents from your own computers, and asks for others.
Item {
  id: root
  property var host: null
  property var service: null
  property string computerId: ""
  property var computer: null
  readonly property Tokens tokens: Tokens {}
  readonly property var access: service ? service.accessTable : null
  readonly property var rows: access && Array.isArray(access.rows) ? access.rows : []
  readonly property bool canAdminister: !!access && access.can_administer === true
  readonly property bool editable: canAdminister && !!service && !service.mutating
  readonly property string editReason: !access ? "" : !canAdminister ? "You can't change access on " + computerLabel + "." : service && service.mutating ? "Wait for the current action to finish." : ""
  readonly property var capabilities: [
    {key: "watch", label: "Watch"}, {key: "files", label: "Files"}, {key: "control", label: "Join"},
    {key: "agents", label: "Agent Tasks"}, {key: "administer", label: "Administer"}
  ]
  readonly property var effectClasses: [
    {key: "observe", label: "Observe"}, {key: "change", label: "Change"}, {key: "send", label: "Send"},
    {key: "spend", label: "Spend"}, {key: "destructive", label: "Destructive"}, {key: "access", label: "Access"}
  ]
  readonly property string computerLabel: tokens.label(computer)
  // This computer's own identity on the one open: the directory's operator name for it.
  readonly property string ownPrincipal: computer && computer.operator_principal ? String(computer.operator_principal) : ""

  // ---- the table's geometry: every column as wide as its widest text, never wider.
  readonly property real cellGap: Style.space(12)
  readonly property real rowHeight: ruleProbe.implicitHeight + Style.space(8)
  readonly property real identityWidth: {
    var widest = headerMetrics.advanceWidth("Who")
    for (var i = 0; i < rows.length; i++) widest = Math.max(widest, identityMetrics.advanceWidth(identityLabel(rows[i])) + Style.space(36))
    return Math.ceil(widest) + cellGap
  }
  function columnWidth(label) { return Math.ceil(Math.max(headerMetrics.advanceWidth(label.toUpperCase()) + label.length * Style.space(1), ruleProbe.implicitWidth)) + cellGap }
  readonly property real tableWidth: {
    var total = identityWidth
    for (var i = 0; i < capabilities.length; i++) total += columnWidth(capabilities[i].label)
    return total
  }
  FontMetrics { id: headerMetrics; font.family: Style.font.family; font.pixelSize: Style.font.caption }
  FontMetrics { id: identityMetrics; font.family: Style.font.family; font.pixelSize: Style.font.bodySmall }
  RuleMenu { id: ruleProbe; visible: false }

  // Someone else's computer reads as theirs: `alex@example.com's Bench`. The computer you are on
  // reads "You (riley)", as under Who can use it on the Screen tab.
  function identityLabel(row) {
    if (row.subject === "owner") return computerLabel + "'s owner"
    if (row.owner) return String(row.owner) + "'s " + String(row.computer_name || row.subject)
    return isOwn(row) ? "You (" + String(row.subject) + ")" : String(row.subject)
  }
  function isOwn(row) { return !!ownPrincipal && row.kind === "computer" && String(row.subject) === ownPrincipal }
  function ruleLabel(rule) { return rule === "allow" ? "Allowed" : rule === "ask" ? "Ask First" : "Denied" }
  function capabilityLabel(key) {
    for (var i = 0; i < capabilities.length; i++) if (capabilities[i].key === key) return capabilities[i].label
    return String(key)
  }
  function effectLabel(key) {
    for (var i = 0; i < effectClasses.length; i++) if (effectClasses[i].key === key) return effectClasses[i].label
    return String(key)
  }
  // Send, spend and delete: the kinds Ask before agents send, spend or delete covers.
  readonly property var askedKinds: ["send", "spend", "destructive"]
  // This computer's own Ask before agents send, spend or delete, as its access table says.
  readonly property bool computerAsks: !access || access.ask_first !== false
  function asksFirst(row) { return askedKinds.some(function(k) { return String(row.effects[k] || "deny") === "ask" }) }
  function askable(row) { return askedKinds.filter(function(k) { return String(row.effects[k] || "deny") !== "deny" }) }
  // The rules the row's own grant sets. A computer on an older ibara lists only the rules its
  // steps follow; those are written back as they are, as before.
  function ownEffects(row) { return Object.assign({}, row.own_effects || row.effects || {}) }
  function setsAsking(row) { var own = row.own_effects || {}; return askedKinds.some(function(k) { return own[k] !== undefined }) }
  // "codex@lumen asks …" for an agent, "vesper's agents ask …" for a computer's.
  function said(row, one, many) { return row.kind === "computer" ? String(row.subject) + "'s agents " + many : String(row.subject) + " " + one }
  // The switch: every kind of those not denied asks first (on), or runs without asking (off).
  function setAsking(row, on) {
    var kinds = askable(row)
    if (!editable || !kinds.length) return
    var previous = ownEffects(row), effects = ownEffects(row)
    kinds.forEach(function(k) { effects[k] = on ? "ask" : "allow" })
    changeOwnEffects(row, effects, previous, kinds[0], on
      ? said(row, "asks", "ask") + " you before sending, spending or deleting on " + computerLabel + "."
      : said(row, "now sends, spends and deletes", "now send, spend and delete") + " on " + computerLabel + " without asking you.")
  }
  // Back to the computer's rules and this computer's Settings tab for those kinds.
  function followComputer(row) {
    if (!editable) return
    var previous = ownEffects(row), effects = ownEffects(row)
    askedKinds.forEach(function(k) { delete effects[k] })
    changeOwnEffects(row, effects, previous, "", said(row, "follows", "follow") + " " + computerLabel + "'s Settings tab again for sending, spending and deleting.")
  }
  function changeOwnEffects(row, effects, previous, key, text) {
    var rule = String(row.capabilities.agents || "deny")
    applyChange({ computerId: computerId, subject: String(row.subject), capability: "agents", rule: rule, undoRule: rule,
      effects: effects, undoEffects: previous, effectKey: key, text: text,
      undoText: String(row.subject) + "'s rules for sending, spending and deleting on " + computerLabel + " are back as they were." })
  }
  function rowFor(subject) {
    for (var i = 0; i < rows.length; i++) if (rows[i].subject === subject) return rows[i]
    return null
  }

  property string selectedSubject: ""
  function reload() { if (visible && computerId && service) service.loadAccess() }
  onVisibleChanged: reload()
  onComputerIdChanged: { selectedSubject = ""; pendingChange = null; reload() }

  // ---- changing a rule. It applies at once; once ibara confirms it, a message offers Undo.
  // pendingChange: { computerId, subject, capability, rule, effects?, undoRule, undoEffects?, text }
  property var pendingChange: null
  function applyChange(change) {
    if (!editable || !service || !access) return
    pendingChange = change
    service.setAccess(change.subject, change.capability, change.rule, access.revision, change.effects)
  }
  // Undo sets the rule back only if the cell still shows what this change set.
  function undo(change) {
    if (!host || !service) return
    var row = computerId === change.computerId ? rowFor(change.subject) : null
    var still = !!row && (!change.effects ? String(row.capabilities[change.capability] || "deny") === change.rule
      : !change.effectKey || String(row.effects[change.effectKey] || "deny") === String(change.effects[change.effectKey] || row.effects[change.effectKey]))
    if (!still) { host.notify("Not undone: " + change.subject + "'s access on " + computerLabel + " changed again since.", true); return }
    if (service.mutating) { host.notify("Wait for the current action to finish, then choose Undo again.", true); return }
    applyChange({ computerId: change.computerId, subject: change.subject, capability: change.capability, rule: change.undoRule,
      effects: change.undoEffects, effectKey: change.effectKey, undoRule: change.rule, undoEffects: change.effects, undoing: true,
      text: change.undoText || change.subject + "'s " + (change.effectKey ? effectLabel(change.effectKey) + " steps" : capabilityLabel(change.capability)) + " on " + computerLabel + " is back to " +
        ruleLabel(change.effects ? change.undoEffects[change.effectKey] : change.undoRule) + "." })
  }
  Connections {
    target: root.service
    ignoreUnknownSignals: true
    function onAccessChangeSettled(computerId, ok) {
      var change = root.pendingChange
      if (!change || change.computerId !== computerId) return
      root.pendingChange = null
      if (!ok || !root.host) return
      if (change.undoing) root.host.notify(change.text, false)
      else root.host.offerUndo(change.text, function() { root.undo(change) })
    }
  }
  function choose(row, capability, next, cell) {
    if (!editable) return
    var previous = String(row.capabilities[capability] || "deny")
    if (next === previous) return
    var who = String(row.subject)
    var change = { computerId: computerId, subject: who, capability: capability, rule: next, undoRule: previous,
      text: who + "'s " + capabilityLabel(capability) + " on " + computerLabel + " is now " + ruleLabel(next) + "." }
    // Taking away your own Administer permission can lock you out, so it asks first, on the cell.
    if (capability === "administer" && isOwn(row) && previous === "allow") {
      if (!host) return
      var scope = computerId, revision = access.revision
      host.askConfirm({
        anchor: cell,
        message: "This takes away your own permission to administer " + computerLabel + ". You may not be able to change access here again.",
        confirmLabel: next === "deny" ? "Deny Myself" : "Ask First for Me",
        danger: true,
        run: function() { root.applyChange(change) },
        valid: function() { return root.computerId === scope && !!root.access && root.access.revision === revision },
        staleMessage: staleMessage()
      })
      return
    }
    applyChange(change)
  }
  // One kind of step: only that rule of the row's own is written, so the others keep following
  // its computer and this computer's Settings tab.
  function chooseEffect(row, key, next) {
    if (!editable) return
    if (String(row.effects[key] || "deny") === next) return
    var previous = ownEffects(row), effects = ownEffects(row)
    effects[key] = next
    var who = String(row.subject)
    applyChange({ computerId: computerId, subject: who, capability: "agents", rule: String(row.capabilities.agents || "deny"), undoRule: String(row.capabilities.agents || "deny"),
      effects: effects, undoEffects: previous, effectKey: key,
      text: who + "'s " + effectLabel(key) + " steps on " + computerLabel + " are now " + ruleLabel(next) + "." })
  }
  function remove(row, unpair, button) {
    if (!editable || !host || row.subject === "owner") return
    var scope = computerId, revision = access.revision, who = String(row.subject)
    host.askConfirm({
      anchor: button,
      message: (unpair ? "Remove the pairing with " : "Remove all access for ") + who + " on " + computerLabel + "?" +
        (isOwn(row) ? " This is your own access, including Administer." : "") + " Its tasks and viewing stop, and generated login access is removed." +
        (unpair ? " To use it again, pair it with Let Another Computer In below." : " The pairing stays, so you can give access back here."),
      confirmLabel: unpair ? "Remove Pairing" : "Remove Access",
      danger: true,
      run: function() { root.service.removeAccess(who, revision, unpair) },
      valid: function() { return root.computerId === scope && !!root.access && root.access.revision === revision },
      staleMessage: staleMessage()
    })
  }
  function staleMessage() { return "Nothing was changed: access on " + computerLabel + " changed while you were deciding. Choose again." }

  // An identity's details and more choices: how it came to be here, its grants, its agents'
  // send, spend and delete rules, and removing it. Under its row, or beside the table when wide.
  component RowDetail: Column {
    id: detail
    required property var row
    readonly property bool owner: row.subject === "owner"
    topPadding: Style.space(4)
    bottomPadding: Style.space(12)
    spacing: Style.space(10)
    Copy {
      width: parent.width
      text: detail.row.kind === "agent" ? "An agent on a paired computer. That computer vouches for its name."
        : detail.owner ? root.computerLabel + "'s owner is the administrator account on " + root.computerLabel + " itself. It always has every permission."
        : !detail.row.paired ? "Pairing removed. Use Let Another Computer In below to pair it again."
        : detail.row.invite ? "Paired with an invite: " + StatusModel.shareLevelLabel(detail.row.invite) + ". Revoking the invite on this computer's Share This Computer page ends it."
        : "Paired with " + root.computerLabel + "."
      dimmed: true
      font.pixelSize: Style.font.bodySmall
    }
    Repeater {
      model: root.access ? Object.keys(root.access.grants).filter(function(k) { return root.access.grants[k].subject === detail.row.subject }).map(function(k) {
        var g = root.access.grants[k]
        return root.tokens.ink(root.capabilityLabel(g.capability) + ": ", root.tokens.dim) + root.tokens.ink(root.ruleLabel(g.rule), root.tokens.textTint(root.tokens.ruleColor(g.rule))) +
          (g.expires_at ? root.tokens.ink(" until " + (root.tokens.clockLabel(g.expires_at) || String(g.expires_at)), root.tokens.dim) : "")
      }) : []
      delegate: Copy { width: detail.width; textFormat: Text.StyledText; text: String(modelData); font.pixelSize: Style.font.bodySmall }
    }
    // Ask before it sends, spends or deletes: its own switch, and the way back to
    // the computer's rules.
    Row {
      visible: !detail.owner && root.askable(detail.row).length > 0
      spacing: Style.space(10)
      Item {
        id: askSwitch
        readonly property bool on: root.asksFirst(detail.row)
        readonly property string title: detail.row.kind === "computer" ? "Ask before its agents send, spend or delete" : "Ask before it sends, spends or deletes"
        function flip() { if (root.editable) root.setAsking(detail.row, !on) }
        anchors.verticalCenter: parent.verticalCenter
        implicitWidth: askToggle.implicitWidth
        implicitHeight: askToggle.implicitHeight
        activeFocusOnTab: true
        Keys.onSpacePressed: function(event) { askSwitch.flip(); event.accepted = true }
        Keys.onReturnPressed: function(event) { askSwitch.flip(); event.accepted = true }
        Keys.onEnterPressed: function(event) { askSwitch.flip(); event.accepted = true }
        Accessible.role: Accessible.CheckBox
        Accessible.checkable: true
        Accessible.checked: askSwitch.on
        Accessible.name: root.identityLabel(detail.row) + ", " + askSwitch.title
        Accessible.focusable: true
        Accessible.onToggleAction: askSwitch.flip()
        Accessible.onPressAction: askSwitch.flip()
        ToggleSwitch {
          id: askToggle
          anchors.fill: parent
          checked: askSwitch.on
          enabled: root.editable
          foreground: Color.popups.text
          accent: Color.accent
          onToggled: { askSwitch.forceActiveFocus(); askSwitch.flip() }
        }
        Rectangle {
          anchors.fill: parent
          z: 10
          visible: askSwitch.activeFocus
          radius: 0
          color: "transparent"
          border.width: Math.max(1, Style.space(2))
          border.color: Color.accent
          Accessible.ignored: true
        }
      }
      Copy { anchors.verticalCenter: parent.verticalCenter; text: askSwitch.title; font.pixelSize: Style.font.bodySmall; wrapMode: Text.NoWrap }
      ActionButton {
        anchors.verticalCenter: parent.verticalCenter
        visible: root.setsAsking(detail.row)
        size: "small"
        role: "quiet"
        label: "Use This Computer's Setting"
        blocked: !root.editable
        disabledReason: root.editReason
        tooltipText: "Send, spend and delete follow " + root.computerLabel + "'s Settings tab (now " + (root.computerAsks ? "on" : "off") + ")"
        onClicked: if (!blocked) root.followComputer(detail.row)
      }
    }
    Copy {
      visible: !detail.owner
      width: parent.width
      text: "Agent task steps, by kind.\nFor Send, Spend and Destructive, Ask First set for an agent or for its computer wins over Allowed, since any agent on a computer can use another's name.\nA kind neither sets follows Ask before agents send, spend or delete on this computer's Settings tab (now " + (root.computerAsks ? "on" : "off") + ") for agents from your own computers; agents from someone else's computer ask.\nDenied always wins."
      font.pixelSize: Style.font.bodySmall
    }
    Flow {
      visible: !detail.owner
      width: parent.width
      spacing: Style.space(14)
      Repeater {
        model: root.effectClasses
        delegate: Row {
          required property var modelData
          spacing: Style.space(6)
          Copy { anchors.verticalCenter: parent.verticalCenter; text: modelData.label; color: root.tokens.textTint(root.tokens.effectColor(modelData.key)); font.pixelSize: Style.font.bodySmall; wrapMode: Text.NoWrap }
          RuleMenu {
            anchors.verticalCenter: parent.verticalCenter
            rule: String(detail.row.effects[modelData.key] || "deny")
            blocked: !root.editable
            disabledReason: root.editReason
            accessibleName: root.identityLabel(detail.row) + ", " + modelData.label + " steps"
            onChosen: function(value) { root.chooseEffect(detail.row, modelData.key, value) }
          }
        }
      }
    }
    Row {
      visible: !detail.owner
      spacing: Style.space(8)
      ActionButton {
        id: removeAccessButton
        size: "small"
        label: "Remove Access"
        role: "danger"
        blocked: !root.editable
        disabledReason: root.editReason
        onClicked: if (!blocked) root.remove(detail.row, false, removeAccessButton)
      }
      ActionButton {
        id: removePairingButton
        size: "small"
        label: "Remove Pairing"
        role: "danger"
        visible: detail.row.kind === "computer" && detail.row.paired
        blocked: !root.editable
        disabledReason: root.editReason
        onClicked: if (!blocked) root.remove(detail.row, true, removePairingButton)
      }
    }
  }

  // Wide, the table on the left, and how access works with the open row's details on the right;
  // narrower, one column with the details under their row.
  readonly property bool wide: width >= tokens.wideAt
  readonly property real sideWidth: Math.min(tokens.proseWidth, width - tableWidth - tokens.columnGap)
  readonly property var selectedRow: rowFor(selectedSubject)
  readonly property string howItWorks: "Choose a cell to change it: the change applies at once and a message offers Undo. Denied wins over Ask First; Ask First wins over Allowed. Open a row for its agents' send, spend and delete rules."

  Flickable {
    id: scroll
    anchors.fill: parent
    contentWidth: Math.max(width, root.tableWidth)
    contentHeight: root.wide ? Math.max(body.implicitHeight, side.implicitHeight) : body.implicitHeight
    clip: true
    boundsBehavior: Flickable.StopAtBounds
    Column {
      id: body
      width: root.wide ? root.tableWidth : scroll.width
      spacing: Style.space(12)
      Copy { text: "Who can use " + root.computerLabel; font.pixelSize: Style.font.heading; font.bold: true }
      Copy {
        visible: !root.wide
        width: Math.min(parent.width, root.tokens.proseWidth)
        text: root.howItWorks
        dimmed: true
      }
      DataAge {
        at: root.service && root.service.scopedReadAt.access || 0
        refreshing: !!root.service && root.service.readPending("access")
        nowMs: root.service ? root.service.nowMs : Date.now()
      }
      Copy { width: parent.width; visible: !root.access; text: root.service && root.service.readErrors.access || "Loading access…"; color: root.tokens.textTint(root.service && root.service.readErrors.access ? root.tokens.attentionColor : root.tokens.workingColor) }

      // The table: a header row, then one compact row per identity.
      Column {
        visible: !!root.access
        width: root.tableWidth
        Row {
          height: Style.space(24)
          Copy { width: root.identityWidth; anchors.verticalCenter: parent.verticalCenter; text: "Who"; eyebrow: true }
          Repeater {
            model: root.capabilities
            delegate: Copy { width: root.columnWidth(modelData.label); anchors.verticalCenter: parent.verticalCenter; text: modelData.label; eyebrow: true; wrapMode: Text.NoWrap }
          }
        }
        Repeater {
          model: root.rows
          delegate: Column {
            id: rowItem
            required property var modelData
            readonly property var row: modelData
            readonly property bool owner: row.subject === "owner"
            readonly property bool open: root.selectedSubject === row.subject
            width: root.tableWidth
            Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
            Row {
              height: root.rowHeight
              Item {
                width: root.identityWidth
                height: parent.height
                ActionButton {
                  anchors.verticalCenter: parent.verticalCenter
                  size: "small"
                  role: "quiet"
                  glyph: rowItem.open ? "▾" : "▸"
                  label: root.identityLabel(rowItem.row)
                  selected: rowItem.open
                  tooltipText: rowItem.open ? "Hide details" : "Show details and more choices"
                  Accessible.name: root.identityLabel(rowItem.row) + (rowItem.open ? ", details shown" : ", show details")
                  onClicked: root.selectedSubject = rowItem.open ? "" : rowItem.row.subject
                }
              }
              Repeater {
                model: root.capabilities
                delegate: Item {
                  id: cell
                  required property var modelData
                  readonly property string capability: modelData.key
                  readonly property string rule: String(rowItem.row.capabilities[capability] || "deny")
                  width: root.columnWidth(modelData.label)
                  height: parent.height
                  // The computer's owner always has every permission; its cells are words, not choices.
                  Copy {
                    visible: rowItem.owner
                    anchors.verticalCenter: parent.verticalCenter
                    x: Style.spacing.controlPaddingX - Style.space(2)
                    text: root.ruleLabel(cell.rule)
                    color: root.tokens.textTint(root.tokens.ruleColor(cell.rule))
                    font.pixelSize: Style.font.bodySmall
                    wrapMode: Text.NoWrap
                  }
                  RuleMenu {
                    id: menu
                    visible: !rowItem.owner
                    anchors.verticalCenter: parent.verticalCenter
                    rule: cell.rule
                    blocked: !root.editable
                    disabledReason: root.editReason
                    accessibleName: root.identityLabel(rowItem.row) + ", " + cell.modelData.label
                    onChosen: function(value) { root.choose(rowItem.row, cell.capability, value, menu) }
                  }
                }
              }
            }
            // Only the open row builds its detail.
            Loader {
              active: rowItem.open && !root.wide
              visible: active
              width: body.width
              sourceComponent: Component { RowDetail { row: rowItem.row; width: body.width } }
            }
          }
        }
        Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }
      }
      Copy { width: Math.min(parent.width, root.tokens.proseWidth); text: root.access ? String(root.access.boundary) : ""; dimmed: true; font.pixelSize: Style.font.bodySmall }
      ActionButton { glyph: "+"; label: "Let Another Computer In"; onClicked: if (root.host) root.host.showAdd() }
    }

    Column {
      id: side
      visible: root.wide
      x: root.tableWidth + root.tokens.columnGap
      width: root.sideWidth
      spacing: Style.space(12)
      Copy { eyebrow: true; text: "How access works" }
      Copy { width: parent.width; text: root.howItWorks; dimmed: true }
      Item { width: 1; height: Style.space(8); visible: !!root.selectedRow }
      Copy { visible: !!root.selectedRow; width: parent.width; text: root.selectedRow ? root.identityLabel(root.selectedRow) : ""; font.pixelSize: Style.font.title; font.bold: true }
      Loader {
        active: root.wide && !!root.selectedRow
        visible: active
        width: parent.width
        sourceComponent: Component { RowDetail { row: root.selectedRow; width: side.width } }
      }
    }
  }
}
