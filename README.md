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
│  ├─ 壁纸：图片/视频/网页场景 + 50 种 GLSL 过渡│
│  ├─ 网页桥：128 频段 → 项目内 Electron      │
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
- **壁纸引擎**：静态图片（`imageWallpaper`，cover/contain/stretch + 完整 mipmap 链）、视频壁纸（GStreamer 解码，音频可选）、网页/场景壁纸、多壁纸轮播，以及 **50+ 种 GLSL 切换动画**（fade/circleopen/crosszoom/glitchmemories…）
- **网页音频 API**：HTML/CSS/Canvas/WebGL 页面可实时接收 128 段频谱、整体能量和低/中/高频，驱动音效动画
- **内置网页预设**：紫色音频粒子、渐变时钟、极光声场；清单见 `assets/wallpapers/presets.json`
- **稳定离屏渲染**：网页壁纸固定使用项目内锁定版本的 Electron，默认以 960×540、约 30fps 软件渲染并丢弃过期帧，避免系统 Electron 版本差异和帧队列堆积
- **歌词**：本地 `~/.config/pulse-ring/lyrics/*.lrc` 优先，在线回退 QQ 音乐 → Lrclib（时长校验防错歌）并缓存；跟随 MPRIS 播放进度，行级点亮高亮 + 上一/下一行预览
- **魔法阵启动动画**：三层环波浪展开 + 旋转 + 前沿光环
- **Lua 脚本**：`onUpdate` / `transformBands` / `pulse.*` API，可逐帧调整配置和频段
- **原生插件**：Rust 动态库可更新状态、变换频段并为 `plugin` widget 渲染 RGBA 纹理
- **多显示器**：每台独立渲染
- **KDE Wayland 录屏兼容**：使用 Bottom layer，保持位于普通窗口下方，同时避免录屏时被 Plasma 桌面背景覆盖
- **音频**：PipeWire monitor 实时 FFT

## 安装（Arch Linux）

```bash
# 手动安装：
git clone https://github.com/MEKCCK/pulse-ring
cd pulse-ring
npm --prefix electron-wallpaper install   # 安装项目锁定的 Electron
cargo build --release
./target/release/pulse-ring
```

基础依赖：Rust/Cargo、PipeWire、Wayland、Vulkan、Fontconfig 和 GStreamer。网页/场景壁纸还需要 Node.js/npm 在项目的 `electron-wallpaper` 目录安装锁定版本 Electron。歌词显示建议安装 JetBrains Maple Mono 或其他 CJK 字体。

pulse-ring 不会调用系统全局 Electron；如果项目内运行时缺失，会明确提示执行上面的 npm 安装命令。

网页壁纸运行时会从编译时的项目目录读取 `electron-wallpaper` 和 `assets`，因此使用网页预设时请保留源码检出目录，或通过包管理器安装完整资源，不要只单独复制可执行文件。

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

**壁纸配置**（壁纸引擎，三种类型任意混排）：
```qml
wallpapers: [                              // 轮播列表：图片 / 视频 / 网页(HTML) 可混排
    "~/Videos/壁纸.mp4",                   // 视频壁纸（GStreamer 解码，可带声音）
    "~/Pictures/壁纸.jpg",                 // 图片壁纸
    "~/wallpapers/我的壁纸/index.html"     // 网页壁纸（Electron 离屏渲染 HTML/CSS/JS）
]
wallpaperInterval: 12                       // 轮换间隔（秒）
wallpaperTransition: 1.8                    // 过渡时长（秒）
wallpaperTransitionEffect: "crosszoom"      // 50+ 种过渡效果之一（fade/circleopen/glitchmemories…）

// 单张模式（不轮播）：
imageWallpaper: "~/Pictures/壁纸.jpg"      // 静态图（留空=透明）
imageWallpaperMode: "cover"                // cover/contain/stretch
videoWallpaper: "~/Videos/壁纸.mp4"        // 视频
videoWallpaperAudio: true                  // 视频是否出声
webWallpaper: "~/wallpapers/index.html"    // 网页壁纸
sceneWallpaper: "~/wallpapers/aurora-scene" // 常驻场景，不参与轮播
```

`sceneWallpaper` 用于持续运行的生活场景；配置后它优先于普通图片/视频轮播。`webWallpaperSize` 默认是 `[960, 540]`，用于控制 Electron 离屏渲染分辨率。

