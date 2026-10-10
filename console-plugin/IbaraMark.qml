import QtQuick
import QtQuick.Shapes
import qs.Commons

// Design-reference paths, with a 16-grid hint at bar size. The rear never enters the front
// footprint; the i is transparent, not painted with an assumed background.
Item {
  id: root
  property color color: Color.foreground
  property bool barSize: width <= Style.space(18)
  property bool compact: width <= Style.space(32)
  implicitWidth: Style.space(32)
  implicitHeight: implicitWidth
  Accessible.ignored: true

  Shape {
    width: root.barSize ? 16 : root.compact ? 32 : 128
    height: width
    anchors.centerIn: parent
    scale: Math.min(root.width, root.height) / width
    preferredRendererType: Shape.CurveRenderer
    ShapePath {
      strokeColor: "transparent"
      fillColor: root.color
      PathSvg { path: root.barSize ? "M2 2H12V5H10V4H4V10H5V12H2Z" : root.compact ? "M6 5H23V10H20V8H9V19H11V22H6Z" : "M24 20H90V40H80V30H34V76H44V86H24Z" }
    }
    ShapePath {
      strokeColor: "transparent"
      fillColor: root.color
      fillRule: ShapePath.OddEvenFill
      PathSvg { path: root.barSize ? "M5 5H15V15H5Z M9 7H11V9H9Z M9 10H11V14H9Z" : root.compact ? "M11 10H28V27H11Z M17 13H21V17H17Z M17 18H21V25H17Z" : "M44 40H110V106H44Z M70 54H82V66H70Z M70 71H82V96H70Z" }
    }
  }
}
