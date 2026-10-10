.pragma library

// The one ThemePalette every Tokens shares, created on first use.
var shared = null

function get(url) {
  if (!shared) {
    var component = Qt.createComponent(url)
    if (component.status !== 1) { console.warn("ibara: theme palette unavailable: " + component.errorString()); return null }
    shared = component.createObject(null)
  }
  return shared
}

// CIELAB (D65) of "#rrggbb", or of "#aarrggbb" (its alpha ignored).
function labOf(hex) {
  var text = String(hex || "").replace(/^#(?:[0-9A-Fa-f]{2})?([0-9A-Fa-f]{6})$/, "$1")
  if (!/^[0-9A-Fa-f]{6}$/.test(text)) return null
  var lin = [0, 2, 4].map(function(at) {
    var c = parseInt(text.substr(at, 2), 16) / 255
    return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4)
  })
  var f = function(t) { return t > 216 / 24389 ? Math.cbrt(t) : (24389 / 27 * t + 16) / 116 }
  var x = f((0.4124 * lin[0] + 0.3576 * lin[1] + 0.1805 * lin[2]) / 0.95047)
  var y = f(0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2])
  var z = f((0.0193 * lin[0] + 0.1192 * lin[1] + 0.9505 * lin[2]) / 1.08883)
  return [116 * y - 16, 500 * (x - y), 200 * (y - z)]
}

// CIEDE2000 between two CIELAB colors: about 2 is barely told apart side by side, 10 or more is
// clearly a different color.
function deltaE2000(lab1, lab2) {
  var rad = Math.PI / 180, deg = 180 / Math.PI
  var C1 = Math.hypot(lab1[1], lab1[2]), C2 = Math.hypot(lab2[1], lab2[2])
  var Cb7 = Math.pow((C1 + C2) / 2, 7)
  var G = 0.5 * (1 - Math.sqrt(Cb7 / (Cb7 + Math.pow(25, 7))))
  var a1 = (1 + G) * lab1[1], a2 = (1 + G) * lab2[1]
  var C1p = Math.hypot(a1, lab1[2]), C2p = Math.hypot(a2, lab2[2])
  var h1 = (Math.atan2(lab1[2], a1) * deg + 360) % 360, h2 = (Math.atan2(lab2[2], a2) * deg + 360) % 360
  var dh = C1p * C2p === 0 ? 0 : Math.abs(h2 - h1) <= 180 ? h2 - h1 : h2 > h1 ? h2 - h1 - 360 : h2 - h1 + 360
  var dL = lab2[0] - lab1[0], dC = C2p - C1p
  var dH = 2 * Math.sqrt(C1p * C2p) * Math.sin(dh / 2 * rad)
  var Lb = (lab1[0] + lab2[0]) / 2, Cbp = (C1p + C2p) / 2
  var hb = C1p * C2p === 0 ? h1 + h2 : Math.abs(h1 - h2) <= 180 ? (h1 + h2) / 2 : h1 + h2 < 360 ? (h1 + h2 + 360) / 2 : (h1 + h2 - 360) / 2
  var T = 1 - 0.17 * Math.cos((hb - 30) * rad) + 0.24 * Math.cos(2 * hb * rad) + 0.32 * Math.cos((3 * hb + 6) * rad) - 0.20 * Math.cos((4 * hb - 63) * rad)
  var Cbp7 = Math.pow(Cbp, 7)
  var Rt = -Math.sin(60 * Math.exp(-Math.pow((hb - 275) / 25, 2)) * rad) * 2 * Math.sqrt(Cbp7 / (Cbp7 + Math.pow(25, 7)))
  var Sl = 1 + 0.015 * Math.pow(Lb - 50, 2) / Math.sqrt(20 + Math.pow(Lb - 50, 2))
  var Sc = 1 + 0.045 * Cbp, Sh = 1 + 0.015 * Cbp * T
  return Math.sqrt(Math.pow(dL / Sl, 2) + Math.pow(dC / Sc, 2) + Math.pow(dH / Sh, 2) + Rt * (dC / Sc) * (dH / Sh))
}

// How different two "#rrggbb" colors look (CIEDE2000); -1 when either isn't a color.
function difference(a, b) {
  var la = labOf(a), lb = labOf(b)
  return la && lb ? deltaE2000(la, lb) : -1
}

// The first of `candidates` (theme colors, most wanted first) that differs from every one of
// `bases` by at least `least`; in a theme where none does, the one whose nearest base is
// farthest. "" when no candidate is a color.
function distinct(bases, candidates, least) {
  var best = "", bestNearest = -1
  for (var i = 0; i < candidates.length; i++) {
    var nearest = Infinity
    for (var b = 0; b < bases.length && nearest >= 0; b++) nearest = Math.min(nearest, difference(bases[b], candidates[i]))
    if (nearest < 0) continue
    if (nearest >= least) return String(candidates[i])
    if (nearest > bestNearest) { best = String(candidates[i]); bestNearest = nearest }
  }
  return best
}
