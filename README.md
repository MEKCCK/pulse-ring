# pulse-ring

Wayland 壁纸层上的音乐律动圆环（GPU 渲染，wgpu/Vulkan）。

## 特性

- **多重圆环**：外环（按频段律动）、中环（跟随整体能量）、内环（跟随低频 bass）
- **形状系统**：ring / square / diamond / hexagon / triangle / star / flower，支持旋转、虚线
- **星环效果**：连续半透明环带 + 密集粒子环绕
- **粒子系统**：burst / orbit / ring 三种模式，支持重力、阻力、淡入、闪烁、波动、自旋、拖尾
- **启动展开特效**：expand 动画，多种缓动（outCubic / outBack / elastic / bounce）
- **QML 配置**：`~/.config/pulse-ring/pulse-ring.qml` 声明式自定义全部效果
- **多显示器**：每台显示器独立渲染
- **音频**：通过 PipeWire 监听默认输出的 monitor，实时 FFT 分析

## 构建

```bash
cargo build --release
```

需要 Rust 1.86+，运行环境为 Wayland（niri / sway / Hyprland 等支持 wlr-layer-shell 的合成器）。

## 运行

```bash
pulse-ring
```

配置在 `~/.config/pulse-ring/pulse-ring.qml`（首次运行自动读取，缺失时用默认值）。

## 配置示例

```qml
PulseRing {
    shape: "flower"          // ring|square|diamond|hexagon|triangle|star|flower
    autoRotate: 5.0          // 自动旋转（度/秒）
    colorMode: "hue"         // "hue" | "solid" | "gradient"

    midRing: true            // 中环
    midColor: "#ffc95e"
    innerRing: true          // 内环
    innerColor: "#00e6e0"

    saturnBand: 0.035        // 星环带宽度（短边比例，0=关闭）
    particleMode: "ring"     // "burst" | "orbit" | "ring" | "none"
    particles: [
        Particle { x: 0.010; angle: 0; speed: 26; size: 0.008; color: "#4da6ff"; life: 60 }
    ]

    spawnEffect: "expand"    // 启动展开
    spawnEase: "outBack"
}
```

## 退出

`pkill pulse-ring`
