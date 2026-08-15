import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ApplicationWindow {
    id: win
    width: 480
    height: 560
    visible: true
    title: "pulse-ring 壁纸包切换"
    color: "#1e1e2e"

    property string cfgPath: FileIO.homeDir() + "/.config/pulse-ring/pulse-ring.qml"
    property string wallpaperDir: FileIO.homeDir() + "/.config/pulse-ring/wallpapers"
    property string cfgText: FileIO.readFile(cfgPath)
    property string currentWallpaper: ""

    // ---- 非注释行的字段读写（保留注释与其余内容）----
    function lineIndex(name) {
        var ls = cfgText.split('\n');
        for (var i = 0; i < ls.length; i++) {
            var t = ls[i].trim();
            if (t.startsWith('//')) continue;
            if (new RegExp("^" + name + "\\s*:").test(t)) return i;
        }
        return -1;
    }
    function setField(name, value) {
        var ls = cfgText.split('\n');
        var i = lineIndex(name);
        if (i >= 0) {
            // 保留原行缩进，整体重写该行（配置 QML 有缩进，^name 锚定会失效）
            var indent = ls[i].match(/^\s*/)[0];
            ls[i] = indent + name + ": " + value;
        } else {
            // 新字段插到 root 块结束 } 之前（文件末尾是块外，解析器会忽略）
            var closeIdx = -1;
            for (var j = 0; j < ls.length; j++) {
                if (ls[j].trim() === '}') closeIdx = j;
            }
            if (closeIdx >= 0) ls.splice(closeIdx, 0, name + ": " + value);
            else ls.push(name + ": " + value);
        }
        cfgText = ls.join('\n');
    }
    function fieldValue(name, def) {
        var ls = cfgText.split('\n');
        var i = lineIndex(name);
        if (i < 0) return def;
        return ls[i].replace(new RegExp("^\\s*" + name + "\\s*:\\s*"), "").trim();
    }
    function save() {
        if (FileIO.writeFile(cfgPath, cfgText)) {
            saveMsg.text = "✓ 已保存 —— 热重载自动生效";
            saveMsg.color = "#a6e3a1";
        } else {
            saveMsg.text = "✗ 写入失败：" + cfgPath;
            saveMsg.color = "#f38ba8";
        }
    }

    // ---- 壁纸包列表（壁纸库 + 内置预设）----
    ListModel { id: packModel }

    function packTitle(dir) {
        var pj = FileIO.readFile(win.wallpaperDir + "/" + dir + "/project.json");
        try {
            return JSON.parse(pj).title || dir;
        } catch (e) { return dir; }
    }
    function packType(dir) {
        var pj = FileIO.readFile(win.wallpaperDir + "/" + dir + "/project.json");
        try {
            return JSON.parse(pj).type || "image";
        } catch (e) { return "image"; }
    }

    function packNameOf(ref) {
        // "Jade-Feet" → "Jade-Feet"；绝对路径 → 取最后一段目录名
        var s = String(ref).replace(/"/g, "").trim();
        if (s === "") return "";
        return s.split("/").pop();
    }
    Component.onCompleted: {
        currentWallpaper = packNameOf(win.fieldValue("imageWallpaper", ""))
            || packNameOf(win.fieldValue("sceneWallpaper", ""))
            || packNameOf(win.fieldValue("webWallpaper", ""));
        // 轮换列表引用包名时也高亮（wallpapers: ["Jade-Feet", ...]）
        if (currentWallpaper === "") {
            try {
                var arr = JSON.parse(win.fieldValue("wallpapers", "[]"));
                if (arr && arr.length > 0) currentWallpaper = packNameOf(arr[0]);
            } catch (e) {}
        }
        var dirs = FileIO.listDir(win.wallpaperDir);
        for (var i = 0; i < dirs.length; i++) {
            var d = dirs[i];
            packModel.append({
                name: d,
                title: packTitle(d),
                type: packType(d),
                active: d === currentWallpaper
            });
        }
    }

    function applyPack(name, type) {
        // 只保留当前类型的字段，清掉其余（sceneWallpaper 优先于 imageWallpaper，
        // 不清会导致切换看似无效）。
        win.setField("sceneWallpaper", '""');
        win.setField("imageWallpaper", '""');
        win.setField("webWallpaper", '""');
        if (type === "scene")
            win.setField("sceneWallpaper", '"' + name + '"');
        else if (type === "web")
            win.setField("webWallpaper", '"' + name + '"');
        else
            win.setField("imageWallpaper", '"' + name + '"');
        for (var i = 0; i < packModel.count; i++)
            packModel.setProperty(i, "active", packModel.get(i).name === name);
        currentWallpaper = name;
        win.save();
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 16
        spacing: 12

        RowLayout {
            Layout.fillWidth: true
            Text {
                text: "壁纸包"
                font.pixelSize: 20
                font.bold: true
                color: "#cdd6f4"
            }
            Item { Layout.fillWidth: true }
            Text {
                text: FileIO.serviceRunning() ? "● 运行中" : "○ 未运行"
                color: FileIO.serviceRunning() ? "#a6e3a1" : "#f38ba8"
            }
        }
        Text {
            text: "点击切换壁纸包（来自 ~/.config/pulse-ring/wallpapers/）"
            color: "#6c7086"
            font.pixelSize: 11
        }

        // 壁纸包网格
        ListView {
            id: packList
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            model: packModel
            delegate: Rectangle {
                width: packList.width
                height: 52
                radius: 8
                color: model.active ? "#31324a" : (index % 2 ? "#24243a" : "#28283e")
                border.color: model.active ? "#cba6f7" : "#3a3a52"
                border.width: model.active ? 2 : 1
                ColumnLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 14
                    anchors.rightMargin: 14
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 2
                    RowLayout {
                        Layout.fillWidth: true
                        Text {
                            text: model.title
                            font.pixelSize: 15
                            font.bold: model.active
                            color: "#cdd6f4"
                            elide: Text.ElideRight
                            Layout.fillWidth: true
                        }
                        Text {
                            text: model.type === "scene" ? "场景" : (model.type === "web" ? "网页" : "图片")
                            font.pixelSize: 10
                            color: model.type === "scene" ? "#89b4fa" : "#a6e3a1"
                        }
                        Text {
                            text: model.active ? "✓ 使用中" : ""
                            font.pixelSize: 11
                            color: "#f9e2af"
                        }
                    }
                    Text {
                        text: model.name === "" ? "" : model.name
                        font.pixelSize: 10
                        color: "#6c7086"
                    }
                }
                MouseArea {
                    anchors.fill: parent
                    onClicked: win.applyPack(model.name, model.type)
                }
            }
        }

        // ---- 全局设置 ----
        GroupBox {
            title: "全局设置"
            Layout.fillWidth: true
            ColumnLayout {
                anchors.fill: parent
                spacing: 6
                GridLayout {
                    columns: 3
                    Layout.fillWidth: true
                    Text { text: "帧率"; color: "#cdd6f4" }
                    SpinBox {
                        id: fpsSpin
                        from: 5; to: 120; stepSize: 5
                        value: parseInt(win.fieldValue("fps", "30"))
                        onValueModified: win.setField("fps", value)
                    }
                    Text { text: "fps"; color: "#6c7086" }
                    Text { text: "视频声音"; color: "#cdd6f4" }
                    Switch {
                        id: audioSwitch
                        checked: win.fieldValue("videoWallpaperAudio", "true") !== "false"
                        onToggled: win.setField("videoWallpaperAudio", checked ? "true" : "false")
                    }
                    Item { }
                    Text { text: "切换动画"; color: "#cdd6f4" }
                    ComboBox {
                        id: effectCombo
                        Layout.columnSpan: 2
                        Layout.fillWidth: true
                        model: ["BookFlip", "Bounce", "BowTieHorizonta", "BowTieVertica", "ButterflyWaveScrawler", "Circle", "CircleCrop", "CircleOpen", "ColourDistance", "CrossWarp", "CrossZoom", "Directiona", "DirectionalScaled", "DirectionalWipe", "Dissolve", "Doom", "Doorway", "Dreamy", "DreamyZoom", "Edge", "Fade", "FilmBurn", "GlitchDisplace", "GlitchMemorie", "GridFlip", "Hexagonalize", "HorizontalClose", "HorizontalOpen", "InvertedPageCur", "LeftRight", "LinearBlur", "Mosaic", "Overexposure", "Pixelize", "PolkaDotsCurtain", "Radia", "Rectangle", "Ripple", "Ro", "RotateScaleFade", "RotateScaleVanish", "SimpleZoom", "Slide", "StaticFade", "StereoViewer", "Swir", "TvStatic", "WaterDrop", "WindowBlind", "ZoomInCircle"]
                        currentIndex: Math.max(0, model.indexOf(win.fieldValue("wallpaperTransitionEffect", "fade").replace(/"/g, "")))
                        onActivated: win.setField("wallpaperTransitionEffect", '"' + currentText + '"')
                    }
                    Text { text: "过渡时长"; color: "#cdd6f4" }
                    SpinBox {
                        id: transSpin
                        from: 1; to: 30; stepSize: 1
                        value: Math.round(parseFloat(win.fieldValue("wallpaperTransition", "1.5")) * 10)
                        onValueModified: win.setField("wallpaperTransition", (value / 10).toFixed(1))
                    }
                    Text { text: "秒"; color: "#6c7086" }
                }
                Button {
                    text: "💾 保存并应用（热重载）"
                    Layout.fillWidth: true
                    font.pixelSize: 14
                    onClicked: win.save()
                }
                Text {
                    id: saveMsg
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    color: "#a6e3a1"
                }
            }
        }
    }
}