**壁纸打包**（Wallpaper Engine 式：一个文件夹 = 一个壁纸）：
```
my-wallpaper/
├── project.json          # 清单
├── index.html            # 网页/场景资源（或 video.mp4 / image.jpg）
└── assets/...            # 任意引用资源
```
```json
{ "type": "scene", "title": "我的壁纸", "file": "index.html",
  "params": { "particles": 80, "baseHue": 262 } }
```
- `wallpapers: ["~/wallpapers/my-wallpaper"]` 直接传文件夹路径，自动读清单按类型加载
- `type`: `web`/`scene`（HTML）、`video`（视频）、`image`（图片）
- `params` 通过 `window.pulseRing.onConfig()` 传给页面
- 网页/场景壁纸通过 `window.pulseRing.onAudio(callback)` 获取 128 段频谱以及 `energy`/`bass`/`mid`/`treble`；该方法返回取消订阅函数。`onBands` 作为兼容别名保留，`getAudioData()` 可读取最新一帧，`apiVersion` 当前为 `1`。

```js
const unsubscribe = window.pulseRing.onAudio(({ bands, energy, bass, mid, treble }) => {
    // bands 是 Float32Array(128)，数值已归一化
    renderAudioFrame(bands, { energy, bass, mid, treble });
});

window.pulseRing.onConfig(params => applyWallpaperParams(params));
// 页面销毁或不再监听时：unsubscribe();
```

仓库自带三套示例：

| ID | 路径 | 音频响应 | 说明 |
| --- | --- | --- | --- |
| `purple-particles` | `assets/wallpapers/audio-scene` | 是 | 紫色粒子、脉冲环和底部频谱 |
| `demo-clock` | `assets/wallpapers/demo-clock.html` | 否 | 渐变数字时钟和静态星点 |
| `aurora` | `assets/wallpapers/aurora-scene` | 是 | 多层极光、星空、透视网格和能量核心 |

**歌词来源**（按优先级）：
1. 本地文件 `~/.config/pulse-ring/lyrics/<标题>.lrc` 或 `<歌手> - <标题>.lrc`
2. 缓存 `~/.cache/pulse-ring/lyrics/`（在线获取成功后自动保存）
3. 在线获取：QQ 音乐（时长校验防错歌）→ Lrclib，按 MPRIS 的标题/歌手自动匹配

**歌词 widget 参数**：`fontSize`（当前行字号）、`color`（上一/下一行颜色）、`colors[0/1/2]`（当前行渐变：起始/中间/高光色）、`showPrevNext`（是否显示上一/下一行）。

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

AGPL-3.0-only © MEKCCK，详见 [LICENSE](LICENSE)。

## 致谢 / Acknowledgements

本项目（渲染优化与歌词功能）在设计上参考/借鉴了以下开源项目，特此致谢并遵循其许可证：

- **[Folia](https://github.com/chthollyphile/folia-major)**（AGPL-3.0）—— 歌词管线（LRC 解析、逐字着色、行级高亮）、主题与视觉设计参考
- **[SPlayer](https://github.com/SPlayer-Dev/SPlayer)**（AGPL-3.0）—— 网络歌词多源获取、时长匹配校验、歌词元数据/署名行过滤思路
- **[Kaleidux](https://github.com/Mjoyufull/Kaleidux)**（AGPL-3.0）—— 壁纸引擎：视频壁纸（GStreamer playbin/appsink）、50+ GLSL 切换动画库（gl-transitions）、mipmap 生成思路

pulse-ring 采用 **AGPL-3.0-only**（与所参考项目 Folia/SPlayer/Kaleidux 的 AGPL-3.0-only 完全对齐；兼容原 GPL-3.0 条款）；引用来源均以概念/思路形式再实现于 Rust，未直接复制其代码。

## 性能 / 帧率

- **帧率**：始终 30fps（`PULSE_RING_MAX_FPS=60` 可开 60fps），空闲动画（呼吸/自转/粒子/时钟）保持流畅；若想省电，可显式设置 `PULSE_RING_IDLE_FPS=15` 在静音 2 秒后降帧（可选，默认不降）
- **性能剖析**：`PULSE_RING_PROFILE=1` 运行，每 60 帧输出各阶段耗时（pull_audio/lua/plugins/particles/widgets/render）
