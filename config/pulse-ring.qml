// ============================================================
// pulse-ring 日常配置 —— Material Design 3 风格
// ============================================================
PulseRing {
    // Lua 脚本（动态控制/自定义算法）
    luaScript: "~/.config/pulse-ring/pulse-ring.lua"

    // ================= 主环（外环）=================
    shape: "ring"
    corners: 5
    spikiness: 0.35
    rotate: 0.0
    autoRotate: 4.0         // 缓慢自转

    colorMode: "gradient"   // MD3 紫罗兰渐变
    colors: ["#6750A4", "#7D5260", "#D0BCFF", "#EADDFF"]
    ringWidth: 7
    baseRadius: 0.13
    growth: 0.20
    haloStrength: 0.18
    haloSize: 0.12
    alpha: 1.0

    // ================= 中环 =================
    midRing: true
    midRadius: 0.78
    midGrowth: 0.08
    midWidth: 3.5
    midColor: "#938F99"

    // ================= 内环（低频呼吸）=================
    innerRing: true
    innerRadius: 0.58
    innerGrowth: 0.07
    innerWidth: 5
    innerColor: "#EADDFF"
    innerAlpha: 0.9

    // ================= 星环 =================
    saturnBand: 0.022
    saturnAlpha: 0.22
    saturnStripes: 0.35

    // ================= 粒子（星环点缀）=================

    // ================= 启动动画（魔法阵）=================
    spawnEffect: "magic"
    spawnDuration: 1800
    spawnEase: "outCubic"
    spawnRotate: 150

    // ================= Widgets =================
    widgets: [
        // 模拟时钟（圆环中心，不超过内环）
        Widget { type: "analog"; x: 0.5; y: 0.5; size: 0.13; color: "#EADDFF"; alpha: 0.9 },
        // 专辑封面（右上角，随乐伸缩+包边，方形裁剪）
        Widget {
            type: "cover"; x: 0.82; y: 0.16; size: 0.14
            color: "#6750A4"; borderWidth: 0.005; coverGrowth: 0.08
            bandMode: "energy"
        },

        // 条形频谱（底部，低频）
        Widget {
            type: "bars"; x: 0.5; y: 0.9
            size: 0.55; bars: 36; barHeight: 0.14; barGap: 0.18
            bandMode: "bass"; colorMode: "gradient"
            colors: ["#4CAF50", "#FFD740", "#FF6E40", "#FF4081"]
        },

        // 镜像条形频谱（紧贴封面下方，宽度对齐封面）
        Widget {
            type: "bars"; x: 0.82; y: 0.27
            size: 0.14; bars: 16; barHeight: 0.10; barGap: 0.25
            mirror: true; bandMode: "full"
            colorMode: "gradient"; colors: ["#00E5FF", "#69F0AE", "#7C4DFF", "#E040FB"]
        },
            // 插件渲染（左下角：渐变圆，由 example-text 插件绘制，随低频膨胀）
        Widget { type: "plugin"; plugin: "example-text"; x: 0.15; y: 0.8; size: 0.12; alpha: 1.0 }
]

    // ================= 全局 =================
    idleBreathe: 0.05
    sensitivity: 1.4
    decay: 0.86
    smoothness: 1.0
    xOffset: 0.0
    yOffset: 0.0
}
