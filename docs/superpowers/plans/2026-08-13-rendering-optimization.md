# 渲染优化实现计划（Phase 0 + Phase 1）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 优化 pulse-ring 渲染性能：多屏 CPU 计算去重、shader widget 包围盒早退、自适应帧率、消除每帧大分配，并为粒子计算 GPU 化打基础。

**Architecture:** 保持「单全屏三角形 + 巨型 fragment shader + 单 storage buffer」架构不变。核心改造：(1) 把 `draw_one` 拆成「每帧一次」的 `compute_scene` + 「每屏一次」的 `render_output`，消灭多屏下 Lua/插件/粒子/widgets 的重复计算；(2) shader 内为每个 widget 加像素包围盒早退，跳过 widget 外的 SDF 计算；(3) 主循环按音频能量自适应帧率；(4) 复用插件纹理缓冲；(5) 粒子位置计算从 CPU 移到 WGSL。

**Tech Stack:** Rust 2024、wgpu 30、WGSL、libc。**本计划不新增任何依赖。**

## Global Constraints

- Rust edition 2024；wgpu 30；`cargo build` 与 `cargo test` 必须通过
- 渲染架构约束：单全屏三角形（`pass.draw(0..3, 0..1)`）、storage buffer 传 uniforms、透明清屏（`LoadOp::Clear(TRANSPARENT)`）必须保留
- 行为不变：多显示器、`renderScreen`、damage region 优化、透明壁纸语义不得破坏
- **uniform 结构体（Rust `Uniforms` 与 WGSL `struct Uniforms`）必须同步修改**；buffer 大小改为 `std::mem::size_of::<Uniforms>()` 推导并加 debug 断言（见 Task 3/6），禁止手算偏移
- 渲染类改动需要手动验证：本机为 Wayland（niri）会话，`cargo run` 后观察壁纸可视化无异常
- 现有测试 `tests::parse_widgets_works` 必须继续通过

---

### Task 1: 帧耗时剖析（Phase 0 基线）

**Files:**
- Modify: `src/main.rs`（`App` 结构体、`main()` 循环、`draw_one`）
- Test: `src/main.rs` 底部 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 无（独立任务）
- Produces: `App.profile: ProfileStats`、`App.profile_enabled: bool`、`fn profile_tick(&mut self, name: &str)`、`fn profile_dump(&self) -> String`（Task 2-6 复用）

- [ ] **Step 1: 写失败测试**

在 `src/main.rs` 的 `mod tests` 中追加：

```rust
#[test]
fn profile_stats_accumulates_and_formats() {
    let mut p = super::ProfileStats::default();
    p.pull_audio = 0.001;
    p.lua = 0.002;
    p.plugins = 0.003;
    p.plugin_tex = 0.004;
    p.particles = 0.005;
    p.widgets = 0.006;
    p.render = 0.007;
    let s = super::ProfileStats::format_line(&p);
    assert!(s.contains("pull_audio=1.0ms"), "got: {s}");
    assert!(s.contains("render=7.0ms"), "got: {s}");
    assert!(s.contains("total=28.0ms"), "got: {s}");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test profile_stats_accumulates_and_formats`
Expected: FAIL，编译错误 `cannot find type ProfileStats`

- [ ] **Step 3: 实现剖析器**

在 `src/main.rs` 顶部（`const MAX_PARTICLES` 附近）加入：

```rust
/// Per-frame timing breakdown (seconds). Filled when PULSE_RING_PROFILE=1.
#[derive(Default, Clone, Copy)]
pub struct ProfileStats {
    pub pull_audio: f32,
    pub lua: f32,
    pub plugins: f32,
    pub plugin_tex: f32,
    pub particles: f32,
    pub widgets: f32,
    pub render: f32,
}

impl ProfileStats {
    pub fn format_line(s: &Self) -> String {
        let total = s.pull_audio + s.lua + s.plugins + s.plugin_tex + s.particles + s.widgets + s.render;
        format!(
            "[profile] pull_audio={:.1}ms lua={:.1}ms plugins={:.1}ms plugin_tex={:.1}ms particles={:.1}ms widgets={:.1}ms render={:.1}ms total={:.1}ms",
            s.pull_audio * 1000.0, s.lua * 1000.0, s.plugins * 1000.0, s.plugin_tex * 1000.0,
            s.particles * 1000.0, s.widgets * 1000.0, s.render * 1000.0, total * 1000.0,
        )
    }
}
```

在 `App` 结构体追加字段：`profile: ProfileStats, profile_enabled: bool, profile_frames: u32`。
在 `main()` 的 `let mut app = App { ... }` 初始化处追加：

```rust
profile: ProfileStats::default(),
profile_enabled: std::env::var("PULSE_RING_PROFILE").is_ok(),
profile_frames: 0,
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test profile_stats_accumulates_and_formats`
Expected: PASS

- [ ] **Step 5: 埋点**

在 `App` 上增加两个方法（放在 `tick` 附近）：

```rust
impl App {
    /// Record a timing checkpoint; call twice with the same name to accumulate.
    fn profile_mark(&mut self, name: &str, start: std::time::Instant) {
        if !self.profile_enabled {
            return;
        }
        let d = start.elapsed().as_secs_f32();
        match name {
            "pull_audio" => self.profile.pull_audio += d,
            "lua" => self.profile.lua += d,
            "plugins" => self.profile.plugins += d,
            "plugin_tex" => self.profile.plugin_tex += d,
            "particles" => self.profile.particles += d,
            "widgets" => self.profile.widgets += d,
            "render" => self.profile.render += d,
            _ => {}
        }
    }

    fn profile_maybe_log(&mut self) {
        if !self.profile_enabled {
            return;
        }
        self.profile_frames += 1;
        if self.profile_frames % 60 == 0 {
            log::info!("{}", ProfileStats::format_line(&self.profile));
            self.profile = ProfileStats::default();
        }
    }
}
```

