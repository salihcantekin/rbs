// Qt Quick prototype UI. Geometry comes from the shared dock-model crate.
import QtQuick
import QtQuick.Controls
import QtQuick.Shapes
import com.rbs.proto

ApplicationWindow {
    id: root
    title: "RBS prototype (Qt)"
    width: 1280; height: 800
    visible: true
    color: "#16181d"

    Backend { id: backend }
    // Presented-frame timestamps for the measurement mode (RBS_PROTO_MEASURE).
    onFrameSwapped: backend.presented()
    readonly property var stacks: JSON.parse(backend.stacks || "[]")
    readonly property var handles: JSON.parse(backend.handles || "[]")
    readonly property var panels: JSON.parse(backend.panels || "[]")
    readonly property var floats: JSON.parse(backend.floats || "[]")

    // Frame ticks only while an animated panel is visible (ADR-0013).
    Timer { interval: 16; repeat: true; running: backend.animating; onTriggered: backend.tick() }

    // Only the active panel's content is instantiated, so hidden panels cost nothing.
    component PanelContent: Rectangle {
        property string panel
        color: "#16181d"
        Loader {
            anchors.fill: parent
            sourceComponent: panel === "rbs.program" ? programC : panel === "rbs.mixer" ? mixerC : placeholderC
        }
        Component {
            id: programC
            ProgramItem {
                frame: backend.frame
                Accessible.role: Accessible.Graphic
                Accessible.name: "Program output"
            }
        }
        Component {
            id: mixerC
            Shape {
                transform: Scale { xScale: width / 1000; yScale: height / 1000 }
                ShapePath { fillColor: "#994c8dff"; strokeColor: "transparent"; PathSvg { path: backend.bars } }
                ShapePath { fillColor: "transparent"; strokeColor: "white"; strokeWidth: 1.5; PathSvg { path: backend.spectrum } }
            }
        }
        Component {
            id: placeholderC
            Item { Text { anchors.centerIn: parent; text: panel === "rbs.live-controls" ? "Go live: ابدأ البث الآن (3 outputs) · שידור חי 12:30" : panel; color: "#9aa1ad" } }
        }
    }

    component StackView: Rectangle {
        property var info
        property bool floating: false
        color: "#20232a"; border.color: "#2a2e37"
        Row {
            id: bar; height: 28; spacing: 2; x: 4
            Repeater {
                model: info ? info.tabs : []
                delegate: Rectangle {
                    required property var modelData
                    required property int index
                    width: label.implicitWidth + 70; height: 28
                    color: index === info.active ? "#2a2e37" : "transparent"
                    Accessible.role: Accessible.PageTab
                    Accessible.name: modelData.title
                    MouseArea { anchors.fill: parent; onClicked: backend.activate(info.win, info.stack, index) }
                    Text { id: label; x: 8; anchors.verticalCenter: parent.verticalCenter; text: modelData.title; color: index === info.active ? "#e6e8ee" : "#9aa1ad" }
                    Row {
                        anchors.right: parent.right; anchors.rightMargin: 4; anchors.verticalCenter: parent.verticalCenter; spacing: 8
                        Text { visible: !floating; text: "→"; color: "#9aa1ad"; MouseArea { anchors.fill: parent; onClicked: backend.movenext(modelData.id) } }
                        Text { visible: !floating; text: "⧉"; color: "#9aa1ad"; MouseArea { anchors.fill: parent; onClicked: backend.floatpanel(modelData.id) } }
                        Text { text: "×"; color: "#9aa1ad"; MouseArea { anchors.fill: parent; onClicked: backend.closepanel(modelData.id) } }
                    }
                }
            }
        }
        PanelContent {
            x: 1; y: 28; width: parent.width - 2; height: parent.height - 29
            panel: info && info.tabs.length > 0 ? info.tabs[info.active].id : ""
        }
    }

    header: Rectangle {
        height: 36; color: "#2a2e37"
        Row {
            anchors.fill: parent; anchors.margins: 4; spacing: 6
            Rectangle {
                width: live.implicitWidth + 16; height: parent.height; color: "#e5484d"; radius: 4
                Accessible.role: Accessible.StaticText; Accessible.name: live.text
                Text { id: live; anchors.centerIn: parent; text: "LIVE 00:12:34 · 2 outputs"; color: "white"; font.bold: true }
            }
            Text { anchors.verticalCenter: parent.verticalCenter; text: "Workspace: " + backend.workspace; color: "#e6e8ee" }
            Button { text: "Edit"; height: parent.height; onClicked: backend.switchto("Edit") }
            Button { text: "Minimal"; height: parent.height; onClicked: backend.switchto("Minimal") }
            Button { text: "Save"; height: parent.height; onClicked: backend.save() }
            Button { text: "Load"; height: parent.height; onClicked: backend.load() }
            Text { anchors.verticalCenter: parent.verticalCenter; text: "Panels:"; color: "#9aa1ad" }
            Repeater {
                model: root.panels
                delegate: Button {
                    required property var modelData
                    height: parent.height
                    text: (modelData.open ? "✓ " : "") + modelData.title
                    highlighted: modelData.open
                    onClicked: backend.toggle(modelData.id)
                }
            }
        }
    }

    Item {
        id: area
        anchors.fill: parent
        onWidthChanged: backend.relayout(width, height)
        onHeightChanged: backend.relayout(width, height)
        Component.onCompleted: backend.relayout(width, height)
        Repeater {
            model: root.stacks
            delegate: StackView {
                required property var modelData
                x: modelData.x; y: modelData.y; width: modelData.w; height: modelData.h
                info: modelData
            }
        }
        Repeater {
            model: root.handles
            delegate: Rectangle {
                required property var modelData
                x: modelData.x; y: modelData.y; width: modelData.w; height: modelData.h
                color: ma.containsMouse || ma.pressed ? "#4c8dff" : "#16181d"
                Accessible.role: Accessible.Slider; Accessible.name: "Resize split"
                MouseArea {
                    id: ma; anchors.fill: parent; hoverEnabled: true
                    cursorShape: modelData.horizontal ? Qt.SizeHorCursor : Qt.SizeVerCursor
                    property real px; property real py
                    onPressed: (m) => { px = m.x; py = m.y }
                    onPositionChanged: (m) => { if (pressed) backend.drag(modelData.index, modelData.horizontal ? m.x - px : m.y - py) }
                }
            }
        }
    }

    Instantiator {
        model: root.floats
        delegate: Window {
            required property var modelData
            title: "RBS prototype (Qt) - floating"
            width: 640; height: 400; visible: true; color: "#16181d"
            Button { id: dockBtn; text: "Dock back"; onClicked: backend.dock(modelData.win) }
            StackView { y: 32; width: parent.width; height: parent.height - 32; info: modelData; floating: true }
        }
    }
}
