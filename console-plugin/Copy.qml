import QtQuick
import qs.Commons

// Plain text in the console's voice. `eyebrow` gives the small uppercase section label.
Text {
  property bool eyebrow: false
  property bool dimmed: false
  textFormat: Text.PlainText
  wrapMode: Text.Wrap
  color: eyebrow || dimmed ? Qt.alpha(Color.popups.text, eyebrow ? 0.5 : 0.64) : Color.popups.text
  font.family: Style.font.family
  font.pixelSize: eyebrow ? Style.font.caption : Style.font.body
  font.letterSpacing: eyebrow ? Style.space(1) : 0
  font.capitalization: eyebrow ? Font.AllUppercase : Font.MixedCase
}