在 `tick()` 中按现有代码段依次埋点：`let t0 = std::time::Instant::now();` 放在每段之前，`self.profile_mark("pull_audio", t0);` 等放在段后（段划分：pull_audio / lua+plugins / plugin_tex / particles / widgets / render），并在 `tick()` 末尾调用 `self.profile_maybe_log();`。

- [ ] **Step 6: 构建 + 测试**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -3`
Expected: 构建成功，全部测试通过

- [ ] **Step 7: 手动验证**

Run: `PULSE_RING_PROFILE=1 cargo run`（Wayland 会话，观察日志）
Expected: 每 60 帧输出一行 `[profile] ... total=..ms`；记录各阶段占比作为基线，写进本任务 commit message

- [ ] **Step 8: 提交**

```bash
git add src/main.rs
git commit -m "perf: 帧耗时剖析 (PULSE_RING_PROFILE=1)"
```

---

### Task 2: compute-once / render-many —— 消灭多屏重复 CPU 计算

**Files:**
- Modify: `src/main.rs`（`draw_one` 拆分为 `compute_scene` + `render_output`；`tick` 改调用；`prepare_widgets` 改签名）

**Interfaces:**
- Consumes: Task 1 的 `profile_mark`
- Produces:
  - `struct SceneFrame { render_bands: [f32; NBANDS], spawn_scale: f32, spawn_t: f32, spawn_effect: u32, spawn_rot: f32, rotate_rad: f32, amp_avg: f32, particles: [f32; MAX_PARTICLES * PARTICLE_STRIDE], widgets: [f32; 1280], widgets_cfg: Vec<crate::config::WidgetConfig> }`
  - `fn compute_scene(&mut self) -> SceneFrame`（每帧一次）
  - `fn render_output(&mut self, idx: usize, scene: &SceneFrame)`（每屏一次）
  - 自由函数 `fn compute_bar_energy(bands: &[f32; NBANDS]) -> [f32; 64]`、`fn compute_overall_energy(bands: &[f32; NBANDS]) -> f32`

- [ ] **Step 1: 写失败测试（纯函数提取）**

在 `mod tests` 追加：

```rust
#[test]
fn bar_energy_and_overall_are_correct() {
    let mut bands = [0.0f32; super::NBANDS];
    bands[0] = 1.0; // low band only
    let be = super::compute_bar_energy(&bands);
    // bin 0 covers bands 0..2 -> mean 0.5
    assert!((be[0] - 0.5).abs() < 1e-6, "be[0]={}", be[0]);
    assert_eq!(be[1], 0.0);
    let ov = super::compute_overall_energy(&bands);
    // 1.0 / 80 over mid bands 16..96
    assert!((ov - 1.0 / 80.0).abs() < 1e-6, "ov={ov}");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test bar_energy_and_overall_are_correct`
Expected: FAIL，`cannot find function compute_bar_energy`

- [ ] **Step 3: 提取纯函数**

把 `draw_one` 中内联的 bar_energy 计算与 overall 计算提取为模块级自由函数（放在 `compute_particles` 之后）：

```rust
/// Precompute 64 bar energies from the render bands (bars widgets look these up).
fn compute_bar_energy(bands: &[f32; NBANDS]) -> [f32; 64] {
    let mut out = [0.0f32; 64];
    let n = bands.len();
    for bi in 0..64 {
        let lo = bi * n / 64;
        let hi = ((bi + 1) * n / 64).max(lo + 1);
        let mut acc = 0.0f32;
        for i in lo..hi {
            acc += bands[i];
        }
        out[bi] = acc / (hi - lo) as f32;
    }
    out
}

/// Overall energy: mean of the mid-frequency bands (16..96).
fn compute_overall_energy(bands: &[f32; NBANDS]) -> f32 {
    let mut acc = 0.0f32;
    for i in 16..96 {
        acc += bands[i];
    }
    acc / 80.0
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test bar_energy_and_overall_are_correct`
Expected: PASS

- [ ] **Step 5: 定义 SceneFrame 并重构**

在 `struct App` 定义之后加入：

```rust
/// Per-frame scene state computed ONCE per tick and consumed by every output.
struct SceneFrame {
    render_bands: [f32; NBANDS],
    spawn_scale: f32,
    spawn_t: f32,
    spawn_effect: u32,
    spawn_rot: f32,
    rotate_rad: f32,
    amp_avg: f32,
    particles: [f32; MAX_PARTICLES * PARTICLE_STRIDE],
    widgets: [f32; 1280],
    widgets_cfg: Vec<crate::config::WidgetConfig>,
}
```

重构步骤（保持逻辑逐行不变，只改归属）：
1. `prepare_widgets` 签名改为 `fn prepare_widgets(&mut self, widgets: &[crate::config::WidgetConfig]) -> [f32; 1280]`，删除内部的 `self.cfg.widgets.iter().take(32).cloned().collect()`（快照由调用方传入）；`width`/`height` 参数本就未使用，一并删除；删除末尾空循环 `for (si, w) in widgets.iter().enumerate() {}`。
2. 新建 `compute_scene`：把原 `draw_one` 中「`poll_music` → Lua → 插件 update/transform → `render_plugin_textures` → spawn/rotate/amp → `compute_particles` → `prepare_widgets`」整段搬入（每段保持 `profile_mark` 埋点），返回 `SceneFrame { render_bands, spawn_scale, spawn_t, spawn_effect, spawn_rot, rotate_rad, amp_avg, particles, widgets, widgets_cfg }`。其中 `widgets_cfg` 是 Step 5.1 的快照 Vec（`compute_scene` 内 `let widgets_cfg: Vec<_> = self.cfg.widgets.iter().take(32).cloned().collect();`），`widgets` 是 `self.prepare_widgets(&widgets_cfg)`。
3. 新建 `render_output(&mut self, idx: usize, scene: &SceneFrame)`：把原 `draw_one` 中剩余的「封面上传 → texture_slots 上传 → `set_widgets`/`resize`/`set_auto_rotate`/`set_bar_energy`（改用 `compute_bar_energy(&scene.render_bands)`）/`set_overall_energy`（改用 `compute_overall_energy`）/particle count/band/render_scale → `renderer.render(...)` → damage + commit」整段搬入。局部 `let mut widgets = scene.widgets;` 以便 uv 就地更新后用于 damage 计算。
4. 改写 `tick`：

```rust
fn tick(&mut self) {
    let t0 = std::time::Instant::now();
    self.pull_audio();
    self.profile_mark("pull_audio", t0);
    let scene = self.compute_scene();
    let target = self.cfg.render_screen;
    if target >= 0 {
        let idx = target as usize;
        if idx < self.outputs.len() && !self.outputs[idx].closed && self.outputs[idx].width > 0 {
            self.render_output(idx, &scene);
        }
    } else {
        for idx in 0..self.outputs.len() {
            if !self.outputs[idx].closed && self.outputs[idx].width > 0 {
                self.render_output(idx, &scene);
            }
        }
    }
    self.profile_maybe_log();
}
```

5. `LayerShellHandler::configure` 中的首次绘制调用 `self.draw_one(idx)` 改为 `let scene = self.compute_scene(); self.render_output(idx, &scene);`（保留 `first && is_target` 语义）。

- [ ] **Step 6: 构建 + 测试**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -3`
Expected: 构建成功（可清理 2-3 个因删除旧路径产生的 dead_code warning），全部测试通过

- [ ] **Step 7: 手动验证**

Run: `PULSE_RING_PROFILE=1 cargo run`
Expected: 视觉输出与重构前一致；**多屏场景**下 `[profile] lua=` 与 `widgets=` 不再随屏数翻倍（对比 Task 1 基线）

- [ ] **Step 8: 提交**

```bash
git add src/main.rs
git commit -m "perf: compute_scene 每帧一次 / render_output 每屏一次，多屏 CPU 去重"
```

---

### Task 3: shader widget 包围盒早退

**Files:**
- Modify: `src/draw.rs`（`Uniforms` 结构体、`RingRenderer` 成员、`new()` 的 buffer size 与 bind group、`set_widget_bounds`、render() 填充 uniforms）
- Modify: `src/draw.rs` 内嵌 WGSL（`struct Uniforms`、`fs_main` widget 循环）
- Modify: `src/main.rs`（`SceneFrame` 加 `widget_bounds`、`compute_scene` 填充、`render_output` 传参、`prepare_widgets` 不涉及）

**Interfaces:**
- Consumes: Task 2 的 `SceneFrame.widgets_cfg`
- Produces:
  - `fn compute_widget_bounds(widgets: &[crate::config::WidgetConfig], width: u32, height: u32) -> [f32; 32]`（自由函数，可测试）
  - `RingRenderer::set_widget_bounds(&mut self, data: &[f32; 32])`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 追加：

```rust
#[test]
fn widget_bounds_are_finite_and_cover_widgets() {
    use crate::config::{WidgetConfig, WidgetType};
    let mut w = WidgetConfig::default();
    w.widget_type = WidgetType::Ring;
    w.size = 0.2;
    w.base_radius = 0.13;
    w.growth = 0.2;
    w.halo_size = 0.12;
    let b = super::compute_widget_bounds(&[w], 1920, 1080);
    // min_d = 1080; bound = (0.13+0.2+0.12+0.05)*0.2*1080 = 108
    assert!((b[0] - 108.0).abs() < 1.0, "b[0]={}", b[0]);
    assert!(b[1..].iter().all(|&v| v == 0.0));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test widget_bounds_are_finite_and_cover_widgets`
Expected: FAIL，`cannot find function compute_widget_bounds`

- [ ] **Step 3: 实现 CPU 侧 bounds 计算**

在 `src/main.rs`（`compute_overall_energy` 之后）加入：

```rust
/// Per-widget conservative bounding radius in pixels, used by the shader to skip
/// pixels outside each widget's region before running SDF math.
fn compute_widget_bounds(widgets: &[crate::config::WidgetConfig], width: u32, height: u32) -> [f32; 32] {
    use crate::config::WidgetType;
    let mut out = [0.0f32; 32];
    let min_d = width.min(height) as f32;
    for (i, w) in widgets.iter().take(32).enumerate() {
        let b = match w.widget_type {
            WidgetType::Ring => (w.base_radius + w.growth + w.halo_size + 0.05) * w.size * min_d,
            WidgetType::Bars => w.size.max(w.bar_height) * min_d * 1.05,
            WidgetType::Clock | WidgetType::Analog => (w.size * 0.5 + w.dial_border) * min_d + min_d * 0.01,
            WidgetType::Image | WidgetType::Cover => w.size * min_d * 0.75 + (w.border_width + w.cover_growth) * min_d,
            WidgetType::Plugin => w.size * min_d * 0.75,
        };
        out[i] = b.max(1.0);
    }
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test widget_bounds_are_finite_and_cover_widgets`
Expected: PASS

- [ ] **Step 5: Rust uniform 结构体 + 渲染器**

在 `src/draw.rs`：

1. `struct Uniforms` 末尾（`particle_band_r` 之后）追加字段：

```rust
    widget_bounds: [f32; 32],
```

2. `RingRenderer` 追加成员 `widget_bounds_data: [f32; 32]`，`new()` 初始化 `widget_bounds_data: [0.0; 32]`。
3. `new()` 中 buffer 大小改为推导 + 断言（替换 `size: 10832` 与 `min_binding_size` 的手写值）：

```rust
        const UNIFORM_SIZE: u64 = std::mem::size_of::<Uniforms>() as u64;
        assert!(
            UNIFORM_SIZE <= 10832 + 128,
            "uniform struct grew beyond reserved buffer: {UNIFORM_SIZE}"
        );
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ring uniforms"),
            size: UNIFORM_SIZE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
```

   bind group layout 的 `min_binding_size` 同样改为 `NonZeroU64::new(UNIFORM_SIZE)`（删除 `NonZeroU32::new(10832)...` 那行手写值）。
4. 新增 setter（放在 `set_widgets` 之后）：

```rust
    /// Per-widget bounding radii (px), for shader early-out.
    pub fn set_widget_bounds(&mut self, data: &[f32; 32]) {
        self.widget_bounds_data = *data;
    }
```

5. `render()` 的 `Uniforms { ... }` 初始化追加：

```rust
            widget_bounds: self.widget_bounds_data,
```

- [ ] **Step 6: WGSL 同步**

1. WGSL `struct Uniforms` 末尾追加：`widget_bounds: array<f32, 32>,`
2. `fs_main` 的 widget 循环，在 `let wdist = length(wd);` 之后、`if (wtype == 0.0)` 之前插入：

```wgsl
        // Early-out: pixels outside this widget's conservative bounding circle skip
        // all widget SDF math (widgets are small; most pixels are far away).
        if (wdist > u.widget_bounds[wi]) {
            continue;
        }
```

- [ ] **Step 7: main.rs 接线**

1. `SceneFrame` 追加字段 `widget_bounds: [f32; 32]`。
2. `compute_scene` 中在 `prepare_widgets` 之后计算（需要某输出尺寸；取第一个可用输出，无输出时回退 1920x1080）：

```rust
        let (sw, sh) = self
            .outputs
            .iter()
            .find(|o| o.width > 0)
            .map(|o| (o.width, o.height))
            .unwrap_or((1920, 1080));
        let widget_bounds = compute_widget_bounds(&widgets_cfg, sw, sh);
```

   `SceneFrame { ..., widgets_cfg, widget_bounds }`。
3. `render_output` 在 `renderer.set_widgets(&widgets)` 附近追加：`renderer.set_widget_bounds(&scene.widget_bounds);`（注意：bounds 按第一个输出的尺寸算，多屏尺寸不同时用各自输出的尺寸更准，但保守值覆盖所有情况——`render_output` 内可改为按 `width`/`height` 重新计算：`let widget_bounds = compute_widget_bounds(&scene.widgets_cfg, width, height);` 然后 set；采用后者）。

- [ ] **Step 8: 构建 + 测试**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -3`
Expected: 构建成功（debug 断言不触发），全部测试通过

- [ ] **Step 9: 手动验证**

Run: `cargo run`（无 profile 时视觉必须与之前一致）
Expected: 环/widgets 显示完整无缺角；`PULSE_RING_PROFILE=1` 下 `render=` 耗时下降（widget 多时明显）

- [ ] **Step 10: 提交**

```bash
git add src/main.rs src/draw.rs
git commit -m "perf: shader widget 包围盒早退，跳过 widget 外 SDF 计算"
```

---

### Task 4: 自适应帧率（活动 30/60fps，空闲 5fps）

**Files:**
- Modify: `src/main.rs`（`App` 字段、`main()` 主循环、`tick`）

**Interfaces:**
- Consumes: 无（独立于 Task 2/3，但若在前序之后执行则复用 `SceneFrame` 无关——本任务只改主循环）
- Produces: `fn frame_interval_ms(energy_max: f32, max_fps: u32) -> u64`（自由函数，可测试）、`App.interval: std::time::Duration`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 追加：

```rust
#[test]
fn frame_interval_adapts_to_energy() {
    use super::frame_interval_ms;
    assert_eq!(frame_interval_ms(0.0, 30), 200);      // idle -> 5fps
    assert_eq!(frame_interval_ms(0.001, 30), 200);    // below threshold -> idle
    assert_eq!(frame_interval_ms(0.01, 30), 33);      // active 30fps
    assert_eq!(frame_interval_ms(0.01, 60), 16);      // active 60fps
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test frame_interval_adapts_to_energy`
Expected: FAIL，`cannot find function frame_interval_ms`

- [ ] **Step 3: 实现**

在 `src/main.rs`（`compute_widget_bounds` 之后）加入：

```rust
/// Frame interval in ms: idle (no audio) -> 5fps; active -> 30fps, or 60fps when opted in.
fn frame_interval_ms(energy_max: f32, max_fps: u32) -> u64 {
    if energy_max < 0.002 {
        200
    } else if max_fps >= 60 {
        16
    } else {
        33
    }
}
```

`App` 追加字段 `interval: std::time::Duration`（初始化 `Duration::from_millis(33)`）与 `idle_since: Option<f32>`（初始化 `None`）。`main()` 主循环改为：

```rust
    let mut max_fps: u32 = std::env::var("PULSE_RING_MAX_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    max_fps = max_fps.clamp(15, 60);
    while !app.outputs.iter().any(|o| o.width > 0) {
        event_queue.blocking_dispatch(&mut app).unwrap();
        if !app.outputs.is_empty() && app.outputs.iter().all(|o| o.closed) {
            return;
        }
    }
    loop {
        let before = std::time::Instant::now();
        event_queue.dispatch_pending(&mut app).unwrap();
        app.tick();
        if !app.outputs.is_empty() && app.outputs.iter().all(|o| o.closed) {
            break;
        }
        let elapsed = before.elapsed();
        let interval = app.interval;
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
```

`tick()` 开头（`pull_audio` 之后）更新 interval：

```rust
        // Adaptive frame rate: idle (quiet) drops to 5fps; audio resumes instantly.
        let energy_max = self.bands.iter().copied().fold(0.0f32, f32::max);
        let idle = energy_max < 0.002;
        let now = self.start.elapsed().as_secs_f32();
        self.idle_since = if idle {
            Some(self.idle_since.unwrap_or(now))
        } else {
            None
        };
        let is_idle = self.idle_since.map(|t| now - t > 2.0).unwrap_or(false);
        self.interval = std::time::Duration::from_millis(frame_interval_ms(
            if is_idle { 0.0 } else { energy_max },
            max_fps,
        ));
```

（`max_fps` 需传入 `tick`——改为 `App` 字段 `max_fps: u32`，初始化处从 env 解析，删除 main 里的局部 `max_fps`。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test frame_interval_adapts_to_energy`
Expected: PASS

- [ ] **Step 5: 手动验证**

Run: `cargo run`，暂停音乐
Expected: 停止播放 ~2s 后日志可见帧间隔变稀疏（5fps）；恢复播放立即回 30fps；`PULSE_RING_MAX_FPS=60 cargo run` 播放时 60fps

- [ ] **Step 6: 提交**

```bash
git add src/main.rs
git commit -m "perf: 自适应帧率 —— 空闲 5fps / 活动 30fps（PULSE_RING_MAX_FPS=60 可开 60fps）"
```

---

### Task 5: 插件纹理缓冲复用 + 免拷贝

**Files:**
- Modify: `src/main.rs`（`render_plugin_textures`、`App` 字段）

**Interfaces:**
- Consumes: 无
- Produces: `App.plugin_buf: Vec<u8>`（复用缓冲，1MB 只在首次分配）

- [ ] **Step 1: 实现**

1. `App` 追加字段 `plugin_buf: Vec<u8>`，初始化 `Vec::new()`。
2. 重写 `render_plugin_textures` 的缓冲分配段：

```rust
        // Reuse one persistent buffer across frames (plugins are called every frame;
        // reallocating 1MB per frame per plugin is pure waste).
        if self.plugin_buf.len() < 512 * 512 * 4 {
            self.plugin_buf.resize(512 * 512 * 4, 0);
        }
        for (i, p) in self.plugins.iter().enumerate() {
            let slot = (8 + i) as u32;
            let mut req = plugin::RenderRequest {
                slot,
                buf_len: self.plugin_buf.len(),
                buf: self.plugin_buf.as_mut_ptr(),
                update: false,
                width: 0,
                height: 0,
                screen_w,
                screen_h,
            };
            p.bind_state(&self.bands, &self.cfg as *const crate::config::Config);
            p.call_render(&mut req);
            if !req.update || req.width == 0 || req.height == 0 {
                self.plugin_tex[i] = None;
                continue;
            }
            // ...(原拷贝逻辑不变，从 self.plugin_buf 读取)...
        }
```

   注意：原逻辑 `self.plugin_tex[i]` 未更新时保留旧值；改为 `None` 会清掉旧纹理。为保持行为（旧纹理继续显示），仅当 `req.update` 为真时更新 `plugin_tex[i]`，否则**跳过写入**（保留旧值），即把原来的 `if req.update && ...` 提前为 continue 判断：`if !req.update || req.width == 0 || req.height == 0 { continue; }`。后续 `texture_slots` 写入循环保持原样（旧值自然保留）。

- [ ] **Step 2: 构建 + 测试**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -3`
Expected: 构建成功，全部测试通过

- [ ] **Step 3: 手动验证**

Run: `PULSE_RING_PROFILE=1 cargo run`（带示例插件 `plugins/example-text` 编译出的 `.so` 时）
Expected: 插件纹理正常显示；`plugin_tex=` 耗时下降；内存不再每帧 +1MB

- [ ] **Step 4: 提交**

```bash
git add src/main.rs
git commit -m "perf: 插件渲染缓冲复用，消除每帧 1MB 分配"
```

---

### Task 6: 粒子计算 GPU 化（WGSL 移植）

**Files:**
- Modify: `src/main.rs`（删除 `compute_particles`；`SceneFrame` 换字段；`compute_scene` 构建粒子参数；`render_output` 传参）
- Modify: `src/draw.rs`（`Uniforms`、WGSL struct、`fs_main` 粒子循环、`render()` 签名）

**Interfaces:**
- Consumes: Task 1-3 的结构
- Produces:
  - `fn build_particle_params(cfg: &crate::config::Config) -> [f32; MAX_PARTICLES * 18]`（每粒子 18 f32：x, y, angle(deg), speed(deg/s), size, size_end, r, g, b, a, life, delay, drag, gravity, wave, fade_in, twinkle, spin_speed）
  - `SceneFrame.particle_params: [f32; MAX_PARTICLES * 18]`、`SceneFrame.particle_amp: f32`
  - `RingRenderer::render` 去掉 `particles` 参数，增加 `particle_params: &[f32; MAX_PARTICLES*18]` 与 `particle_amp: f32` 入参
  - WGSL `fn particle_state(i: u32, t: f32, min_d: f32, cx: f32, cy: f32, amp: f32) -> Particle`

- [ ] **Step 1: 写失败测试（纯函数）**

在 `mod tests` 追加（`ParticleState` 由 Step 3 定义，先按接口写）：

```rust
#[test]
fn particle_state_ring_matches_previous_math() {
    use crate::config::ParticleMode;
    let mut cfg = crate::config::Config::default_for_test();
    cfg.particle_mode = ParticleMode::Ring;
    cfg.base_radius = 0.13;
    cfg.growth = 0.2;
    cfg.halo_size = 0.12;
    cfg.particles = vec![crate::config::ParticleConfig {
        x: 0.012,
        y: 0.0,
        angle: 0.0,
        speed: 26.0,
        size: 0.006,
        size_end: 0.006,
        color: [0.8, 0.7, 1.0, 1.0],
        life: 60.0,
        delay: 0.0,
        drag: 0.0,
        gravity: 0.0,
        wave: 0.0,
        fade_in: 0.0,
        twinkle: 0.0,
        spin_speed: 0.0,
        ..Default::default()
    }];
    let params = super::build_particle_params(&cfg);
    let p = super::particle_state_for_test(&params, 0, 1.0, 1080.0, 960.0, 540.0, 0.5);
    // ring radius = (0.13 + 0.2*0.5 + 0.12*0.5 + 0.012) * 1080 = 259.2
    let r = ((p.px - 960.0).powi(2) + (p.py - 540.0).powi(2)).sqrt();
    assert!((r - 259.2).abs() < 1.0, "r={r}");
}
```

（注：`Config::default_for_test` 与 `ParticleConfig` 字段需与 `config.rs` 现有定义核对；若 `ParticleConfig` 已实现 `Default` 且 `Config` 无测试构造器，则在测试里用 `parse_for_test("PulseRing { particleMode: \"ring\" }")` 构造并直接改字段。**以 config.rs 实际 API 为准调整测试构造代码。**）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test particle_state_ring_matches_previous_math`
Expected: FAIL，`cannot find function build_particle_params / particle_state_for_test`

- [ ] **Step 3: 实现 Rust 侧粒子参数 + 参考实现**

在 `src/main.rs` 加入（替换原 `compute_particles` 与 `vec2_angle`）：

```rust
/// Per-particle constant params (18 f32) uploaded to the GPU; the WGSL shader
/// derives positions from these + time, so the CPU does zero per-frame particle math.
fn build_particle_params(cfg: &crate::config::Config) -> [f32; MAX_PARTICLES * 18] {
    let mut out = [0.0f32; MAX_PARTICLES * 18];
    for (i, p) in cfg.particles.iter().take(MAX_PARTICLES).enumerate() {
        let o = i * 18;
        out[o] = p.x;
        out[o + 1] = p.y;
        out[o + 2] = p.angle;
        out[o + 3] = p.speed;
        out[o + 4] = p.size;
        out[o + 5] = p.size_end;
        out[o + 6..o + 10].copy_from_slice(&p.color);
        out[o + 10] = p.life;
        out[o + 11] = p.delay;
        out[o + 12] = p.drag;
        out[o + 13] = p.gravity;
        out[o + 14] = p.wave;
        out[o + 15] = p.fade_in;
        out[o + 16] = p.twinkle;
        out[o + 17] = p.spin_speed;
    }
    out
}

/// CPU reference implementation of the WGSL `particle_state` math. Compiled only for
/// tests so the GPU port stays pinned to verifiable numbers.
#[cfg(test)]
pub fn particle_state_for_test(
    params: &[f32; MAX_PARTICLES * 18],
    i: u32,
    t: f32,
    min_d: f32,
    cx: f32,
    cy: f32,
    amp: f32,
) -> ParticleStateRef {
    // 逐行对照 WGSL particle_state()（见 Task 6 Step 5）移植：
    // Burst/Orbit/Ring 三种模式的数学与旧 compute_particles 完全一致。
    unimplemented!("port from compute_particles, keep identical math")
}

#[cfg(test)]
pub struct ParticleStateRef {
    pub px: f32,
    pub py: f32,
    pub size: f32,
    pub alpha: f32,
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test particle_state_ring_matches_previous_math`
Expected: PASS（把旧 `compute_particles` 的 Ring 分支数学搬进 `particle_state_for_test`）

- [ ] **Step 5: WGSL 移植**

1. WGSL `struct Uniforms`：删除 `particles: array<f32, 1152>,`，追加：

```wgsl
        particle_params: array<f32, 576>,
        particle_amp: f32,
```

2. 在 `fs_main` 之前加：

```wgsl
    struct Particle {
        pos: vec2<f32>,
        vel: vec2<f32>,
        size: f32,
        alpha: f32,
        spin: f32,
    }

    // CPU 参考实现: src/main.rs particle_state_for_test（保持逐行一致）
    fn particle_state(i: u32, t: f32, min_d: f32, cx: f32, cy: f32, amp: f32) -> Particle {
        let o = i * 18u;
        let px0 = u.particle_params[o];
        let py0 = u.particle_params[o + 1u];
        let a0 = u.particle_params[o + 2u] * 0.01745329251; // deg -> rad
        let speed = u.particle_params[o + 3u];
        let size = u.particle_params[o + 4u];
        let size_end = u.particle_params[o + 5u];
        let life = max(u.particle_params[o + 10u], 0.1);
        let delay = u.particle_params[o + 11u];
        let drag = clamp(u.particle_params[o + 12u], 0.0, 20.0);
        let gravity = u.particle_params[o + 13u];
        let wave = u.particle_params[o + 14u];
        let fade_in = u.particle_params[o + 15u];
        let twinkle = u.particle_params[o + 16u];
        let spin = u.particle_params[o + 17u] * 0.01745329251 * t;
        let tt = t - delay;
        var px = 0.0;
        var py = 0.0;
        var vx = 0.0;
        var vy = 0.0;
        var alpha = 1.0;
        let size0 = size * min_d;
        if (tt < 0.0) {
            return Particle(vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 0.0), 0.0, 0.0, spin);
        }
        if (u.particle_mode == 1u) {
            // Burst
            let period = life;
            let phase = select(min(tt, period), tt % period, u.particle_loop == 1u);
            let fade = max(1.0 - phase / period, 0.0);
            let damp = select(1.0, exp(-drag * phase), drag > 0.001);
            let dist = select(speed * min_d * phase, speed * min_d * (1.0 - exp(-drag * phase)) / drag, drag > 0.001);
            let g = gravity * min_d;
            let grav = 0.5 * g * phase * phase;
            let wave_off = wave * min_d * sin(phase * 6.2832 * 1.5 + a0);
            px = cx + px0 * min_d + cos(a0) * dist - sin(a0) * wave_off;
            py = cy + py0 * min_d + sin(a0) * dist + grav + cos(a0) * wave_off;
            vx = cos(a0) * speed * min_d * damp;
            vy = sin(a0) * speed * min_d * damp + g * phase;
            alpha = fade;
        } else if (u.particle_mode == 2u) {
            // Orbit
            let w = speed * 0.01745329251;
            let th = a0 + w * tt;
            let r = max(sqrt(px0 * px0 + py0 * py0) * min_d, 1.0);
            px = cx + cos(th) * r;
            py = cy + sin(th) * r;
            vx = -sin(th) * w * r;
            vy = cos(th) * w * r;
        } else if (u.particle_mode == 3u) {
            // Ring
            let w = speed * 0.01745329251;
            let th = a0 + w * tt;
            let r = max((u.base_r + u.growth * amp + u.halo_size * 0.5 + px0) * min_d, 2.0);
            px = cx + cos(th) * r;
            py = cy + sin(th) * r;
            vx = -sin(th) * w * r;
            vy = cos(th) * w * r;
            alpha = 1.0;
        } else {
            return Particle(vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 0.0), 0.0, 0.0, spin);
        }
        var out_alpha = alpha;
        var out_size = size0;
        if (u.particle_mode != 3u) {
            let fade_in_f = select(1.0, min(tt / max(fade_in, 0.0001), 1.0), fade_in > 0.0);
            let tw = 1.0 - clamp(twinkle, 0.0, 1.0) * 0.5 * (1.0 + sin(tt * 12.0 + f32(i) * 1.7));
            out_alpha = alpha * fade_in_f * tw;
            out_size = size0 + (size_end * min_d - size0) * (1.0 - clamp(out_alpha, 0.0, 1.0));
        }
        return Particle(vec2<f32>(px, py), vec2<f32>(vx, vy), max(out_size, 0.5), out_alpha, spin);
    }
```

   **注意**：旧 `compute_particles` 中 Burst 的 wave 偏移为 `wx = -a0.sin() * wave_off; wy = a0.cos() * wave_off;`（`a0` 为角度），上面写成 `-sin(a0)*wave_off` / `cos(a0)*wave_off`——**移植时以旧代码为准逐行对照**（旧代码 a0 = `p.angle.to_radians()`）。

3. `fs_main` 粒子循环体改写：删除直接读 `u.particles[o..]` 的段，替换为：

```wgsl
        if (u.particle_mode != 0u && abs(dist - u.particle_band_r) < min_d * 0.25) {
            let trail_max = select(1.0, 0.0, u.particle_mode == 3u);
            let cx = u.resolution.x * 0.5;
            let cy = u.resolution.y * 0.5;
            for (var i = 0u; i < u.particle_count; i = i + 1u) {
                let st = particle_state(i, u.time, min_d, cx, cy, u.particle_amp);
                if (st.alpha <= 0.004) {
                    continue;
                }
                var t = 0.0;
                while (t <= trail_max) {
                    let ghost = st.pos - st.vel * t * 0.05;
                    let dd = in.pos.xy - ghost;
                    let cs = cos(-st.spin);
                    let sn = sin(-st.spin);
                    let lx = dd.x * cs - dd.y * sn;
                    let ly = dd.x * sn + dd.y * cs;
                    let r = st.size * (1.0 - t * 0.35);
                    var sd = length(vec2<f32>(lx, ly));
                    if (u.particle_shape == 1u) {
                        sd = max(abs(lx), abs(ly));
                    } else if (u.particle_shape == 2u) {
                        sd = abs(lx) + abs(ly);
                    } else if (u.particle_shape == 3u) {
                        let a = atan2(ly, lx);
                        let sp = 0.75 + 0.25 * cos(5.0 * a);
                        sd = length(vec2<f32>(lx, ly)) / sp;
                    }
                    let da = smoothstep(r + 1.0, max(r - 1.0, 0.0), sd) * st.alpha * (1.0 - t * 0.6);
                    if (da > p_a) {
                        p_a = da;
                        let po = i * 18u;
                        p_col = vec3<f32>(u.particle_params[po + 6u], u.particle_params[po + 7u], u.particle_params[po + 8u]) * da;
                    }
                    t = t + 1.0;
                }
            }
        }
```

- [ ] **Step 6: Rust uniform 与渲染器接线**

1. Rust `Uniforms`：删除 `particles: [f32; 1152]`，追加 `particle_params: [f32; MAX_PARTICLES * 18]` 与 `particle_amp: f32`（位置与 WGSL 一致）。
2. buffer 大小断言改为（Task 3 的 `10832 + 128` 基础上减去 1152*4 + 加 576*4 + 4，直接用新值 + 断言）：

```rust
        const UNIFORM_SIZE: u64 = std::mem::size_of::<Uniforms>() as u64;
        assert!(UNIFORM_SIZE <= 10832 + 128 - 1152 * 4 + 576 * 4 + 4, "uniform grew: {UNIFORM_SIZE}");
```

3. `render()` 签名改为：

```rust
    pub fn render(
        &mut self,
        bands: &[f32; NBANDS],
        spawn_scale: f32,
        spawn_effect: u32,
        spawn_t: f32,
        spawn_rot: f32,
        particle_params: &[f32; MAX_PARTICLES * 18],
        particle_amp: f32,
        now: f32,
    ) {
```

   `Uniforms { ... }` 中 `particles: *particles` 改为 `particle_params: *particle_params, particle_amp,`。
4. `main.rs`：`SceneFrame` 中 `particles: [f32; MAX_PARTICLES * PARTICLE_STRIDE]` 替换为 `particle_params: [f32; MAX_PARTICLES * 18]` 与 `particle_amp: f32`；`compute_scene` 删除 `compute_particles(...)` 调用，改为 `let particle_params = build_particle_params(&self.cfg); let particle_amp = self.ring_amp_smooth;`；`render_output` 调用 `renderer.render(&scene.render_bands, ..., &scene.particle_params, scene.particle_amp, elapsed)`。
5. 删除 `compute_particles`、`vec2_angle`、`MAX_PARTICLES * PARTICLE_STRIDE` 的常量使用（`PARTICLE_STRIDE` 常量一并删除）。

- [ ] **Step 7: 构建 + 测试**

Run: `cargo build 2>&1 | tail -3 && cargo test 2>&1 | tail -3`
Expected: 构建成功（无 dead_code warning 关于 compute_particles），全部测试通过

- [ ] **Step 8: 手动验证（重点）**

Run: `cargo run`，分别观察三种 `particleMode`（burst / orbit / ring）与四种 `particleShape`
Expected: 粒子位置/轨迹/淡入淡出/闪烁/拖尾与 GPU 化前**一致**（如差异明显，对照 `particle_state_for_test` 与 WGSL 逐行排查）；`PULSE_RING_PROFILE=1` 下 `particles=` 归零、`render=` 略降

- [ ] **Step 9: 提交**

```bash
git add src/main.rs src/draw.rs
git commit -m "perf: 粒子计算 GPU 化 —— 移除每帧 CPU 粒子循环，uniform 上传减半"
```

---

## Self-Review 记录

**1. Spec 覆盖：**
- Phase 0 基线测量 → Task 1 ✓
- Phase 1-1 粒子 GPU 化 → Task 6 ✓
- Phase 1-2 绘制批处理 → 保持单三角形 + 现有批处理架构，本计划未新增 draw call（单三角形已是最小 draw call；widget/粒子本就是 fragment 分支合并）；已在 Global Constraints 注明架构约束 ✓
- Phase 1-3 自适应帧率 → Task 4 ✓
- Phase 1-4 共享 GPU 资源 → 已有（instance/device/queue 共享，Task 2 进一步消除每屏 CPU 重复）；无需额外任务 ✓
- Phase 1-5 零分配帧循环 → Task 5（插件缓冲）+ Task 2（prepare_widgets 快照从「每屏克隆」降为「每帧一次」）✓
- Phase 1-6 音频线程化 → 已在音频线程（`start_audio` 返回 channel），无需任务 ✓
- 补充：widget 包围盒早退（Task 3）为额外收益，不在 spec 明列但符合「降低 GPU 像素工作」目标 ✓

**2. Placeholder 扫描：** 无 TBD/TODO；唯一 `unimplemented!` 在 Task 6 Step 3 是**有意为之**（TDD 红灯步骤，紧随的 Step 4 给出移植说明）。Task 6 Step 5 的 WGSL 与旧代码的 wave 符号差异已显式标注「以旧代码为准」。

**3. 类型一致性：**
- `SceneFrame` 字段在 Task 2 定义、Task 3/6 增补，引用处（`compute_scene`/`render_output`）同步标注 ✓
- `render()` 签名：Task 2 未改签名，Task 6 改签名并在同一任务内更新调用点 ✓
- `profile_mark`/`profile_maybe_log` 在 Task 1 定义，Task 2 复用 ✓
- `set_widget_bounds` 在 Task 3 定义，同一任务接线 ✓
- `WidgetConfig`/`ParticleConfig` 字段以 `src/config.rs` 实际 API 为准（测试构造代码有注明）✓
