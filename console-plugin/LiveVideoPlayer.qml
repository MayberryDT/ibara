import QtQuick
import QtMultimedia

// The Live Video player: LiveVideo.qml loads it, so a desktop without QtMultimedia only loses
// video. It plays the pipe ibarad writes the computer's stream into. When the stream ends
// (ibarad starts it again after the computer's encoder restarted), it opens the pipe again.
// moved: a new frame showed. failed: the player gave up.
Item {
  id: root
  property url source: ""
  signal moved()
  signal failed()

  function start() {
    player.stop()
    player.source = ""
    if (String(source) === "") return
    player.source = source
    player.play()
  }
  onSourceChanged: start()
  Component.onDestruction: player.stop()

  MediaPlayer {
    id: player
    videoOutput: output
    // Live: show each frame as it arrives rather than buffering ahead of the screen.
    playbackOptions.playbackIntent: PlaybackOptions.LowLatencyStreaming
    onPositionChanged: if (position > 0) root.moved()
    onErrorOccurred: root.failed()
    onMediaStatusChanged: if (mediaStatus === MediaPlayer.EndOfMedia) reopen.restart()
  }
  VideoOutput {
    id: output
    anchors.fill: parent
    fillMode: VideoOutput.PreserveAspectFit
  }
  Timer { id: reopen; interval: 500; onTriggered: root.start() }
}
