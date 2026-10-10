import QtQuick
import QtQuick.Controls
import qs.Commons
import qs.Ui
import "Reveal.js" as Reveal

// The console's one button. Every clickable action uses it, in one of four roles and two sizes.
//   role  primary    accent fill: the one main action on a surface
//         secondary  bordered (the default)
//         danger     urgent label and border: removes, stops or powers off
//         quiet      no border at rest: back links, close, filters and toggles in a bar
//   size  normal, or small for card overlays, table rows and messages
// The label is one line in Title Case, sized to its text. A blocked button stays reachable so
// its reason shows in the tooltip and to screen readers; handlers must check `blocked`. A
// blocked primary loses its fill. Fills, borders, keys and mouse handling follow the kit
// Button; the glyph and tooltip load only when used.
BorderSurface {
  id: root
  property string label: ""
  property string glyph: ""
  property string disabledReason: ""
  property string tooltipText: disabledReason
  property bool blocked: false
  property string role: "secondary"
  property string size: "normal"
  property bool selected: false
  property bool hasCursor: false
  property bool focusable: true
  signal clicked()
  // True while the focus it has came from a click, not the keyboard (Toasts reads it).
  property bool pointerFocused: false

  readonly property bool small: size === "small"
  readonly property bool filled: role === "primary" && !blocked
  readonly property bool quiet: role === "quiet"
  readonly property bool hot: mouseArea.containsMouse || hasCursor
  readonly property color ink: Color.popups.text
  property color labelColor: filled ? Color.popups.background
    : role === "danger" ? Color.urgent
    : selected ? Style.selectedStateColor(ink, Color.accent)
    : ink
  readonly property real padding: small ? Style.spacing.controlPaddingX - Style.space(2) : Style.spacing.controlPaddingX + Style.space(2)

  implicitHeight: small ? Style.space(26) : Style.space(32)
  implicitWidth: Math.ceil(labelText.implicitWidth + (glyph ? symbol.width + Style.space(5) : 0) + padding * 2)
  opacity: blocked || !enabled ? 0.55 : 1
  activeFocusOnTab: focusable
  color: filled ? (hot || activeFocus ? Qt.tint(Color.accent, Qt.alpha(Color.popups.background, 0.18)) : Color.accent)
    : focusable && activeFocus ? Style.focusFillFor(ink, Color.accent)
    : hot ? Style.hoverFillFor(ink, Color.accent)
    : quiet ? "transparent"
    : selected ? Style.selectedFillFor(ink, Color.accent)
    : Qt.alpha(ink, 0.07)
  borderSpec: filled ? Border.none()
    : focusable && activeFocus ? Border.controlSpec("focus", ink, Color.accent)
    : hot ? Border.controlSpec("hover-cursor", ink, Color.accent)
    : role === "danger" ? Border.flat(Qt.alpha(Color.urgent, 0.6), Math.max(1, Style.normalBorderWidth))
    : quiet ? Border.none()
    : selected && Border.controlHasWidth("selected") ? Border.controlSpec("selected", ink, Color.accent)
    : Border.controlSpec("normal", ink, Color.accent)
  Behavior on color { ColorAnimation { duration: 120 } }
  // A quick press under the pointer, so a click is felt.
  scale: mouseArea.pressed && !blocked ? 0.9 : 1
  Behavior on scale { NumberAnimation { duration: 110; easing.type: Easing.OutCubic } }

  Keys.onReturnPressed: if (focusable) root.clicked()
  Keys.onEnterPressed: if (focusable) root.clicked()
  Keys.onSpacePressed: if (focusable) root.clicked()
  Accessible.role: Accessible.Button
  Accessible.name: label
  Accessible.description: disabledReason
  Accessible.focusable: enabled
  Accessible.onPressAction: if (enabled && !blocked) clicked()
  onActiveFocusChanged: if (activeFocus) Reveal.reveal(root); else pointerFocused = false

  Loader {
    active: root.tooltipText !== "" && mouseArea.containsMouse
    sourceComponent: ToolTip {
      id: tip
      readonly property var borderSpec: Border.localOrSurfaceSpec("tooltip", "border", Color.tooltip.border, Color.tooltip.border, Math.max(1, Style.normalBorderWidth))
      parent: root
      visible: true
      text: root.tooltipText
      delay: 400
      padding: 0
      background: BorderSurface {
        color: Color.tooltip.background
        borderSpec: tip.borderSpec
        radius: 0
      }
      contentItem: Text {
        textFormat: Text.PlainText
        text: root.tooltipText
        color: Color.tooltip.text
        font.family: Style.font.family
        font.pixelSize: Style.font.bodySmall
        leftPadding: Border.left(tip.borderSpec) + Style.spacing.controlPaddingX
        rightPadding: Border.right(tip.borderSpec) + Style.spacing.controlPaddingX
        topPadding: Border.top(tip.borderSpec) + Style.spacing.controlPaddingY
        bottomPadding: Border.bottom(tip.borderSpec) + Style.spacing.controlPaddingY
      }
    }
  }
  MouseArea {
    id: mouseArea
    anchors.fill: parent
    hoverEnabled: true
    cursorShape: Qt.PointingHandCursor
    // Right clicks focus the button and stop there, as on the kit Button.
    acceptedButtons: Qt.LeftButton | Qt.RightButton
    onClicked: function(mouse) {
      if (root.focusable) { if (!root.activeFocus) root.pointerFocused = true; root.forceActiveFocus() }
      if (mouse.button === Qt.LeftButton) root.clicked()
    }
  }
  Row {
    anchors.centerIn: parent
    width: Math.min(implicitWidth, root.width - root.padding * 2)
    spacing: Style.space(5)
    Loader {
      id: symbol
      active: root.glyph !== ""
      visible: active
      width: active ? Style.space(14) : 0
      anchors.verticalCenter: parent.verticalCenter
      sourceComponent: Text {
        text: root.glyph
        textFormat: Text.PlainText
        color: root.labelColor
        font.family: Style.font.family
        font.pixelSize: (root.small ? Style.font.bodySmall : Style.font.body) + Style.space(2)
        horizontalAlignment: Text.AlignHCenter
        Accessible.ignored: true
      }
    }
    Text {
      id: labelText
      anchors.verticalCenter: parent.verticalCenter
      width: Math.min(implicitWidth, root.width - root.padding * 2 - (symbol.active ? symbol.width + parent.spacing : 0))
      text: root.label
      textFormat: Text.PlainText
      wrapMode: Text.NoWrap
      elide: Text.ElideRight
      color: root.labelColor
      font.family: Style.font.family
      font.pixelSize: root.small ? Style.font.bodySmall : Style.font.body
      font.bold: root.selected || root.role === "primary"
    }
  }
  Rectangle {
    anchors.fill: parent
    z: 10
    visible: root.activeFocus || root.hasCursor
    radius: 0
    color: "transparent"
    border.width: Math.max(1, Style.space(2))
    border.color: root.filled ? Color.popups.text : Color.accent
    Accessible.ignored: true
  }
}
