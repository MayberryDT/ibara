.pragma library

// Scroll the nearest enclosing Flickable so a newly focused control is fully visible.
function reveal(item) {
  if (!item) return
  var flick = item.parent
  while (flick && !(flick.contentItem !== undefined && flick.contentY !== undefined && flick.flickableDirection !== undefined)) flick = flick.parent
  // Item views keep their current delegate in view themselves.
  if (!flick || !flick.contentItem || flick.model !== undefined) return
  var point = item.mapToItem(flick.contentItem, 0, 0)
  var margin = 8
  if (point.y < flick.contentY) flick.contentY = Math.max(0, point.y - margin)
  else if (point.y + item.height > flick.contentY + flick.height)
    flick.contentY = Math.max(0, Math.min(flick.contentHeight - flick.height, point.y + item.height - flick.height + margin))
}
