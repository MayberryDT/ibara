import QtQuick
import qs.Commons
import "StatusModel.js" as StatusModel

// How old a tab's data is. Coming back to a computer shows its tabs' last data at once; this
// says when that data was read and whether ibara is reading it again. Nothing read yet, nothing
// shown: the tab's own loading text covers that.
Copy {
  property double at: 0
  property bool refreshing: false
  property double nowMs: Date.now()
  visible: at > 0
  text: at > 0 ? "Updated " + StatusModel.ageLabel(new Date(at).toISOString(), nowMs) + (refreshing ? " · refreshing…" : "") : ""
  dimmed: true
  font.pixelSize: Style.font.caption
  wrapMode: Text.NoWrap
}
