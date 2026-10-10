import QtQuick
import "Reveal.js" as Reveal

// Equal-height rows inside a scrolling Column, built only while they are inside `flick`'s
// view (plus the row holding focus), each exactly where a Column would put it.
// The view builds rows in the order they scroll in, so the focus chain cannot run through
// them: each focusable part of a row hands Tab and Backtab to step() and reports focus to
// focused(), and a gate at each end passes focus arriving from outside to the first or
// last row. Delegates reach this item as `ListView.view.parent`.
Item {
  id: root
  required property Flickable flick
  property var model
  property alias delegate: view.delegate
  property int rowHeight: 0
  property alias spacing: view.spacing
  readonly property int count: view.count
  // Where the rows start in flick's content.
  readonly property real contentTop: {
    var y = root.y
    for (var item = parent; item && item !== flick.contentItem; item = item.parent) y += item.y
    return y
  }
  height: count > 0 ? count * (rowHeight + spacing) - spacing : 0
  // The part of the rows inside flick's view. The view covers only this slice and scrolls with
  // flick, so it builds only these rows.
  readonly property int sliceTop: Math.max(0, Math.min(height, Math.floor(flick.contentY - contentTop)))
  readonly property int sliceBottom: Math.max(sliceTop, Math.min(height, Math.ceil(flick.contentY + flick.height - contentTop)))
  onSliceTopChanged: view.contentY = sliceTop
  // A new model makes the view start over from row 0 and build rows there at once, and a
  // count change inside that reset can move the view before it builds. So the view takes a
  // new model while it has no height, moves to the slice, and builds the slice when its
  // height returns.
  property bool loading: false
  function load() {
    loading = true
    view.model = model
    view.contentY = sliceTop
    loading = false
  }
  onModelChanged: load()
  Component.onCompleted: load()
  // A new model replaces every row, and a focused row leaves focus with the view, which would
  // hand it to its new first row. As with rows built straight into a Column, focus returns to
  // the enclosing scope instead.
  Connections {
    target: root.Window
    function onActiveFocusItemChanged() { if (root.Window.activeFocusItem === view) view.focus = false }
  }

  function rowOf(item) {
    while (item && item.parent !== view.contentItem) item = item.parent
    return item
  }
  function inside(item, row) {
    for (; item; item = item.parent) if (item === row) return true
    return false
  }
  function firstFocusable(item) {
    if (item.activeFocusOnTab && item.visible && item.enabled) return item
    for (var i = 0; i < item.children.length; i++) {
      var found = firstFocusable(item.children[i])
      if (found) return found
    }
    return null
  }
  function lastFocusable(item) {
    for (var i = item.children.length - 1; i >= 0; i--) {
      var found = lastFocusable(item.children[i])
      if (found) return found
    }
    return item.activeFocusOnTab && item.visible && item.enabled ? item : null
  }
  // The current row is never released, so focus survives scrolling it out of view. The view
  // gives focus to each new current row, so it lets go of focus while the row changes.
  function makeCurrent(row) {
    view.focus = false
    view.currentIndex = row
  }
  function enter(row, forward) {
    makeCurrent(row)
    var target = forward ? firstFocusable(view.currentItem) : lastFocusable(view.currentItem)
    target.forceActiveFocus(forward ? Qt.TabFocusReason : Qt.BacktabFocusReason)
  }
  // Reveal.js leaves item views alone, so the column reveals a stand-in placed over the part.
  function focused(row, part) {
    if (view.currentIndex !== row) {
      makeCurrent(row)
      part.forceActiveFocus()
      return
    }
    revealTarget.y = row * (rowHeight + spacing) + part.mapToItem(rowOf(part), 0, 0).y
    revealTarget.height = part.height
    Reveal.reveal(revealTarget)
  }
  // Tab or Backtab from `item` in `row`: inside the row the focus chain still holds; past it,
  // move by index, and past the last or first row, continue the chain from the gates.
  function step(row, item, event) {
    if (event.modifiers & (Qt.ControlModifier | Qt.AltModifier)) return false
    var forward = event.key === Qt.Key_Tab && !(event.modifiers & Qt.ShiftModifier)
    if (inside(item.nextItemInFocusChain(forward), rowOf(item))) return false
    var next = row + (forward ? 1 : -1)
    if (next >= 0 && next < count) enter(next, forward)
    else (forward ? endGate : startGate).nextItemInFocusChain(forward).forceActiveFocus(forward ? Qt.TabFocusReason : Qt.BacktabFocusReason)
    return true
  }

  Item { id: startGate; activeFocusOnTab: root.count > 0; onActiveFocusChanged: if (activeFocus) root.enter(0, true) }
  ListView {
    id: view
    y: root.sliceTop
    width: parent.width
    height: root.loading ? 0 : root.sliceBottom - root.sliceTop
    interactive: false
    keyNavigationEnabled: false
    cacheBuffer: 0
    highlightFollowsCurrentItem: false
  }
  Item { id: revealTarget; width: parent.width }
  Item { id: endGate; activeFocusOnTab: root.count > 0; onActiveFocusChanged: if (activeFocus) root.enter(root.count - 1, false) }
}
