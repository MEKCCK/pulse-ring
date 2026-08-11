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
│  └─ Widgets：时钟/封面/频谱/圆环/粒子        │
└─────────────────────────────────────────────┘
```

- **QML（`pulse-ring.qml`）**：只负责静态样式——形状、颜色、大小、位置、widget 布局
- **Lua（`pulse-ring.lua`）**：负责所有动态行为——粒子（轨道/速度）、音频条幅度、主环运动、衰减/平滑、自转、空闲呼吸、夜间模式、频段变换

## 特性

- **多重圆环**：外环（频段律动）/ 中环（整体能量）/ 内环（低频 bass）
- **形状系统**：ring / square / diamond / hexagon / triangle / star / flower，旋转、虚线
- **星环效果**：连续半透明环带 + 粒子环绕
- **Widgets**：模拟时钟、数字时钟、专辑封面（MPRIS 实时）、条形频谱（含镜像）、独立圆环，自由放置
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
        Widget { type: "bars";   x: 0.5;  y: 0.9;  size: 0.55; bars: 36 }
    ]
}
```

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

GPL-3.0-or-later © MEKCCK，详见 [LICENSE](LICENSE)。
