# pulse-ring

Wayland 壁纸层上的音乐律动可视化（GPU 渲染，wgpu/Vulkan）。

## 架构：QML 样式 + Lua 行为

```
┌─────────────────────────────────────────────┐
│  Lua 脚本层（怎么工作）                       │
│  粒子/音频幅度/运动/条件逻辑/动态调参          │
└──────────────┬──────────────────────────────┘
               │ 每帧调用
┌──────────────▼──────────────────────────────┐
│  Rust 内核                                  │
│  ├─ 音频：PipeWire monitor → FFT → 128 频段  │
│  ├─ 配置：QML 解析 → Config                 │
│  ├─ 渲染：wgpu (Vulkan) → wl-layer-shell    │
│  └─ Widgets：时钟/封面/频谱/圆环/粒子/歌词   │
└─────────────────────────────────────────────┘
```

- **QML（`pulse-ring.qml`）**：只负责静态样式——形状、颜色、大小、位置、widget 布局
- **Lua（`pulse-ring.lua`）**：负责所有动态行为——粒子（轨道/速度）、音频条幅度、主环运动、衰减/平滑、自转、空闲呼吸、夜间模式、频段变换

## 特性

- **多重圆环**：外环（频段律动）/ 中环（整体能量）/ 内环（低频 bass）
- **形状系统**：ring / square / diamond / hexagon / triangle / star / flower，旋转、虚线
- **星环效果**：连续半透明环带 + 粒子环绕
- **Widgets**：模拟时钟、数字时钟、专辑封面（MPRIS 实时）、条形频谱（含镜像）、独立圆环、**歌词（LRC 逐字卡拉OK）**，自由放置
- **歌词**：本地 `~/.config/pulse-ring/lyrics/*.lrc` 优先，自动回退在线获取（Lrclib）并缓存；跟随 MPRIS 播放进度，当前行高亮 + 逐字卡拉OK着色 + 上一/下一行预览
- **魔法阵启动动画**：三层环波浪展开 + 旋转 + 前沿光环
- **Lua 插件**：`onUpdate` / `transformBands` / `pulse.*` API，动态控制一切
- **多显示器**：每台独立渲染
- **音频**：PipeWire monitor 实时 FFT

## 安装（Arch Linux）

```bash
# 从 AUR 或手动：
git clone https://github.com/MEKCCK/pulse-ring
cd pulse-ring
cargo build --release
sudo cp target/release/pulse-ring /usr/local/bin/
```

依赖：rust、pipewire、fontconfig（JetBrains Maple Mono 或任意 CJK 字体）

## 运行

```bash
pulse-ring
```

首次运行自动生成 `~/.config/pulse-ring/pulse-ring.qml` + `pulse-ring.lua`（内置默认配置）。

## 配置

```qml
// ~/.config/pulse-ring/pulse-ring.qml —— 静态样式
PulseRing {
    shape: "ring"
    colorMode: "gradient"
    colors: ["#6750A4", "#7D5260", "#D0BCFF", "#EADDFF"]
    widgets: [
        Widget { type: "analog"; x: 0.5; y: 0.5; size: 0.13 },
        Widget { type: "cover";  x: 0.82; y: 0.16; size: 0.14 },
        Widget { type: "bars";   x: 0.5;  y: 0.9;  size: 0.55; bars: 36 },
        Widget {
            type: "lyric"; x: 0.5; y: 0.82; size: 0.7; fontSize: 42; showPrevNext: true
            color: "#B8B4C8"                     // 上一/下一行（暗色）
            colors: ["#EADDFF", "#FFD740"]      // 当前行 / 卡拉OK进度色
        }
    ]
}
```

**歌词来源**（按优先级）：
1. 本地文件 `~/.config/pulse-ring/lyrics/<标题>.lrc` 或 `<歌手> - <标题>.lrc`
2. 缓存 `~/.cache/pulse-ring/lyrics/`（在线获取成功后自动保存）
3. 在线获取：Lrclib（`https://lrclib.net`），按 MPRIS 的标题/歌手自动匹配

**歌词 widget 参数**：`fontSize`（当前行字号）、`color`（上一/下一行颜色）、`colors[0]`（当前行）、`colors[1]`（卡拉OK进度色）、`showPrevNext`（是否显示上一/下一行）。

```lua
-- ~/.config/pulse-ring/pulse-ring.lua —— 动态行为
function onUpdate(dt)
    config.growth = 0.14 + ring_amp * 0.12
    pulse.setWidget(2, "barHeight", 0.04 + energy * 0.16)
end
function transformBands(bands) ... end
```

## 退出

`pkill pulse-ring`

## 许可证

AGPL-3.0-or-later © MEKCCK，详见 [LICENSE](LICENSE)。

## 致谢 / Acknowledgements

本项目（渲染优化与歌词功能）在设计上参考/借鉴了以下开源项目，特此致谢并遵循其许可证：

- **[Folia](https://github.com/chthollyphile/folia-major)**（AGPL-3.0）—— 歌词管线（LRC 解析、逐字着色、行级高亮）、主题与视觉设计参考
- **[SPlayer](https://github.com/SPlayer-Dev/SPlayer)**（AGPL-3.0）—— 网络歌词多源获取、时长匹配校验、歌词元数据/署名行过滤思路

pulse-ring 采用 **AGPL-3.0-or-later**（兼容原 GPL-3.0 与所参考项目的 AGPL-3.0 条款）；引用来源均以概念/思路形式再实现于 Rust，未直接复制其代码。

## 性能 / 帧率

- **帧率**：始终 30fps（`PULSE_RING_MAX_FPS=60` 可开 60fps），空闲动画（呼吸/自转/粒子/时钟）保持流畅；若想省电，可显式设置 `PULSE_RING_IDLE_FPS=15` 在静音 2 秒后降帧（可选，默认不降）
- **性能剖析**：`PULSE_RING_PROFILE=1` 运行，每 60 帧输出各阶段耗时（pull_audio/lua/plugins/particles/widgets/render）
