pragma Singleton
import QtQuick

QtObject {
  function none() { return { color: "transparent", width: 0 } }
  function flat(color, width) { return { color: color, width: width } }
  function controlHasWidth(state) { return true }
  function controlSpec(state, ink, accent) { return flat(state === "focus" ? accent : Qt.alpha(ink, 0.3), state === "focus" ? 2 : 1) }
  function localOrSurfaceSpec(surface, name, color, fallback, width) { return flat(color || fallback, width) }
  function left(spec) { return spec ? spec.width : 0 }
  function right(spec) { return left(spec) }
  function top(spec) { return left(spec) }
  function bottom(spec) { return left(spec) }
}
