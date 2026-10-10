import QtQuick

Rectangle {
  property var borderSpec: ({ color: "transparent", width: 0 })
  border.color: borderSpec ? borderSpec.color : "transparent"
  border.width: borderSpec ? borderSpec.width : 0
}
