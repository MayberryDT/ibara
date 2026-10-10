import QtQuick
import qs.Commons
import qs.Ui
import "Reveal.js" as Reveal
import "SettingsText.js" as SettingsText

// Settings, as ibarad describes them: sections of settings, each a title, one help line and the
// control for its type (a switch, a row of choices, or a field for a number, text or shortcut).
// Presentation only: a change or reset is a signal, and the parent sends it to the service and
// hands back the new sections, the keys still saving and any sentence explaining a refusal.
// A search field at the top narrows the list by title, help or key. The keyboard alone does it
// all: Tab walks every control and Reset button in reading order, Up and Down move between the
// settings, Left and Right move between choices, and Space or Enter flips a switch or picks.
Item {
  id: root
  // [{ id, title, settings: [{ key, title, help, type, choices?, min?, max?, value, default, scope }] }]
  property var sections: []
  // key -> true while a change for that key is on its way, or what the person is waiting on.
  property var busy: ({})
  // key -> a sentence explaining why a change didn't take.
  property var errors: ({})
  // key -> a line or two the page adds under that setting.
  property var notes: ({})
  // key -> a Component the page adds under that setting's words, for what isn't one setting
  // (Logins: a site count and the Manage Logins button).
  property var extras: ({})
  // Shown while there are no sections at all: still loading, or why they couldn't be read.
  property color emptyColor: tokens.dim
  property string emptyText: ""
  property alias searchText: searchField.text
  // Every value leaves as text: "true"/"false", a decimal number, a choice's id, or what was typed.
  signal changeRequested(string key, string value)
  signal resetRequested(string key)
  signal sectionResetRequested(string sectionId)

  readonly property Tokens tokens: Tokens {}
  readonly property string query: searchField.text.trim().toLowerCase()
  readonly property bool anyMatch: Array.isArray(sections) && sections.some(function(section) {
    return !!section && Array.isArray(section.settings) && section.settings.some(root.matches)
  })

  // The keyboard lands on the first setting, or on the search field while none shows.
  function focusDefault() {
    var rows = visibleRows()
    if (rows.length) rows[0].focusControl()
    else focusSearch()
  }
  function focusSearch() { searchField.focusInput() }

  // Values arrive as JSON booleans, numbers or strings; they compare, and leave, as text.
  function textOf(value) {
    if (value === undefined || value === null) return ""
    return typeof value === "object" ? JSON.stringify(value) : String(value)
  }
  function isChanged(setting) {
    return !!setting && textOf(setting.value) !== textOf(setting["default"])
  }
  function matches(setting) {
    if (!setting) return false
    if (!query) return true
    return [setting.title, setting.help, setting.key].some(function(part) {
      return textOf(part).toLowerCase().indexOf(root.query) !== -1
    })
  }
  // Choices come as ["a", "b"] or [{ value, label }]; a bare id reads as a word.
  function choicesOf(setting) {
    var list = setting && Array.isArray(setting.choices) ? setting.choices : []
    return list.map(function(choice) {
      if (choice && typeof choice === "object")
        return { value: textOf(choice.value), label: choice.label !== undefined && choice.label !== null ? textOf(choice.label) : tokens.sentence(textOf(choice.value)) }
      return { value: textOf(choice), label: tokens.sentence(textOf(choice)) }
    })
  }
  // A value as words, for the Reset tooltip: "Back to off", "Back to Dark", "Back to 5".
  function valueWords(setting, value) {
    var text = textOf(value)
    if (setting.type === "bool") return text === "true" ? "on" : "off"
    if (setting.type === "choice") {
      var choices = choicesOf(setting)
      for (var i = 0; i < choices.length; i++) if (choices[i].value === text) return choices[i].label
    }
    if (text === "") return setting.type === "shortcut" ? "no shortcut" : "blank"
    return setting.type === "text" ? "“" + text + "”" : text
  }
  function hasBound(value) { return value !== undefined && value !== null && textOf(value) !== "" }
  // " (1–30)" after a number's help line, so the allowed range is never a surprise.
  function rangeSuffix(setting) {
    var low = hasBound(setting.min), high = hasBound(setting.max)
    if (low && high) return " (" + textOf(setting.min) + "–" + textOf(setting.max) + ")"
    if (low) return " (" + textOf(setting.min) + " or more)"
    if (high) return " (up to " + textOf(setting.max) + ")"
    return ""
  }
  function rangeSentence(setting) {
    var low = hasBound(setting.min), high = hasBound(setting.max)
    if (low && high) return "Choose a number from " + textOf(setting.min) + " to " + textOf(setting.max) + "."
    if (low) return "Choose a number of " + textOf(setting.min) + " or more."
    if (high) return "Choose a number up to " + textOf(setting.max) + "."
    return "Type a number."
  }

  // The settings showing now, in reading order, for Up and Down.
  function visibleRows() {
    var list = []
    for (var s = 0; s < leftRepeater.count + rightRepeater.count; s++) {
      var section = s < leftRepeater.count ? leftRepeater.itemAt(s) : rightRepeater.itemAt(s - leftRepeater.count)
      if (!section || !section.visible) continue
      for (var r = 0; r < section.rowCount(); r++) {
        var row = section.rowAt(r)
        if (row && row.visible) list.push(row)
      }
    }
    return list
  }
  // Up from the first setting reaches the search field; Down from the last lets the key go on.
  function moveFrom(row, step) {
    var rows = visibleRows()
    var next = rows.indexOf(row) + step
    if (next < 0) { focusSearch(); return true }
    if (next >= rows.length) return false
    rows[next].focusControl()
    return true
  }
  // From a section's Reset button: Down reaches its first setting, Up the setting above it.
  function moveFromSection(section, step) {
    var rows = visibleRows()
    for (var i = 0; i < rows.length; i++) {
      if (rows[i].sectionBox !== section) continue
      if (step > 0) { rows[i].focusControl(); return true }
      if (i === 0) { focusSearch(); return true }
      rows[i - 1].focusControl()
      return true
    }
    return false
  }

  // One section: its title (with Reset Section while any setting in it is changed), then its
  // settings.
  component SectionBox: Column {
    id: sectionItem
    property int sectionIndex: 0
    readonly property var section: root.sections[sectionIndex] || ({})
    readonly property var settings: Array.isArray(section.settings) ? section.settings : []
    readonly property bool changed: settings.some(root.isChanged)
    function rowCount() { return settingRepeater.count }
    function rowAt(i) { return settingRepeater.itemAt(i) }
    width: parent ? parent.width : 0
    spacing: 0
    visible: settings.some(root.matches)

    Item {
      width: parent.width
      height: sectionItem.section.id === "logins" ? 0 : Math.max(sectionTitle.implicitHeight, resetSection.visible ? resetSection.height : 0) + Style.space(8)
      visible: sectionItem.section.id !== "logins"
      Keys.onUpPressed: function(event) { event.accepted = root.moveFromSection(sectionItem, -1) }
      Keys.onDownPressed: function(event) { event.accepted = root.moveFromSection(sectionItem, 1) }
      Copy {
        id: sectionTitle
        anchors.left: parent.left
        anchors.right: resetSection.left
        anchors.rightMargin: Style.space(10)
        anchors.verticalCenter: parent.verticalCenter
        eyebrow: true
        text: root.textOf(sectionItem.section.title)
      }
      ActionButton {
        id: resetSection
        visible: sectionItem.changed
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        size: "small"
        role: "quiet"
        label: "Reset Section"
        Accessible.name: "Reset every " + root.textOf(sectionItem.section.title) + " setting"
        tooltipText: "Put every setting here back to its default"
        onClicked: {
          // The button hides once the section is back to its defaults; the keyboard moves on.
          if (resetSection.activeFocus) root.moveFromSection(sectionItem, 1)
          root.sectionResetRequested(root.textOf(sectionItem.section.id))
        }
      }
    }

    Repeater {
      id: settingRepeater
      model: sectionItem.settings.length
      delegate: Rectangle {
        id: row
        required property int index
        readonly property Item sectionBox: sectionItem
        readonly property var setting: sectionItem.settings[index] || ({})
        readonly property string key: root.textOf(setting.key)
        readonly property string title: root.textOf(SettingsText.titles[key] || setting.title)
        readonly property string kind: ["bool", "choice", "number", "shortcut", "summary"].indexOf(setting.type) !== -1 ? setting.type : "text"
        readonly property bool choiceMenu: kind === "choice" && (root.choicesOf(setting).length > 3 || row.width < Style.space(650))
        readonly property string valueText: root.textOf(setting.value)
        readonly property bool changed: root.isChanged(setting)
        readonly property bool saving: root.busy[key] === true || typeof root.busy[key] === "string"
        readonly property string detail: [root.textOf(setting.details || SettingsText.details[key]), root.textOf(root.notes[key])].filter(function(line) { return line !== "" }).join("\n")
        property bool detailsOpen: false
        readonly property string help: root.textOf(SettingsText.help[key] || setting.help) + (kind === "number" ? root.rangeSuffix(setting) : "")
        readonly property string errorText: localError !== "" ? localError : root.errors[key] ? root.textOf(root.errors[key]) : ""
        readonly property bool focusInside: switchBox.activeFocus || group.activeFocus || choiceDropdown.opened || choiceDropdown.button.activeFocus || field.input.activeFocus || resetButton.activeFocus || infoButton.activeFocus
        // A field's number that is out of range, said here and never sent.
        property string localError: ""
        // The text last sent for this field, so Enter then leaving the field sends it once.
        property var pending: null

        function focusControl() {
          if (kind === "bool") switchBox.forceActiveFocus()
          else if (kind === "choice") { if (choiceMenu) choiceDropdown.focusTrigger(); else group.forceActiveFocus() }
          else if (kind === "summary") { if (summaryLoader.item) summaryLoader.item.focusDefault(); else root.focusSearch() }
          else field.focusInput()
        }
        function flip() {
          root.changeRequested(key, valueText === "true" ? "false" : "true")
        }
        // Fields send on Enter or on leaving, only when the text says something new.
        function commitField() {
          var text = field.text
          if (kind === "number") {
            var trimmed = text.trim()
            var number = Number(trimmed)
            var low = root.hasBound(setting.min), high = root.hasBound(setting.max)
            if (trimmed === "" || !isFinite(number)
                || (low && number < Number(setting.min)) || (high && number > Number(setting.max))) {
              localError = root.rangeSentence(setting)
              return
            }
            text = String(number)
            if (text === valueText) { field.text = valueText; localError = ""; return }
          } else if (text === valueText) return
          if (text === pending) return
          pending = text
          localError = ""
          root.changeRequested(key, text)
        }
        // Escape puts the field back to the saved value.
        function restoreField() {
          field.text = valueText
          pending = null
          localError = ""
        }
        // A new value shows in the field unless you are in the middle of typing another.
        onValueTextChanged: {
          localError = ""
          if (!field.input.activeFocus || pending !== null) field.text = valueText
          pending = null
        }
        onFocusInsideChanged: if (focusInside) Reveal.reveal(row)
        Component.onCompleted: field.text = valueText

        width: sectionItem.width
        height: Math.max(words.implicitHeight, controls.height) + Style.space(14)
        visible: root.matches(setting)
        radius: 0
        color: focusInside ? Qt.alpha(Color.accent, 0.08) : "transparent"
        Keys.onUpPressed: function(event) { event.accepted = root.moveFrom(row, -1) }
        Keys.onDownPressed: function(event) { event.accepted = root.moveFrom(row, 1) }

        Rectangle { width: parent.width; height: 1; radius: 0; color: root.tokens.rule }

        Column {
          id: words
          x: Style.space(10)
          anchors.verticalCenter: parent.verticalCenter
          // Short help stays on one line; the control keeps to the right.
          width: Math.min(root.tokens.proseWidth, Math.max(Style.space(160), row.width - x - controls.width - Style.space(34)))
          spacing: Style.space(3)
          Row {
            visible: row.kind !== "summary"
            width: parent.width
            spacing: Style.space(6)
            Copy {
              id: titleText
              width: Math.min(implicitWidth, parent.width - (savingText.visible ? savingText.implicitWidth + parent.spacing : 0) - (infoButton.visible ? infoButton.width + parent.spacing : 0))
              text: row.title
              font.bold: true
            }
            ActionButton {
              id: infoButton
              anchors.verticalCenter: parent.verticalCenter
            visible: row.detail !== ""
            size: "small"
            role: "quiet"
            label: "ⓘ"
            selected: row.detailsOpen
            tooltipText: "Learn More"
            Accessible.name: "Learn more about " + row.title
            Accessible.role: Accessible.Button
            Accessible.description: row.detailsOpen ? "Expanded" : "Collapsed"
            onClicked: row.detailsOpen = !row.detailsOpen
          }
            Copy {
              id: savingText
              visible: row.saving
              anchors.baseline: titleText.baseline
              text: typeof root.busy[row.key] === "string" ? root.busy[row.key] : "Saving…"
              color: root.tokens.textTint(root.tokens.workingColor)
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.NoWrap
            }
          }
          Copy {
            visible: text !== ""
            width: parent.width
            text: row.help
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Copy {
            visible: row.detailsOpen && row.detail !== ""
            width: parent.width
            text: row.detail
            dimmed: true
            font.pixelSize: Style.font.bodySmall
          }
          Copy {
            visible: text !== ""
            width: parent.width
            text: row.errorText
            color: root.tokens.textTint(root.tokens.attentionColor)
            font.pixelSize: Style.font.bodySmall
          }
          Loader {
            id: summaryLoader
            active: !!root.extras[row.key]
            visible: active
            width: parent.width
            sourceComponent: root.extras[row.key] || null
          }
        }

        // The control sits at the right edge; its Reset button, when shown, just left of it.
        // Laid out right to left so Tab reaches the control first, then its Reset.
        Row {
          id: controls
          anchors.right: parent.right
          anchors.rightMargin: Style.space(10)
          // Beside a page's extra content the control stays level with the setting's title.
          y: root.extras[row.key] ? words.y : Math.round((parent.height - height) / 2)
          layoutDirection: Qt.RightToLeft
          spacing: Style.space(8)

          // The kit switch doesn't take the keyboard, so this box does: Space or Enter
          // flips it, and an accent ring shows where the keyboard is.
          Item {
            id: switchBox
            visible: row.kind === "bool"
            anchors.verticalCenter: parent.verticalCenter
            implicitWidth: toggle.implicitWidth
            implicitHeight: toggle.implicitHeight
            activeFocusOnTab: row.kind === "bool"
            Keys.onSpacePressed: function(event) { row.flip(); event.accepted = true }
            Keys.onReturnPressed: function(event) { row.flip(); event.accepted = true }
            Keys.onEnterPressed: function(event) { row.flip(); event.accepted = true }
            Accessible.role: Accessible.CheckBox
            Accessible.checkable: true
            Accessible.checked: toggle.checked
            Accessible.name: row.title
            Accessible.description: row.help
            Accessible.focusable: true
            Accessible.onToggleAction: row.flip()
            Accessible.onPressAction: row.flip()
            ToggleSwitch {
              id: toggle
              anchors.fill: parent
              checked: row.valueText === "true"
              foreground: Color.popups.text
              accent: Color.accent
              onToggled: { switchBox.forceActiveFocus(); row.flip() }
            }
            Rectangle {
              anchors.fill: parent
              z: 10
              visible: switchBox.activeFocus
              radius: 0
              color: "transparent"
              border.width: Math.max(1, Style.space(2))
              border.color: Color.accent
              Accessible.ignored: true
            }
          }

          ButtonGroup {
            id: group
            visible: row.kind === "choice" && !row.choiceMenu
            focusable: visible
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(6)
            options: row.kind === "choice" ? root.choicesOf(row.setting) : []
            value: row.valueText
            foreground: Color.popups.text
            background: Color.popups.background
            accent: Color.accent
            fontSize: Style.font.bodySmall
            Accessible.role: Accessible.Grouping
            Accessible.name: row.title
            Accessible.description: row.help
            onChanged: function(value) { if (value !== row.valueText) root.changeRequested(row.key, value) }
          }

          ActionMenu {
            id: choiceDropdown
            visible: row.choiceMenu
            anchors.verticalCenter: parent.verticalCenter
            size: "small"
            label: root.valueWords(row.setting, row.setting.value) + " ▾"
            accessibleName: row.title
            items: root.choicesOf(row.setting).map(function(choice) { return {id: choice.value, label: choice.label, selected: choice.value === row.valueText} })
            onTriggered: value => { if (value !== row.valueText) root.changeRequested(row.key, value) }
          }

          FieldInput {
            id: field
            visible: row.kind === "number" || row.kind === "text" || row.kind === "shortcut"
            anchors.verticalCenter: parent.verticalCenter
            width: row.kind === "number" ? Style.space(84) : row.kind === "shortcut" ? Style.space(200) : Style.space(260)
            monospace: row.kind === "shortcut"
            placeholder: row.kind === "number" ? "" : "Not set"
            accessibleName: row.title
            onEdited: {
              row.pending = null
              row.localError = ""
              // Numbers take digits, a point and a leading minus; anything else drops out.
              if (row.kind === "number") {
                var digits = field.text.replace(/[^0-9.\-]/g, "")
                if (digits !== field.text) field.text = digits
              }
            }
            onEditingFinished: row.commitField()
            // Escape with an edit to discard puts the saved value back and stops there;
            // with nothing to discard it goes on, so Escape still leaves the page.
            Keys.onEscapePressed: function(event) {
              var edited = field.text !== row.valueText || row.localError !== ""
              if (edited) row.restoreField()
              event.accepted = edited
            }
            Binding {
              target: field.input
              property: "Accessible.description"
              value: row.help
            }
          }

          ActionButton {
            id: resetButton
            visible: row.changed
            anchors.verticalCenter: parent.verticalCenter
            size: "small"
            role: "quiet"
            label: "Reset"
            Accessible.name: "Reset " + row.title
            tooltipText: "Back to " + root.valueWords(row.setting, row.setting["default"])
            onClicked: {
              // The button hides once the setting is back to its default; the keyboard
              // goes to the setting's control rather than nowhere.
              if (resetButton.activeFocus) row.focusControl()
              root.resetRequested(row.key)
            }
          }
        }
      }
    }
  }

  // Wide, the sections in two columns side by side, split so each holds about half the settings
  // showing; narrower, one column. `centered` puts them in the middle of the page (ibara's
  // Settings); a computer's Settings tab keeps them under its tabs.
  property bool centered: false
  readonly property bool wide: width >= tokens.wideAt
  readonly property real columnWidth: wide ? Math.min(Style.space(800), Math.floor((width - tokens.columnGap) / 2)) : Math.min(width, Style.space(860))
  readonly property real usedWidth: wide ? columnWidth * 2 + tokens.columnGap : columnWidth
  readonly property real leftX: centered ? Math.max(0, Math.round((width - usedWidth) / 2)) : 0
  // How many sections, from the first, go in the left column.
  readonly property int splitAt: {
    if (!wide) return sections.length
    var counts = sections.map(function(section) { return section && Array.isArray(section.settings) ? section.settings.filter(root.matches).length : 0 })
    var total = counts.reduce(function(a, b) { return a + b }, 0), sum = 0
    for (var i = 0; i < counts.length; i++) {
      sum += counts[i]
      if (sum * 2 >= total) return i + 1
    }
    return sections.length
  }

  // Room left at the search field's right for a line of the page's own (the computer Settings
  // tab's "Updated …"), so nothing sits between the tabs and the settings.
  property real searchInset: 0
  readonly property real searchHeight: searchField.height
  FieldInput {
    id: searchField
    x: root.leftX
    width: Math.min(root.columnWidth, root.width - root.searchInset - x)
    placeholder: "Find a setting"
    accessibleName: "Find a setting"
    hint: "/"
    Keys.onDownPressed: function(event) {
      var rows = root.visibleRows()
      if (rows.length) rows[0].focusControl()
      event.accepted = rows.length > 0
    }
  }

  Flickable {
    id: scroll
    x: root.leftX
    y: searchField.height + Style.space(14)
    width: root.usedWidth
    height: parent.height - y
    clip: true
    contentWidth: width
    contentHeight: Math.max(leftColumn.implicitHeight, rightColumn.implicitHeight) + Style.space(16)
    boundsBehavior: Flickable.StopAtBounds
    flickableDirection: Flickable.VerticalFlick

    // Sections and settings repeat by count, so a fresh copy of the same settings (after every
    // change) updates the rows in place and the keyboard stays where it was.
    Column {
      id: leftColumn
      width: root.columnWidth
      spacing: Style.space(14)
      Copy {
        visible: root.sections.length === 0 || !root.anyMatch
        width: parent.width
        text: root.sections.length === 0 ? root.emptyText : "No settings match. Clear the search."
        color: root.sections.length === 0 ? root.emptyColor : root.tokens.dim
      }
      Repeater {
        id: leftRepeater
        model: root.splitAt
        delegate: SectionBox { sectionIndex: index }
      }
    }
    Column {
      id: rightColumn
      x: root.columnWidth + root.tokens.columnGap
      width: root.columnWidth
      spacing: Style.space(14)
      Repeater {
        id: rightRepeater
        model: root.sections.length - root.splitAt
        delegate: SectionBox { sectionIndex: root.splitAt + index }
      }
    }
  }
}
