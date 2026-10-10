import QtQuick
import QtQuick.Window

// Keep the last decoded frame visible while the next one loads. A new frame
// must not blank the tile, and an empty incoming value must not clear it.
// Frames are raw PPM files that ibarad writes at the size this item shows them,
// so they decode on the GUI thread in a few milliseconds: Qt's asynchronous
// image reader kept about 15 MiB in the shell after the console closed. Once
// the new frame shows, the old layer lets go of its picture.
// The shown size (device pixels, rounded up to 64-pixel steps, at most
// 1920 × 1080) settles 250 ms after a resize stops. shownSizeSettled reports it
// then and whenever this item becomes visible, so the service asks ibarad for
// frames of that size.
Item {
  id: root
  property string incoming: ""
  property bool drop: false
  readonly property real devicePixels: Screen.devicePixelRatio > 0 ? Screen.devicePixelRatio : 1
  property int shownWidth: 0
  property int shownHeight: 0
  property string sourceA: ""
  property string sourceB: ""
  property bool showB: false
  // While a new picture fades in on top, the old one stays under it; then it lets go.
  property bool fading: false
  Timer {
    id: fadeDone
    interval: 420
    onTriggered: {
      root.fading = false
      if (root.showB) root.sourceA = ""
      else root.sourceB = ""
    }
  }
  function reveal(b) {
    fading = true
    showB = b
    fadeDone.restart()
  }
  readonly property bool hasFrame: layerA.opacity === 1 || layerB.opacity === 1
  // The shown picture's width over its height (the screen's shape), or 0 before one shows.
  readonly property real frameAspect: {
    var layer = layerB.opacity === 1 ? layerB : layerA.opacity === 1 ? layerA : null
    return layer && layer.implicitHeight > 0 ? layer.implicitWidth / layer.implicitHeight : 0
  }
  signal shownSizeSettled(int width, int height)

  function noteIncoming() {
    if (drop) {
      sourceA = ""
      sourceB = ""
      showB = false
      return
    }
    var next = incoming
    if (!next || next === (showB ? sourceB : sourceA)) return
    if (showB) sourceA = next
    else sourceB = next
  }
  onIncomingChanged: noteIncoming()
  onDropChanged: noteIncoming()

  function settleShownSize() {
    if (width <= 0 || height <= 0) return
    shownWidth = Math.min(1920, Math.max(64, Math.ceil(width * devicePixels / 64) * 64))
    shownHeight = Math.min(1080, Math.max(64, Math.ceil(height * devicePixels / 64) * 64))
    if (visible) shownSizeSettled(shownWidth, shownHeight)
  }
  // The first size applies at once; later ones wait until the resize stops.
  function noteSize() {
    if (shownWidth > 0) settle.restart()
    else settleShownSize()
  }
  onWidthChanged: noteSize()
  onHeightChanged: noteSize()
  onDevicePixelsChanged: noteSize()
  onVisibleChanged: if (visible && shownWidth > 0) shownSizeSettled(shownWidth, shownHeight)
  Component.onCompleted: settleShownSize()
  Timer { id: settle; interval: 250; onTriggered: root.settleShownSize() }

  Image {
    id: layerA
    // True once this layer has shown its current source; a reload of the same source keeps it.
    property bool painted: false
    anchors.fill: parent
    fillMode: Image.PreserveAspectFit
    asynchronous: false
    cache: false
    retainWhileLoading: true
    source: root.shownWidth > 0 ? root.sourceA : ""
    z: root.showB ? 0 : 1
    opacity: (!root.showB || root.fading) && painted && status !== Image.Error ? 1 : 0
    Behavior on opacity { enabled: root.fading; NumberAnimation { duration: 400; easing.type: Easing.InOutQuad } }
    onSourceChanged: painted = false
    onStatusChanged: {
      if (status !== Image.Ready) return
      painted = true
      if (root.sourceA && root.sourceA === root.incoming) root.reveal(false)
    }
  }
  Image {
    id: layerB
    property bool painted: false
    anchors.fill: parent
    fillMode: Image.PreserveAspectFit
    asynchronous: false
    cache: false
    retainWhileLoading: true
    source: root.shownWidth > 0 ? root.sourceB : ""
    z: root.showB ? 1 : 0
    opacity: (root.showB || root.fading) && painted && status !== Image.Error ? 1 : 0
    Behavior on opacity { enabled: root.fading; NumberAnimation { duration: 400; easing.type: Easing.InOutQuad } }
    onSourceChanged: painted = false
    onStatusChanged: {
      if (status !== Image.Ready) return
      painted = true
      if (root.sourceB && root.sourceB === root.incoming) root.reveal(true)
    }
  }
}
