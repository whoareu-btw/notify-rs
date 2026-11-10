//@ pragma UseQApplication
//
// Retro "Window Maker" style Quickshell bar for labwc
// ---------------------------------------------------
// Install:  mkdir -p ~/.config/quickshell && cp shell-labwc.qml ~/.config/quickshell/shell.qml
// Run:      qs   (or: quickshell)
// Autostart: add these lines to ~/.config/labwc/autostart
//              notify-rs daemon --theme retro >"$XDG_RUNTIME_DIR/notify-rs.log" 2>&1 &
//              qs >"$XDG_RUNTIME_DIR/quickshell.log" 2>&1 &
//            (not /tmp: a predictable file name there lets another local user plant a
//             symlink and make your session overwrite your own files. $XDG_RUNTIME_DIR
//             is private to you. `qs log` also shows the log.)
//
// notify-rs (notifications + media card) needs `notify-rs daemon` running and no other
// notification daemon (mako, dunst...). Do not add Quickshell's own NotificationServer:
// only one program can own the notification name.
//
// Needs: a Quickshell release that has Quickshell.WindowManager (workspaces
// through ext-workspace-v1). The battery is read from /sys, no upower needed.
// Volume is read from PipeWire (PipeWire or its pulse/wireplumber setup must run).
// Taskbar uses wlr-foreign-toplevel-management, which labwc provides.
//
// Icons come from the Qt icon theme. If most of them show as letter tiles, no
// icon theme is installed/selected: install one (Papirus, Adwaita...) and set
// the QS_ICON_THEME environment variable to its name before starting qs.
//
// Mouse on the task icons:
//   left   = focus, or minimize if it is already focused
//   middle = close the window

import Quickshell
import Quickshell.Io
import Quickshell.Widgets
import Quickshell.Wayland
import Quickshell.Services.Pipewire
import Quickshell.WindowManager
import Quickshell.Services.SystemTray
import QtQuick
import QtQuick.Layouts

