import QtQuick

// Live Video (Preview) over a computer's picture. Put it inside the SteadyPreview it plays over,
// so it scales with it and the picture's moments (PictureMoments) stay drawn above it. The
// picture stays under it as the poster: the video shows only once its frames move.
// While `shown`, it asks the service for the stream now and every 5 s (the service sends
// `ibara video open`, which ibarad closes unless renewed within 10 s), and lets it go when hidden
// or off. It never says anything when video fails: an error, or no frames for a minute, puts the
// pictures back and the service tries again a minute later; frames that stop for 6 s hide the
// video until they move again. The player lives in LiveVideoPlayer.qml, loaded here, so a
// desktop without QtMultimedia only loses video.
Item {
  id: root
  property var service: null
  property string computerId: ""
  // The picture it plays over: its shown size and its screen's shape pick the stream's size.
  property Item preview: null
  // Whether the place it sits in shows now: a fleet card in view, or the Screen tab open.
  property bool shown: false
  anchors.fill: parent
  z: 2
  Accessible.ignored: true

  property string holder: ""
  readonly property bool wanted: shown && visible && !!service && service.videoAllowed(computerId)
  readonly property var size: wanted && preview && service ? service.videoSize(preview.shownWidth, preview.shownHeight, preview.frameAspect) : null
  readonly property string sizeText: size ? size.width + "x" + size.height : ""
  readonly property var stream: sizeText && service ? service.videoStreams[computerId + ":" + sizeText] || null : null
  readonly property string source: stream ? "file://" + stream.path : ""

  // The video shows while its frames move.
  property bool playing: false
  property double lastMoved: 0
  property double loadedAt: 0

  function renew() {
    if (!service) return
    if (!holder) holder = service.newVideoHolder()
    if (wanted && sizeText) service.holdVideo(holder, computerId, size.width, size.height)
    else service.releaseVideo(holder)
  }
  function setPlaying(value) {
    if (playing === value) return
    playing = value
    if (service && holder) service.setVideoPlaying(holder, computerId, value)
  }
  function fail() {
    setPlaying(false)
    if (service) service.videoFailed(computerId)
  }
  function check() {
    var now = Date.now()
    if (playing && now - lastMoved > 6000) setPlaying(false)
    if (!playing && now - Math.max(lastMoved, loadedAt) > 60000) fail()
  }

  onWantedChanged: renew()
  onSizeTextChanged: renew()
  onComputerIdChanged: { setPlaying(false); renew() }
  onSourceChanged: { setPlaying(false); loadedAt = Date.now() }
  Component.onDestruction: if (service && holder) service.releaseVideo(holder)
  Timer { interval: 5000; repeat: true; running: root.wanted; onTriggered: root.renew() }
  Timer { interval: 1000; repeat: true; running: player.status === Loader.Ready; onTriggered: root.check() }

  Loader {
    id: player
    anchors.fill: parent
    active: root.source !== "" && !!root.service && root.service.videoPlayerWorks
    source: Qt.resolvedUrl("LiveVideoPlayer.qml")
    opacity: root.playing ? 1 : 0
    Behavior on opacity { NumberAnimation { duration: 300; easing.type: Easing.InOutQuad } }
    onLoaded: { root.loadedAt = Date.now(); item.source = Qt.binding(function() { return root.source }) }
    onActiveChanged: if (!active) root.setPlaying(false)
    // QtMultimedia missing or broken here: pictures only, everywhere, without a word.
    onStatusChanged: if (status === Loader.Error && root.service) root.service.videoPlayerWorks = false
  }
  Connections {
    target: player.item
    ignoreUnknownSignals: true
    function onMoved() {
      root.lastMoved = Date.now()
      root.setPlaying(true)
    }
    function onFailed() { root.fail() }
  }
}