ShellRoot {
    id: root

    // ------------------------------------------------------------ data
    property real cpu: 0          // 0..1
    property real mem: 0          // 0..1
    property real prevTotal: 0
    property real prevIdle: 0

    // battery, read straight from /sys (no upower)
    property bool hasBat: false
    property bool charging: false
    property bool discharging: false
    property real batPct: 0       // 0..1
    // only while really running on the battery ("Not charging", "Full" and
    // "Unknown" are what a plugged-in laptop reports)
    readonly property bool batLow: hasBat && discharging && batPct < 0.2

    function parseBat(text) {
        const m = /^(\d+)\s+(.*)$/.exec(text.trim())
        hasBat = m !== null
        if (m) {
            batPct = Math.max(0, Math.min(1, Number(m[1]) / 100))
            charging = m[2] === "Charging"
            discharging = m[2] === "Discharging"
        }
    }

    function parseStat(text) {
        const f = text.split("\n")[0].trim().split(/\s+/).slice(1, 9).map(Number)
        const idle = f[3] + f[4]
        const total = f.reduce((a, b) => a + b, 0)
        const dTotal = total - prevTotal
        const dIdle = idle - prevIdle
        if (prevTotal > 0 && dTotal > 0)
            cpu = Math.max(0, Math.min(1, 1 - dIdle / dTotal))
        prevTotal = total
        prevIdle = idle
    }

    function parseMem(text) {
        const total = /MemTotal:\s+(\d+)/.exec(text)
        const avail = /MemAvailable:\s+(\d+)/.exec(text)
        if (total && avail)
            mem = Math.max(0, Math.min(1, 1 - avail[1] / total[1]))
    }

    FileView { id: statFile; path: "/proc/stat";    onLoaded: root.parseStat(text()) }
    FileView { id: memFile;  path: "/proc/meminfo"; onLoaded: root.parseMem(text()) }

    Timer {
        interval: 1000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: { statFile.reload(); memFile.reload() }
    }

    // finds the first BAT* in /sys/class/power_supply and prints "<percent> <status>"
    Process {
        id: batProc
        command: ["sh", "-c",
            "for d in /sys/class/power_supply/BAT*; do [ -r \"$d/capacity\" ] || continue; echo \"$(cat \"$d/capacity\") $(cat \"$d/status\")\"; exit; done"]
        stdout: StdioCollector { onStreamFinished: root.parseBat(this.text) }
    }

    Timer {
        interval: 5000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: batProc.running = true
    }

    // volume of the default output, read from PipeWire (display only)
    readonly property var sink: Pipewire.defaultAudioSink
    readonly property bool hasVol: !!(sink && sink.audio)
    readonly property bool muted: hasVol && sink.audio.muted
    readonly property real vol: hasVol ? sink.audio.volume : 0     // can go above 1

    // the node has to be tracked before its audio values are valid
    PwObjectTracker { objects: root.sink ? [root.sink] : [] }

    SystemClock { id: clock; precision: SystemClock.Seconds }

    // Icon for a taskbar button. The app id comes from the application itself, so it
    // is untrusted: it may only ever be used as a plain icon *name*, never as a path.
    // An absolute path is accepted from the app's desktop entry only (a file you
    // installed), and only if it looks like an image file.
    readonly property var iconNameOk: /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/
    readonly property var iconFileOk: /^\/[^?#\u0000]*\.(png|svg|svgz|xpm)$/i

    function taskIcon(appId, entryIcon) {
        if (entryIcon && entryIcon.startsWith("/") && iconFileOk.test(entryIcon)
                && entryIcon.indexOf("/../") < 0)
            return "file://" + encodeURI(entryIcon)
        for (const n of [entryIcon, appId, appId ? appId.toLowerCase() : ""]) {
            if (!n || !iconNameOk.test(n))
                continue
            const p = Quickshell.iconPath(n, true)
            if (p)
                return p
        }
        return ""
    }

    // ----------------------------------------------------------- notify-rs
    // `notify-rs bell` prints one JSON line per change. What is read from it is
    // checked before use and drawn as plain text.
    readonly property string notifyRs: "notify-rs"      // or an absolute path

    property string bellText: ""          // the bell glyph ("" while the daemon is not running)

    function parseBell(line) {
        let j
        try { j = JSON.parse(line) } catch (e) { return }
        if (j !== null && typeof j === "object" && typeof j.text === "string")
            bellText = j.text.trim().slice(0, 4)
    }

    Process {
        id: bellProc
        command: [root.notifyRs, "bell"]
        running: true
        stdout: SplitParser { splitMarker: "\n"; onRead: data => root.parseBell(data) }
        onExited: bellRetry.restart()       // the feed should never end; come back if it does
    }
    Timer { id: bellRetry; interval: 2000; onTriggered: bellProc.running = true }

    // fire and forget, no shell: every argument is its own list item
    function nr(args) {
        Quickshell.execDetached([notifyRs].concat(args))
    }

    // ------------------------------------------------- retro building blocks
    // Palette: face #aeaaae, highlight #ffffff, shadow #555555, outline #000000,
    // title blue #505075, LED green #3cd23c.

    // A bevelled box. raised by default, sunken when `sunken: true`.
    component Bevel: Rectangle {
        id: bv
        property bool sunken: false
        property color face: "#aeaaae"
        readonly property color hi: "#ffffff"
        readonly property color lo: "#555555"

        color: face
        border.width: 1
        border.color: "#000000"

        // top + left edges
        Rectangle { x: 1; y: 1; width: bv.width - 2; height: 1; color: bv.sunken ? bv.lo : bv.hi }
        Rectangle { x: 1; y: 1; width: 1; height: bv.height - 2; color: bv.sunken ? bv.lo : bv.hi }
        // bottom + right edges
        Rectangle { x: 1; y: bv.height - 2; width: bv.width - 2; height: 1; color: bv.sunken ? bv.hi : bv.lo }
        Rectangle { x: bv.width - 2; y: 1; width: 1; height: bv.height - 2; color: bv.sunken ? bv.hi : bv.lo }
    }

    // Plain text. Swap the family for "Terminus", "Misc Fixed", "Nimbus Sans"...
    component Lbl: Text {
        color: "#000000"
        font.family: "monospace"
        font.pixelSize: 12
        renderType: Text.NativeRendering
        textFormat: Text.PlainText      // workspace names and app ids are not ours to interpret
    }

    // Dockapp-style LCD readout: label, 10 LED segments, value.
    component Meter: Item {
        id: m
        property string label: ""
        property real value: 0
        property string valueText: Math.round(value * 100) + "%"
        property color lit: "#3cd23c"
        property int segments: 10

        implicitWidth: Math.ceil(row.implicitWidth) + 14
        implicitHeight: 22

        Bevel { anchors.fill: parent; sunken: true; face: "#0c140c" }

        RowLayout {
            id: row
            anchors.centerIn: parent
            spacing: 6

            Lbl { text: m.label; color: "#4f8f4f" }

            Row {
                Layout.alignment: Qt.AlignVCenter
                spacing: 1
                Repeater {
                    model: m.segments
                    Rectangle {
                        required property int index
                        width: 4
                        height: 10
                        color: index < Math.round(m.value * m.segments) ? m.lit : "#14301a"
                    }
                }
            }

            Lbl {
                text: m.valueText
                color: m.lit
                horizontalAlignment: Text.AlignRight
                Layout.preferredWidth: 40
            }
        }
    }

    // ------------------------------------------------------------- the bar
    Variants {
        model: Quickshell.screens

        PanelWindow {
            id: bar
            required property var modelData
            screen: modelData

            anchors { top: true; left: true; right: true }
            implicitHeight: 30
            color: "#aeaaae"

            Bevel { anchors.fill: parent }

            RowLayout {
                anchors { fill: parent; margins: 4 }
                spacing: 4

                // ---- workspaces (labwc, via ext-workspace) ----
                RowLayout {
                    Layout.fillHeight: true
                    spacing: 2

                    Repeater {
                        model: WindowManager.windowsets

                        Bevel {
                            id: ws
                            required property int index
                            required property var modelData
                            readonly property bool current: modelData.active
                            readonly property bool urgent: modelData.urgent
                            // short names are shown as they are, long ones as a number
                            readonly property string label: modelData.name.length <= 3
                                ? modelData.name : String(index + 1)

                            visible: modelData.shouldDisplay
                            Layout.fillHeight: true
                            Layout.preferredWidth: Math.max(26, Math.ceil(wsText.implicitWidth) + 12)
                            sunken: current || wsMouse.pressed
                            face: current ? "#505075" : "#aeaaae"

                            Lbl {
                                id: wsText
                                anchors.centerIn: parent
                                text: ws.label
                                color: ws.current ? "#ffffff" : (ws.urgent ? "#b00000" : "#000000")
                            }

                            MouseArea {
                                id: wsMouse
                                anchors.fill: parent
                                onClicked: {
                                    if (ws.modelData.canActivate)
                                        ws.modelData.activate()
                                }
                            }
                        }
                    }
                }

                // ---- groove between workspaces and tasks ----
                Row {
                    Layout.fillHeight: true
                    Layout.topMargin: 2
                    Layout.bottomMargin: 2
                    Rectangle { width: 1; height: parent.height; color: "#555555" }
                    Rectangle { width: 1; height: parent.height; color: "#ffffff" }
                }

                // ---- taskbar: running apps, icons only ----
                RowLayout {
                    Layout.fillHeight: true
                    spacing: 2

                    Repeater {
                        model: ToplevelManager.toplevels

                        Bevel {
                            id: task
                            required property var modelData
                            // reading the list length makes this run again once the
                            // desktop entries have finished loading
                            readonly property var entry: DesktopEntries.applications.values.length >= 0
                                ? DesktopEntries.heuristicLookup(modelData.appId) : null

                            // first icon that really exists, or "" if none does
                            readonly property string iconSource:
                                root.taskIcon(modelData.appId, entry ? entry.icon : "")

                            Layout.fillHeight: true
                            Layout.preferredWidth: 26
                            sunken: modelData.activated || taskMouse.pressed

                            IconImage {
                                anchors.centerIn: parent
                                width: 16
                                height: 16
                                visible: task.iconSource !== ""
                                opacity: task.modelData.minimized ? 0.45 : 1
                                source: task.iconSource
                            }

                            // no icon found: small bevel-style tile with the app's first letter
                            Rectangle {
                                anchors.centerIn: parent
                                width: 16
                                height: 16
                                visible: task.iconSource === ""
                                opacity: task.modelData.minimized ? 0.45 : 1
                                color: "#d8d4d8"
                                border.width: 1
                                border.color: "#000000"

                                Lbl {
                                    anchors.centerIn: parent
                                    font.pixelSize: 11
                                    font.bold: true
                                    text: (task.modelData.appId || "?").charAt(0).toUpperCase()
                                }
                            }

                            MouseArea {
                                id: taskMouse
                                anchors.fill: parent
                                acceptedButtons: Qt.LeftButton | Qt.MiddleButton

                                onClicked: mouse => {
                                    if (mouse.button === Qt.MiddleButton) {
                                        task.modelData.close()
                                    } else if (task.modelData.activated) {
                                        task.modelData.minimized = true
                                    } else {
                                        task.modelData.minimized = false
                                        task.modelData.activate()
                                    }
                                }
                            }
                        }
                    }
                }

                Item { Layout.fillWidth: true }

                // ---- system tray ----
                Bevel {
                    Layout.fillHeight: true
                    visible: SystemTray.items.values.length > 0
                    implicitWidth: Math.ceil(trayRow.implicitWidth) + 10
                    sunken: true

                    Row {
                        id: trayRow
                        anchors.centerIn: parent
                        spacing: 4

                        Repeater {
                            model: SystemTray.items

                            MouseArea {
                                id: slot
                                required property SystemTrayItem modelData
                                width: 16
                                height: 16
                                acceptedButtons: Qt.LeftButton | Qt.MiddleButton | Qt.RightButton

                                function showMenu() {
                                    const p = slot.mapToItem(null, 0, 0)
                                    modelData.display(bar, Math.round(p.x), bar.height)
                                }

                                onClicked: mouse => {
                                    if (mouse.button === Qt.LeftButton) {
                                        // "onlyMenu" items do nothing when activated
                                        if (modelData.onlyMenu && modelData.hasMenu)
                                            showMenu()
                                        else
                                            modelData.activate()
                                    } else if (mouse.button === Qt.MiddleButton) {
                                        modelData.secondaryActivate()
                                    } else if (modelData.hasMenu) {
                                        showMenu()
                                    }
                                }

                                IconImage {
                                    anchors.fill: parent
                                    source: slot.modelData.icon
                                }
                            }
                        }
                    }
                }

                // ---- notify-rs: bell ----
                // Just the icon notify-rs sends, as in the waybar module: 󰂚 empty, 󱅫 something
                // waiting, 󰂛 Do Not Disturb; nothing while the daemon is not running.
                // left = open/close the panel, right = Do Not Disturb, middle = clear all
                Bevel {
                    id: bell
                    Layout.fillHeight: true
                    visible: root.bellText !== ""
                    implicitWidth: Math.ceil(bellGlyph.implicitWidth) + 14
                    sunken: bellMouse.pressed

                    Lbl {
                        id: bellGlyph
                        anchors.centerIn: parent
                        text: root.bellText
                        font.family: "JetBrainsMono Nerd Font Propo"
                        font.pixelSize: 14
                    }

                    MouseArea {
                        id: bellMouse
                        anchors.fill: parent
                        acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton
                        onClicked: mouse => root.nr(
                            mouse.button === Qt.RightButton ? ["dnd", "toggle"]
                            : mouse.button === Qt.MiddleButton ? ["clear"]
                            : ["center", "toggle"])
                    }
                }

                // ---- cpu / ram / battery ----
                Meter {
                    Layout.fillHeight: true
                    label: "CPU"
                    value: root.cpu
                }

                Meter {
                    Layout.fillHeight: true
                    label: "MEM"
                    value: root.mem
                }

                // display only: no mouse handling, clicking does nothing
                Meter {
                    Layout.fillHeight: true
                    visible: root.hasVol
                    label: "VOL"
                    value: root.muted ? 0 : Math.min(1, root.vol)
                    valueText: root.muted ? "MUTE" : Math.round(root.vol * 100) + "%"
                    lit: root.muted ? "#4f8f4f" : "#3cd23c"
                }

                Meter {
                    Layout.fillHeight: true
                    visible: root.hasBat
                    label: "BAT"
                    value: root.batPct
                    valueText: Math.round(root.batPct * 100) + "%" + (root.charging ? "+" : "")
                    lit: root.batLow ? "#ff5040" : "#3cd23c"
                }

                // ---- clock ----
                Bevel {
                    Layout.fillHeight: true
                    implicitWidth: Math.ceil(clockText.implicitWidth) + 16
                    sunken: true
                    face: "#0c140c"

                    Lbl {
                        id: clockText
                        anchors.centerIn: parent
                        color: "#3cd23c"
                        text: Qt.formatDateTime(clock.date, "ddd dd MMM yyyy  HH:mm:ss")
                    }
                }
            }
        }
    }
}
