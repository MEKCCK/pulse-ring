use std::num::NonZeroU32;
use std::ptr::NonNull;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shell::wlr_layer::{
        Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
        LayerSurfaceConfigure,
    },
    shell::WaylandSurface,
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_surface},
    Connection, Proxy, QueueHandle,
};

mod audio;
mod config;
mod draw;
mod lua;
mod lyrics;
mod plugin;
use audio::NBANDS;
use draw::RingRenderer;

const MAX_PARTICLES: usize = 96;
const PARTICLE_STRIDE: usize = 12;

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
    bar_energy: [f32; 64],
    overall: f32,
    widgets_cfg: Vec<crate::config::WidgetConfig>,
}

/// One full rendering instance per output (layer surface + wgpu surface + renderer).
struct OutputSurfaces {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    renderer: RingRenderer,
    width: u32,
    height: u32,
    configured: bool,
    closed: bool,
    frame_skip: u32,
}

struct App {
    compositor: CompositorState,
    layer_shell: LayerShell,
    start: std::time::Instant,
    registry_state: RegistryState,
    output_state: OutputState,
    cfg: config::Config,
    bands: [f32; NBANDS],
    audio_rx: crossbeam_channel::Receiver<[f32; NBANDS]>,
    /// wgpu instance/device/queue shared across all outputs.
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: wgpu::Adapter,
    display_handle: RawDisplayHandle,
    outputs: Vec<OutputSurfaces>,
    image_cache: Vec<(String, std::sync::Arc<ImageData>)>,
    font: std::sync::Arc<rusttype::Font<'static>>,
    // Per-widget clock cache: (last_text, tex_w, tex_h, tex_index)
    clock_cache: [(String, u32, u32, u32); 8],
    texture_slots: Vec<Option<ImageData>>,
    /// Which texture slots changed this frame and still need a GPU upload.
    texture_dirty: Vec<bool>,
    /// True when the cover image changed and every renderer's atlas needs it.
    cover_dirty: bool,
    widget_uvs: [(f32, f32, f32, f32); 32],
    cover_rx: std::sync::mpsc::Receiver<ImageData>,
    last_cover_path: String,
    cover_tex_index: usize,
    cover_loaded: bool,
    cover_aspect: f32,
    current_cover: Option<ImageData>,
    cover_slot: usize,
    lua_state: lua::LuaState,
    plugins: Vec<plugin::LoadedPlugin>,
    plugin_tex: Vec<Option<(u32, u32, Vec<u8>)>>,
    plugin_smooth_bands: [f32; 128],
    music: lua::MusicInfo,
    ring_amp_smooth: f32,
    last_music_poll: f32,
    profile: ProfileStats,
    profile_enabled: bool,
    profile_frames: u32,
    interval: std::time::Duration,
    idle_since: Option<f32>,
    max_fps: u32,
    /// Optional idle frame-rate cap (PULSE_RING_IDLE_FPS). None = always render at max_fps
    /// (smooth idle animation); some = drop to this rate after 2s without audio (battery).
    idle_fps: Option<u32>,
    plugin_buf: Vec<u8>,
    lyric_data: Option<lyrics::LyricData>,
    lyric_key: String,
    lyric_tx: std::sync::mpsc::Sender<String>,
    lyric_rx: std::sync::mpsc::Receiver<(String, Option<lyrics::LyricData>)>,
    lyric_pos_poll_elapsed: f32,
    /// Per-widget-slot raster cache for lyric banners: (signature, image).
    lyric_cache: Vec<Option<(String, ImageData)>>,
    /// Async banner rasteriser: (seq, request) sender / (seq, image) receiver.
    lyric_raster_tx: std::sync::mpsc::Sender<(u64, LyricRasterReq)>,
    lyric_raster_rx: std::sync::mpsc::Receiver<(u64, ImageData)>,
    lyric_raster_seq: u64,
    /// In-flight request tags: (seq, widget slot, signature) — results only land if
    /// the signature is still current, so stale renders are dropped.
    lyric_raster_pending: Vec<(u64, usize, String)>,
    /// Line-change transition state.
    lyric_cur_idx: i32,
    lyric_line_changed_at: f32,
}

fn main() {
    env_logger::init();

    let mut cfg = config::Config::load(&config::config_path());
    let audio_rx = audio::start_audio(cfg.sensitivity, cfg.decay);

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr layer shell is not available");

    // Initialise wgpu against this Wayland connection. Devices are shared by all surfaces.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr() as *mut _).unwrap(),
    ));
    // A dummy surface on a scratch wl_surface so we can pick a compatible adapter; it is
    // immediately destroyed — the real surfaces are created per-output in new_output().
    let scratch_surface = compositor.create_surface(&qh);
    let scratch_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(scratch_surface.id().as_ptr() as *mut _).unwrap(),
    ));
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(raw_display_handle),
        raw_window_handle: scratch_handle,
    };
    let scratch_wgpu = unsafe { instance.create_surface_unsafe(target) }
        .expect("create scratch wgpu surface");

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&scratch_wgpu),
        ..Default::default()
    }))
    .expect("no suitable GPU adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))
        .expect("failed to acquire wgpu device");

    // Drop the scratch surfaces; real ones are created in new_output().
    drop(scratch_wgpu);

    let lua_script = cfg.lua_script.clone();
    let lua_state = lua::LuaState::new(lua_script.as_deref(), &mut cfg);
    let (lyric_tx, lyric_rx) = spawn_lyric_thread();
    let (lyric_raster_tx, lyric_raster_rx) = spawn_lyric_raster_thread();
    let mut app = App {
        compositor,
        layer_shell,
        start: std::time::Instant::now(),
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        cfg,
        bands: [0.0; NBANDS],
        audio_rx,
        instance,
        device,
        queue,
        adapter,
        display_handle: raw_display_handle,
        outputs: Vec::new(),
        image_cache: Vec::new(),
        font: std::sync::Arc::new(load_font()),
        clock_cache: std::array::from_fn(|_| (String::new(), 0, 0, 0)),
        texture_slots: vec![None; 16],
        texture_dirty: vec![false; 16],
        cover_dirty: true,
        widget_uvs: [(0.0, 0.0, 0.0, 0.0); 32],
        cover_rx: spawn_cover_thread(),
        last_cover_path: String::new(),
        cover_tex_index: 0,
        cover_loaded: false,
        cover_aspect: 1.0,
        current_cover: None,
        cover_slot: 0,
        lua_state,
        plugins: plugin::load_plugins_with_log(),
        plugin_tex: Vec::new(),
        plugin_smooth_bands: [0.0; 128],
        music: lua::MusicInfo::default(),
        ring_amp_smooth: 0.0,
        last_music_poll: -10.0,
        profile: ProfileStats::default(),
        profile_enabled: std::env::var("PULSE_RING_PROFILE").is_ok(),
        profile_frames: 0,
        interval: std::time::Duration::from_millis(33),
        idle_since: None,
        max_fps: std::env::var("PULSE_RING_MAX_FPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
            .clamp(15, 60),
        idle_fps: std::env::var("PULSE_RING_IDLE_FPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(|v: u32| v.clamp(5, 30)),
        plugin_buf: Vec::new(),
        lyric_data: None,
        lyric_key: String::new(),
        lyric_tx,
        lyric_rx,
        lyric_pos_poll_elapsed: 0.0,
        lyric_cache: vec![None; 32],
        lyric_raster_tx,
        lyric_raster_rx,
        lyric_raster_seq: 0,
        lyric_raster_pending: Vec::new(),
        lyric_cur_idx: -1,
        lyric_line_changed_at: 0.0,
    };

    // Wait for the first configure (outputs sized) via blocking dispatch, then switch to a
    // timed render loop (adaptive ~30fps active / 5fps idle) so the compositor only
    // recomposites on our updates.
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
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _f: i32) {}
    fn transform_changed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _t: wl_output::Transform) {}
    fn surface_enter(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _o: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _o: &wl_output::WlOutput) {}

    fn frame(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _t: u32) {
        // Rendering is driven by the timed tick() loop (~15 fps); frame callbacks are only
        // used to keep the surface presented.
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _c: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.outputs.iter().any(|o| o.output == output) {
            return;
        }
        // Create a layer surface bound to this specific output.
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("pulse-ring"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();

        // wgpu surface for this output's wl_surface.
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).unwrap(),
        ));
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(self.display_handle),
            raw_window_handle,
        };
        let wgpu_surface = unsafe { self.instance.create_surface_unsafe(target) }
            .expect("create wgpu surface for output");

        let renderer = RingRenderer::new(
            self.device.clone(),
            self.queue.clone(),
            wgpu_surface,
            &self.adapter,
            &self.cfg,
            self.outputs.len() as u32,
        );

        log::info!("added surface for output {}", output.id());
        self.outputs.push(OutputSurfaces {
            output,
            layer,
            renderer,
            width: 0,
            height: 0,
            configured: false,
            closed: false,
            frame_skip: 0,
        });
    }

    fn update_output(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output == output) {
            if let Some(info) = self.output_state.info(&output) {
                if let Some((w, h)) = info.logical_size {
                    o.width = w.max(0) as u32;
                    o.height = h.max(0) as u32;
                }
            }
        }
    }

    fn output_destroyed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.outputs.retain(|o| o.output != output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.layer == *layer) {
            o.closed = true;
        }
    }

    fn configure(&mut self, _c: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _serial: u32) {
        if let Some(idx) = self.outputs.iter().position(|o| o.layer == *layer) {
            log::info!("configure for output idx={idx} size={:?}", configure.new_size);
            let cfg_new_size = configure.new_size;
            let o = &mut self.outputs[idx];
            if cfg_new_size.0 > 0 {
                o.width = cfg_new_size.0;
            }
            if cfg_new_size.1 > 0 {
                o.height = cfg_new_size.1;
            }
            let first = !o.configured;
            o.configured = true;
            // Only the configured render target gets an initial draw; other screens stay
            // blank (no buffer) so niri has nothing to composite for them.
            let is_target = self.cfg.render_screen < 0 || self.cfg.render_screen == idx as i32;
            if first && is_target {
                let _ = qh;
                let scene = self.compute_scene();
                self.render_output(idx, &scene);
            }
        }
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

smithay_client_toolkit::delegate_dispatch2!(App);

/// Startup expansion scale 0..1 (may overshoot for elastic/back easings).
fn spawn_scale_for(cfg: &crate::config::Config, elapsed: f32) -> f32 {
    use crate::config::{SpawnEffect, SpawnEase};
    if matches!(cfg.spawn_effect, SpawnEffect::None) {
        return 1.0;
    }
    let dur = cfg.spawn_duration.max(1.0) / 1000.0;
    let t = (elapsed / dur).min(1.0);
    let e = match cfg.spawn_ease {
        SpawnEase::OutCubic => 1.0 - (1.0 - t).powi(3),
        SpawnEase::OutBack => {
            let c1 = 1.70158;
            let c3 = c1 + 1.0;
            1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
        }
        SpawnEase::Elastic => {
            let c4 = std::f32::consts::PI * 2.5;
            if t == 0.0 { 0.0 } else if t == 1.0 { 1.0 } else {
                2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * c4).sin() + 1.0
            }
        }
        SpawnEase::Bounce => {
            let n1 = 7.5625;
            let d1 = 2.75;
            if t < 1.0 / d1 {
                n1 * t * t
            } else if t < 2.0 / d1 {
                let t = t - 1.5 / d1;
                n1 * t * t + 0.75
            } else if t < 2.5 / d1 {
                let t = t - 2.25 / d1;
                n1 * t * t + 0.9375
            } else {
                let t = t - 2.625 / d1;
                n1 * t * t + 0.984375
            }
        }
    };
    e.clamp(0.0, 1.35)
}

/// Compute per-frame particle layout: 32 slots x (x, y, size, alpha, r, g, b, a) in pixels.
fn compute_particles(
    cfg: &crate::config::Config,
    elapsed: f32,
    width: u32,
    height: u32,
    amp_avg: f32,
) -> [f32; MAX_PARTICLES * PARTICLE_STRIDE] {
    use crate::config::ParticleMode;
    let mut out = [0.0f32; MAX_PARTICLES * PARTICLE_STRIDE];
    let min_d = width.min(height) as f32;
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    for (slot, p) in cfg.particles.iter().take(MAX_PARTICLES).enumerate() {
        let o = slot * PARTICLE_STRIDE;
        let t = elapsed - p.delay;
        if t < 0.0 {
            continue;
        }
        let a0 = p.angle.to_radians();
        let (mut px, mut py, mut vx, mut vy, mut alpha, size0) = match cfg.particle_mode {
            ParticleMode::Burst => {
                let period = p.life.max(0.1);
                let phase = if cfg.particle_loop { t % period } else { t.min(period) };
                let fade = (1.0 - phase / period).max(0.0);
                // Drag-damped distance: d = v0 * (1 - e^(-drag*t)) / drag
                let drag = p.drag.clamp(0.0, 20.0);
                let dist = if drag > 0.001 {
                    p.speed * min_d * (1.0 - (-drag * phase).exp()) / drag
                } else {
                    p.speed * min_d * phase
                };
                // Gravity: 0.5 * g * t^2 along +y (edge fractions per s^2)
                let g = p.gravity * min_d;
                let grav = 0.5 * g * phase * phase;
                let wave_off = p.wave * min_d * (phase * 6.2832 * 1.5 + a0).sin();
                let wx = -a0.sin() * wave_off;
                let wy = a0.cos() * wave_off;
                (
                    cx + p.x * min_d + a0.cos() * dist + wx,
                    cy + p.y * min_d + a0.sin() * dist + grav + wy,
                    a0.cos() * p.speed * min_d * (if drag > 0.001 { (-drag * phase).exp() } else { 1.0 }),
                    a0.sin() * p.speed * min_d * (if drag > 0.001 { (-drag * phase).exp() } else { 1.0 }) + g * phase,
                    fade,
                    p.size * min_d,
                )
            }
            ParticleMode::Orbit => {
                let w = p.speed.to_radians();
                let th = a0 + w * t;
                let r = ((p.x * p.x + p.y * p.y).sqrt() * min_d).max(1.0);
                let dir = vec2_angle(th);
                (
                    cx + dir.0 * r,
                    cy + dir.1 * r,
                    -dir.1 * w * r,
                    dir.0 * w * r,
                    1.0,
                    p.size * min_d,
                )
            }
            ParticleMode::Ring => {
                // Orbit just outside the ring's *current* edge: the band swells and shrinks
                // with the music (mean band amplitude) plus a small fixed offset `x`, so the
                // particles always hug the ring without ever being swallowed by it.
                let w = p.speed.to_radians();
                let th = a0 + w * t;
                // Orbit follows the ring's outer edge through the low-passed amplitude, so the
                // band swells/settles smoothly and never twitches in and out.
                let r = ((cfg.base_radius + cfg.growth * amp_avg + cfg.halo_size * 0.5 + p.x) * min_d)
                    .max(2.0);
                let dir = vec2_angle(th);
                (
                    cx + dir.0 * r,
                    cy + dir.1 * r,
                    -dir.1 * w * r,
                    dir.0 * w * r,
                    1.0,
                    p.size * min_d,
                )
            }
            ParticleMode::None => continue,
        };
        // Ring-mode particles stay rock steady (a Saturn band reads as a band, not a flicker);
        // burst/orbit get fade-in, twinkle and size interpolation.
        let (alpha, size) = if cfg.particle_mode == ParticleMode::Ring {
            (1.0, size0)
        } else {
            let fade_in = if p.fade_in > 0.0 { (t / p.fade_in).min(1.0) } else { 1.0 };
            let tw = 1.0 - p.twinkle.clamp(0.0, 1.0) * 0.5 * (1.0 + (t * 12.0 + slot as f32 * 1.7).sin());
            let alpha = alpha * fade_in * tw;
            let size = size0 + (p.size_end * min_d - size0) * (1.0 - alpha).clamp(0.0, 1.0);
            (alpha, size)
        };
        out[o] = px;
        out[o + 1] = py;
        out[o + 2] = size.max(0.5);
        out[o + 3] = alpha;
        out[o + 4] = p.color[0];
        out[o + 5] = p.color[1];
        out[o + 6] = p.color[2];
        out[o + 7] = p.color[3] * alpha;
        out[o + 8] = p.spin_speed.to_radians() * t;
        out[o + 9] = vx;
        out[o + 10] = vy;
    }
    out
}

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

/// Per-widget conservative bounding radius in pixels, used by the shader to skip
/// pixels outside each widget's region before running its SDF math.
fn compute_widget_bounds(widgets: &[crate::config::WidgetConfig], width: u32, height: u32) -> [f32; 32] {
    use crate::config::WidgetType;
    let mut out = [0.0f32; 32];
    let min_d = width.min(height) as f32;
    for (i, w) in widgets.iter().take(32).enumerate() {
        let b = match w.widget_type {
            WidgetType::Ring => (w.base_radius + w.growth + w.halo_size + 0.05) * w.size * min_d,
            WidgetType::Bars => w.size.max(w.bar_height) * min_d * 1.05,
            WidgetType::Clock | WidgetType::Analog => (w.size * 0.5 + w.dial_border) * min_d + min_d * 0.01,
            WidgetType::Image | WidgetType::Cover | WidgetType::Lyric => w.size * min_d * 0.75 + (w.border_width + w.cover_growth) * min_d,
            WidgetType::Plugin => w.size * min_d * 0.75,
        };
        out[i] = b.max(1.0);
    }
    out
}

/// Frame interval in ms for the given target fps.
fn frame_interval_ms(fps: u32) -> u64 {
    (1000 / fps.clamp(15, 60)).max(16) as u64
}

fn vec2_angle(a: f32) -> (f32, f32) {
    (a.cos(), a.sin())
}


/// Current time as "HH:MM" (system local time, no chrono dependency).
/// Current local time parts: (hour, minute, second, sub-second fraction).
pub fn main_now_hmsparts() -> (i32, i32, i32, f32) {
    now_hmsparts()
}

fn now_hmsparts() -> (i32, i32, i32, f32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    (tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_nanos() as f32 / 1e9)
}

/// Background lyric fetcher. The App sends a track key (`title\u{1}artist`) when the
/// track changes; the thread resolves local -> cache -> NetEase and replies with the
/// parsed lyric data tagged with the same key.
fn spawn_lyric_thread() -> (
    std::sync::mpsc::Sender<String>,
    std::sync::mpsc::Receiver<(String, Option<lyrics::LyricData>)>,
) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<(String, Option<lyrics::LyricData>)>();
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg_dir = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(&home).join(".config"))
        .join("pulse-ring")
        .join("lyrics");
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(&home).join(".cache"))
        .join("pulse-ring")
        .join("lyrics");
    std::thread::spawn(move || {
        while let Ok(key) = cmd_rx.recv() {
            let (title, artist) = match key.split_once('\u{1}') {
                Some((t, a)) => (t, a),
                None => (key.as_str(), ""),
            };
            let data = lyrics::fetch_lyrics(
                title,
                artist,
                &cfg_dir.to_string_lossy(),
                &cache_dir.to_string_lossy(),
            )
            .map(|text| lyrics::parse_lrc(&text));
            log::info!("lyric: fetched {} for '{}'", if data.is_some() { "ok" } else { "none" }, title);
            if res_tx.send((key, data)).is_err() {
                break;
            }
        }
    });
    (cmd_tx, res_rx)
}


/// A lyric banner rasterisation request. Sent to the worker thread; the worker
/// replies with `(seq, ImageData)`. Keeping rasterisation off the main thread makes
/// word-level karaoke updates (and the heavier glow/transition rendering) hitch-free.
struct LyricRasterReq {
    seq: u64,
    font: std::sync::Arc<rusttype::Font<'static>>,
    prev: Option<String>,
    current: String,
    next: Option<String>,
    words: Vec<(f32, f32, String)>,
    word_idx: usize,
    progress: f32,
    style: LyricStyle,
    alpha: f32,
    y_off: f32,
}

/// Spawn the lyric banner rasteriser worker. Returns (request sender, result receiver).
fn spawn_lyric_raster_thread() -> (
    std::sync::mpsc::Sender<(u64, LyricRasterReq)>,
    std::sync::mpsc::Receiver<(u64, ImageData)>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<(u64, LyricRasterReq)>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<(u64, ImageData)>();
    std::thread::spawn(move || {
        while let Ok((seq, req)) = rx.recv() {
            let img = rasterize_lyric_image(
                &req.font,
                req.prev.as_deref(),
                &req.current,
                req.next.as_deref(),
                &req.words,
                req.word_idx,
                req.progress,
                &req.style,
                req.alpha,
                req.y_off,
            );
            if let Some(img) = img {
                if res_tx.send((seq, img)).is_err() {
                    break;
                }
            }
        }
    });
    (tx, res_rx)
}

/// Poll the MPRIS cover via `playerctl` every 2s, decode it, send RGBA through a channel.
fn spawn_cover_thread() -> std::sync::mpsc::Receiver<ImageData> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        loop {
            let art = std::process::Command::new("playerctl")
                .args(["metadata", "mpris:artUrl"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(url) = art {
                let path = url.strip_prefix("file://").map(str::to_string).unwrap_or_else(|| url.clone());
                if last.as_deref() != Some(&path) {
                    last = Some(path.clone());
                    log::info!("cover: new art {path}");
                    match load_image_path(&path) {
                        Some(img) => { log::info!("cover: decoded {}x{}", img.w, img.h); let _ = tx.send(img); }
                        None => log::warn!("cover: decode failed {path}"),
                    }
                }
            } else {
                log::warn!("cover: no artUrl");
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    rx
}

/// Decode a PNG or JPEG file into RGBA (scaled to fit 256 slot).
fn load_image_path(path: &str) -> Option<ImageData> {
    let expanded = path.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
    let bytes = std::fs::read(&expanded).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let img = ImageData { w, h, rgba: rgba.into_raw() };
    Some(fit_slot(img))
}

/// Current local time as two lines: "HH:MM\nMM-DD" (libc localtime_r, system timezone).
fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = now;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!(
        "{:02}:{:02}\n{:02}-{:02}",
        tm.tm_hour, tm.tm_min, tm.tm_mon + 1, tm.tm_mday
    )
}


/// Simple RGBA image holder.
#[derive(Clone)]
struct ImageData {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

/// Load a system font for clock rendering (Noto Sans, fallback DejaVu).
fn load_font() -> rusttype::Font<'static> {
    // JetBrains Maple Mono (contains Chinese + Latin glyphs).
    let candidates = [
        "/usr/share/fonts/TTF/JetBrains-Maple-Mono-NF-XX-XX/JetBrainsMapleMono-Regular.ttf",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for p in candidates {
        if let Ok(data) = std::fs::read(p) {
            if p.ends_with(".ttc") {
                for idx in 0..8 {
                    if let Some(f) = rusttype::Font::try_from_vec_and_index(data.clone(), idx) {
                        if f.glyph('中').id().0 > 0 {
                            return f;
                        }
                    }
                }
            } else if let Some(f) = rusttype::Font::try_from_vec(data) {
                if f.glyph('中').id().0 > 0 {
                    return f;
                }
            }
        }
    }
    panic!("no usable system font found");
}

/// Decode a PNG file to RGBA.
fn load_png(path: &str) -> Option<ImageData> {
    let data = std::fs::read(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).ok()?;
    let w = info.width;
    let h = info.height;
    let bytes = &buf[..info.buffer_size()];
    let (rgba, w, h) = match info.color_type {
        png::ColorType::Rgba => (bytes.to_vec(), w, h),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(bytes.len() / 3 * 4);
            for c in bytes.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            (out, w, h)
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(bytes.len() * 4);
            for &g in bytes {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            (out, w, h)
        }
        _ => return None,
    };
    Some(ImageData { w, h, rgba })
}

/// Scale an image down (bilinear-ish) to fit a 1024x1024 atlas slot, keeping aspect.
fn fit_slot(img: ImageData) -> ImageData {
    const MAX: u32 = 1024;
    if img.w <= MAX && img.h <= MAX {
        return img;
    }
    let scale = (MAX as f32 / img.w as f32).min(MAX as f32 / img.h as f32);
    let nw = ((img.w as f32 * scale).floor() as u32).max(1);
    let nh = ((img.h as f32 * scale).floor() as u32).max(1);
    let mut out = ImageData { w: nw, h: nh, rgba: vec![0u8; (nw * nh * 4) as usize] };
    for y in 0..nh {
        for x in 0..nw {
            let sx = ((x as f32 + 0.5) / scale - 0.5).max(0.0) as usize;
            let sy = ((y as f32 + 0.5) / scale - 0.5).max(0.0) as usize;
            let si = (sy * img.w as usize + sx) * 4;
            let di = ((y * nw + x) * 4) as usize;
            out.rgba[di..di + 4].copy_from_slice(&img.rgba[si..si + 4]);
        }
    }
    out
}

/// Rasterise a text string (may contain '\n' lines) to RGBA at the given font size.
fn rasterize_text(font: &rusttype::Font, text: &str, size_pt: f32, color: [f32; 4]) -> ImageData {
    // pt -> px at 96 DPI (13.5pt = 18px)
    let size_px = size_pt * 96.0 / 72.0;
    let scale = rusttype::Scale { x: size_px, y: size_px };
    let v_metrics = font.v_metrics(scale);
    let line_h = (v_metrics.ascent - v_metrics.descent).ceil() as u32;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut g_w = 1u32;
    for line in &lines {
        let w: u32 = font
            .layout(line, scale, rusttype::point(0.0, 0.0))
            .map(|g| g.unpositioned().h_metrics().advance_width.ceil() as u32)
            .sum();
        g_w = g_w.max(w);
    }
    let g_h = (line_h * lines.len() as u32).max(1);
    let mut img = ImageData { w: g_w, h: g_h, rgba: vec![0u8; (g_w * g_h * 4) as usize] };
    let (cr, cg, cb, ca) = (color[0], color[1], color[2], color[3]);
    for (li, line) in lines.iter().enumerate() {
        let y_base = (li as u32 * line_h) as f32;
        let glyphs: Vec<rusttype::PositionedGlyph> = font
            .layout(line, scale, rusttype::point(0.0, v_metrics.ascent + y_base))
            .collect();
        for g in &glyphs {
            if let Some(bb) = g.pixel_bounding_box() {
                g.draw(|x, y, cov| {
                    let px = bb.min.x as u32 + x;
                    let py = bb.min.y as u32 + y;
                    if px < img.w && py < img.h {
                        let a = cov * ca;
                        let o = ((py * img.w + px) * 4) as usize;
                        img.rgba[o] = (cr * 255.0 * a) as u8;
                        img.rgba[o + 1] = (cg * 255.0 * a) as u8;
                        img.rgba[o + 2] = (cb * 255.0 * a) as u8;
                        img.rgba[o + 3] = (a * 255.0) as u8;
                    }
                });
            }
        }
    }
    img
}

/// Compose prev/current/next lyric lines into one RGBA banner. The current line is drawn
/// in `active` colour; its first `progress` fraction is overpainted in `karaoke` colour
/// (a smooth per-word highlight like a karaoke bar). Returns None when there is no text.
/// Styling for the lyric banner (word karaoke colours).
struct LyricStyle {
    font_size: f32,
    /// Unsung words + prev/next base colour.
    base: [f32; 4],
    /// Already-sung words.
    sung: [f32; 4],
    /// Current word highlight.
    cur: [f32; 4],
    /// Glow colour behind the current word.
    glow: [f32; 4],
    show_prev_next: bool,
}

/// Draw `text` into `img` with the given colour/alpha; optional `clip_x` limits
/// drawing to pixels left of it (karaoke progress on non-word lines).
fn blit_text(
    img: &mut ImageData,
    font: &rusttype::Font,
    text: &str,
    scale: rusttype::Scale,
    base_x: f32,
    baseline_y: f32,
    color: [f32; 4],
    alpha: f32,
    clip_x: Option<f32>,
) {
    let glyphs: Vec<rusttype::PositionedGlyph> =
        font.layout(text, scale, rusttype::point(base_x, baseline_y)).collect();
    for g in &glyphs {
        if let Some(bb) = g.pixel_bounding_box() {
            g.draw(|x, y, cov| {
                let px = (bb.min.x as u32).wrapping_add(x);
                let py = (bb.min.y as u32).wrapping_add(y);
                if let Some(cx) = clip_x {
                    if px as f32 >= cx {
                        return;
                    }
                }
                if px < img.w && py < img.h {
                    let a = cov * color[3] * alpha;
                    if a <= 0.004 {
                        return;
                    }
                    let o = ((py * img.w + px) * 4) as usize;
                    img.rgba[o] = (color[0] * 255.0 * a) as u8;
                    img.rgba[o + 1] = (color[1] * 255.0 * a) as u8;
                    img.rgba[o + 2] = (color[2] * 255.0 * a) as u8;
                    img.rgba[o + 3] = (a * 255.0) as u8;
                }
            });
        }
    }
}

/// Draw a word; if `glow` is set, first stamp a soft halo by re-drawing the word at
/// eight small offsets, then the crisp word on top.
fn blit_word(
    img: &mut ImageData,
    font: &rusttype::Font,
    text: &str,
    scale: rusttype::Scale,
    base_x: f32,
    baseline_y: f32,
    color: [f32; 4],
    glow: Option<[f32; 4]>,
    alpha: f32,
) {
    if let Some(gc) = glow {
        if gc[3] > 0.004 {
            let r = 2.5;
            for (ox, oy) in [
                (r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r),
                (r * 0.71, r * 0.71), (r * 0.71, -r * 0.71),
                (-r * 0.71, r * 0.71), (-r * 0.71, -r * 0.71),
            ] {
                blit_text(img, font, text, scale, base_x + ox, baseline_y + oy, gc, alpha * 0.45, None);
            }
        }
    }
    blit_text(img, font, text, scale, base_x, baseline_y, color, alpha, None);
}

fn dim_color(c: [f32; 4]) -> [f32; 4] {
    [c[0] * 0.62, c[1] * 0.62, c[2] * 0.62, c[3] * 0.72]
}

fn mix_color(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Colour at gradient position t (0..1) across the stops.
fn gradient_color(stops: &[[f32; 4]], t: f32) -> [f32; 4] {
    if stops.is_empty() {
        return [1.0, 1.0, 1.0, 1.0];
    }
    if stops.len() == 1 {
        return stops[0];
    }
    let t = t.clamp(0.0, 1.0);
    let f = t * (stops.len() - 1) as f32;
    let i = (f.floor() as usize).min(stops.len() - 2);
    mix_color(stops[i], stops[i + 1], f - i as f32)
}

/// Draw `text` clipped to `clip_x` with a horizontal gradient across the first
/// `lit_w` pixels (base_x..base_x+lit_w). Used for the lit (already-sung) karaoke
/// portion of the current line.
fn blit_gradient_clipped(
    img: &mut ImageData,
    font: &rusttype::Font,
    text: &str,
    scale: rusttype::Scale,
    base_x: f32,
    baseline_y: f32,
    stops: &[[f32; 4]],
    lit_w: f32,
    clip_x: f32,
    alpha: f32,
) {
    let glyphs: Vec<rusttype::PositionedGlyph> =
        font.layout(text, scale, rusttype::point(base_x, baseline_y)).collect();
    for g in &glyphs {
        if let Some(bb) = g.pixel_bounding_box() {
            g.draw(|x, y, cov| {
                let px = (bb.min.x as u32).wrapping_add(x);
                let py = (bb.min.y as u32).wrapping_add(y);
                if px as f32 >= clip_x {
                    return;
                }
                if px < img.w && py < img.h {
                    let t = if lit_w > 0.5 {
                        ((px as f32 - base_x) / lit_w).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let c = gradient_color(stops, t);
                    let a = cov * c[3] * alpha;
                    if a <= 0.004 {
                        return;
                    }
                    let o = ((py * img.w + px) * 4) as usize;
                    img.rgba[o] = (c[0] * 255.0 * a) as u8;
                    img.rgba[o + 1] = (c[1] * 255.0 * a) as u8;
                    img.rgba[o + 2] = (c[2] * 255.0 * a) as u8;
                    img.rgba[o + 3] = (a * 255.0) as u8;
                }
            });
        }
    }
}

/// Rasterize the lyric banner (prev/current/next) with Folia-style karaoke:
/// the current line colours each word by state — already sung words in `sung`,
/// the current word in `cur` with a glow halo, unsung words in `base`. Lines without
/// word timestamps use a smooth `progress` clip instead. `alpha`/`y_off` drive the
/// line-change transition (fade + slide up). Runs on a worker thread (never the main
/// render loop) — this function must stay Send-friendly (it only reads its args).
fn rasterize_lyric_image(
    font: &rusttype::Font,
    prev: Option<&str>,
    current: &str,
    next: Option<&str>,
    words: &[(f32, f32, String)],
    word_idx: usize,
    progress: f32,
    st: &LyricStyle,
    alpha: f32,
    y_off: f32,
) -> Option<ImageData> {
    let current = current.trim();
    if current.is_empty() {
        return None;
    }
    let sub_f = 0.62;
    let cur_scale = rusttype::Scale::uniform(st.font_size);
    let sub_scale = rusttype::Scale::uniform(st.font_size * sub_f);
    let metrics = |sc: rusttype::Scale| {
        let v = font.v_metrics(sc);
        ((v.ascent - v.descent).ceil() as u32, v.ascent)
    };
    let (cur_h, cur_ascent) = metrics(cur_scale);
    let (sub_h, sub_ascent) = metrics(sub_scale);
    let line_w = |text: &str, sc: rusttype::Scale| -> u32 {
        font.layout(text, sc, rusttype::point(0.0, 0.0))
            .map(|g| g.unpositioned().h_metrics().advance_width.ceil() as u32)
            .sum()
    };
    let prev_line = if st.show_prev_next { prev.unwrap_or("").trim() } else { "" };
    let next_line = if st.show_prev_next { next.unwrap_or("").trim() } else { "" };
    let gap = (cur_h as f32 * 0.22).ceil() as u32;
    let show_prev = !prev_line.is_empty();
    let show_next = !next_line.is_empty();
    let sub_lines = (if show_prev { 1 } else { 0 }) + (if show_next { 1 } else { 0 });
    let cur_w: u32 = if !words.is_empty() {
        words.iter().map(|(_, _, t)| line_w(t, cur_scale)).sum()
    } else {
        line_w(current, cur_scale)
    };
    let w = cur_w
        .max(if show_prev { line_w(prev_line, sub_scale) } else { 0 })
        .max(if show_next { line_w(next_line, sub_scale) } else { 0 })
        .max(8);
    let shift = y_off.abs().ceil() as u32;
    let h = (cur_h
        + if sub_lines > 0 {
            sub_h * sub_lines as u32 + gap * 2
        } else {
            0
        })
    .max(4)
        + shift
        + 2;
    let mut img = ImageData {
        w,
        h,
        rgba: vec![0u8; (w * h * 4) as usize],
    };
    let y_shift = y_off as i64;
    let y_at = |top: u32| -> f32 { (top as i64 + y_shift).max(0) as f32 };
    let center_x = |text: &str, sc: rusttype::Scale| -> f32 {
        ((w as i64 - line_w(text, sc) as i64) / 2).max(0) as f32
    };
    // Vertical layout: [prev] [gap] [current] [gap] [next], current centered.
    let cur_top = if show_prev { sub_h + gap } else { 0 };
    let next_top = cur_top + cur_h + gap;
    if show_prev {
        blit_text(&mut img, font, prev_line, sub_scale, center_x(prev_line, sub_scale),
            sub_ascent + y_at(0), dim_color(st.base), alpha, None);
    }
    let base_x = ((w as i64 - cur_w as i64) / 2).max(0) as f32;
    let baseline = cur_ascent + y_at(cur_top);
    if !words.is_empty() {
        let mut wx = base_x;
        for (i, (_, _, wt)) in words.iter().enumerate() {
            let color = if i < word_idx {
                st.sung
            } else if i == word_idx {
                st.cur
            } else {
                st.base
            };
            // The current word gets a strong halo; already-sung words a softer one.
            let glow = if i == word_idx {
                Some(st.glow)
            } else if i < word_idx {
                Some([st.glow[0], st.glow[1], st.glow[2], st.glow[3] * 0.5])
            } else {
                None
            };
            blit_word(&mut img, font, wt, cur_scale, wx, baseline, color, glow, alpha);
            wx += line_w(wt, cur_scale) as f32;
        }
    } else {
        // Karaoke "dim-to-lit": the full line stays dim, the already-sung portion
        // lights up with a flowing gradient and a glow halo as progress advances.
        blit_text(&mut img, font, current, cur_scale, base_x, baseline, dim_color(st.base), alpha, None);
        let lit_w = progress.clamp(0.0, 1.0) * cur_w as f32;
        let clip_x = base_x + lit_w;
        if lit_w > 1.0 {
            if st.glow[3] > 0.004 {
                let r = 3.0;
                for (ox, oy) in [
                    (r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r),
                    (r * 0.71, r * 0.71), (r * 0.71, -r * 0.71),
                    (-r * 0.71, r * 0.71), (-r * 0.71, -r * 0.71),
                ] {
                    blit_text(&mut img, font, current, cur_scale, base_x + ox, baseline + oy,
                        st.glow, alpha * 0.4, Some(clip_x));
                }
            }
            let stops = [st.sung, mix_color(st.sung, st.cur, 0.55), st.cur];
            blit_gradient_clipped(&mut img, font, current, cur_scale, base_x, baseline, &stops, lit_w, clip_x, alpha);
        }
    }
    if show_next {
        blit_text(&mut img, font, next_line, sub_scale, center_x(next_line, sub_scale),
            sub_ascent + y_at(next_top), dim_color(st.base), alpha, None);
    }
    Some(img)
}

impl App {
    /// Compute widget uniform data (12 f32 each). Returns the 96-float layout.
    /// `widgets` is the per-frame snapshot taken once in compute_scene.
    fn prepare_widgets(&mut self, widgets: &[crate::config::WidgetConfig]) -> [f32; 1280] {
        use crate::config::WidgetType;
        let mut data = [0.0f32; 1280];
        let mut tex_index = 0u32;
        // Reserve slot 3 for the album cover (clocks/images use 0..2).
        self.cover_tex_index = 3;
        for (slot, w) in widgets.iter().enumerate() {
            let o = slot * 40;
            data[o] = match w.widget_type {
                WidgetType::Ring => 0.0,
                WidgetType::Image => 1.0,
                WidgetType::Clock => 2.0,
                WidgetType::Bars => 3.0,
                WidgetType::Cover => 4.0,
                WidgetType::Analog => 5.0,
                WidgetType::Plugin => 6.0,
                WidgetType::Lyric => 1.0, // textured quad, like Image
            };
            data[o + 1] = w.x;
            data[o + 2] = w.y;
            data[o + 3] = w.size;
            data[o + 4] = w.alpha;
            data[o + 5] = w.rotate.to_radians();
            let (cux, cuy, cuw, cuh) = self.widget_uvs[slot];
            data[o + 7] = cux;
            data[o + 8] = cuy;
            data[o + 9] = cuw;
            data[o + 10] = cuh;
            // ring widget style
            data[o + 12] = match w.shape {
                crate::config::Shape::Ring => 0.0,
                crate::config::Shape::Square => 1.0,
                crate::config::Shape::Diamond => 2.0,
                crate::config::Shape::Hexagon => 3.0,
                crate::config::Shape::Triangle => 4.0,
                crate::config::Shape::Star => 5.0,
                crate::config::Shape::Flower => 6.0,
            };
            data[o + 13] = w.corners.max(2.0);
            data[o + 14] = w.spikiness.clamp(0.0, 1.0);
            data[o + 15] = match w.color_mode {
                crate::config::ColorMode::Hue => 0.0,
                crate::config::ColorMode::Solid => 1.0,
                crate::config::ColorMode::Gradient => 2.0,
            };
            data[o + 16] = w.dash_count.max(0.0);
            data[o + 17] = w.dash_ratio.clamp(0.0, 1.0);
            data[o + 18] = w.ring_width.max(1.0);
            data[o + 19] = w.base_radius.max(0.01);
            data[o + 20] = w.growth.max(0.0);
            data[o + 21] = w.halo_strength.clamp(0.0, 1.0);
            data[o + 22] = w.halo_size.max(0.0);
            data[o + 39] = match w.band_mode {
                crate::config::BandMode::Full => 0.0,
                crate::config::BandMode::Bass => 1.0,
                crate::config::BandMode::Mid => 2.0,
                crate::config::BandMode::Treble => 3.0,
                crate::config::BandMode::Energy => 4.0,
            };
            // palette at 23..39
            let pal = if w.colors.len() >= 4 {
                &w.colors[..4]
            } else if w.colors.len() >= 1 {
                &w.colors[..1]
            } else {
                &[[0.404, 0.314, 0.643, 1.0]]
            };
            for (ci, col) in pal.iter().enumerate() {
                let co = o + 23 + ci * 4;
                data[co] = col[0];
                data[co + 1] = col[1];
                data[co + 2] = col[2];
                data[co + 3] = col[3];
            }
            match w.widget_type {
                WidgetType::Plugin => {
                    // tex index points at the plugin's render slot (8 + plugin index)
                    let pidx = w
                        .plugin
                        .as_ref()
                        .and_then(|n| self.plugins.iter().position(|p| p.name() == n))
                        .unwrap_or(0);
                    let ti = (8 + pidx) as u32;
                    data[o + 6] = ti as f32;
                    data[o + 11] = 1.0; // square aspect default; updated when rendered
                    if tex_index <= ti {
                        tex_index = ti + 1;
                    }
                }
                WidgetType::Ring => {
                    data[o + 6] = 0.0;
                    data[o + 7] = 0.0;
                    data[o + 8] = 0.0;
                }
                WidgetType::Analog => {
                    // 18=tickCount, 19=hour angle, 20=minute angle, 21=second angle,
                    // 22=dial border, colors[0]=hand colour
                    data[o + 18] = w.tick_count.clamp(2.0, 24.0);
                    data[o + 22] = w.dial_border.max(0.0);
                    for (ci, ch) in w.color.iter().enumerate() {
                        data[o + 23 + ci] = *ch;
                    }
                    // hand angles (radians, 12 o'clock = -PI/2)
                    let t = now_hmsparts();
                    let sec = t.2 as f32 + t.3 as f32;
                    let min = t.1 as f32 + sec / 60.0;
                    let hour = (t.0 as f32 % 12.0) + min / 60.0;
                    data[o + 19] = (hour / 12.0 * 6.28318530718 - 1.5707963268);
                    data[o + 20] = (min / 60.0 * 6.28318530718 - 1.5707963268);
                    data[o + 21] = (sec / 60.0 * 6.28318530718 - 1.5707963268);
                }
                WidgetType::Cover => {
                    self.cover_slot = slot;
                    // tex_index points at the cover texture slot (set when loaded).
                    data[o + 6] = self.cover_tex_index as f32;
                    // 18=border width, 19=cover growth
                    data[o + 18] = w.border_width.max(0.0);
                    data[o + 19] = w.cover_growth.max(0.0);
                    data[o + 11] = self.cover_aspect;
                    // border colour from widget.color -> colors[0]
                    for (ci, ch) in w.color.iter().enumerate() {
                        data[o + 23 + ci] = *ch;
                    }
                    // Pull the latest cover from the MPRIS thread.
                    while let Ok(img) = self.cover_rx.try_recv() {
                        self.cover_loaded = true;
                        self.cover_aspect = img.h as f32 / img.w as f32;
                        self.current_cover = Some(img);
                        self.cover_dirty = true;
                        log::info!("cover: new cover stored ({}x{})", self.cover_aspect, 0);
                    }
                }
                WidgetType::Bars => {
                    // Reuse style slots: 18=bars count, 19=max height, 20=gap, 21=mirror.
                    data[o + 18] = w.bar_count.clamp(2.0, 64.0);
                    data[o + 19] = w.bar_height.max(0.01);
                    data[o + 20] = w.bar_gap.clamp(0.0, 0.9);
                    data[o + 21] = w.bar_mirror as u32 as f32;
                }
                WidgetType::Image => {
                    let src = match &w.source {
                        Some(s) => s.clone(),
                        None => continue,
                    };
                    let img = self.get_image(&src).cloned();
                    if let Some(img) = img {
                        let img = fit_slot(img);
                        let (iw, ih) = (img.w as f32, img.h as f32);
                        data[o + 6] = tex_index as f32;
                        data[o + 11] = ih / iw; // aspect
                        self.texture_slots[tex_index as usize] = Some(img);
                        self.texture_dirty[tex_index as usize] = true;
                        tex_index += 1;
                    }
                }
                WidgetType::Clock => {
                    let txt = chrono_now();
                    let (cached_text, cw, ch, cached_tex) = &self.clock_cache[slot];
                    let (cw, ch) = (*cw, *ch);
                    let mut ti = *cached_tex;
                    if &txt != cached_text || cw == 0 {
                        // 3x supersampling: sharper text when downscaled on screen.
                        let img = fit_slot(rasterize_text(&self.font, &txt, w.font_size * 3.0, w.color));
                        let (iw, ih) = (img.w, img.h);
                        ti = tex_index;
                        self.texture_slots[ti as usize] = Some(img);
                        self.texture_dirty[ti as usize] = true;
                        self.clock_cache[slot] = (txt.clone(), iw, ih, ti);
                        data[o + 11] = ih as f32 / iw as f32;
                        if ti >= tex_index {
                            tex_index = ti + 1;
                        }
                    } else {
                        data[o + 11] = ch as f32 / cw as f32;
                    }
                    data[o + 6] = ti as f32;
                }
                WidgetType::Lyric => {
                    // Current song lyric banner: prev/current/next lines with Folia-style
                    // per-word karaoke colouring baked into the rasterised texture.
                    // Rasterisation runs on a worker thread so word changes never hitch
                    // the render loop; the cached (possibly one-frame-stale) image keeps
                    // the display continuous while the new one arrives.
                    let Some(lt) = self.lyric_time() else { continue };
                    let Some(ldata) = &self.lyric_data else { continue };
                    let Some(ls) = lyrics::line_state(ldata, lt + w.lyric_offset) else { continue };
                    let elapsed = self.start.elapsed().as_secs_f32();
                    // Line-change transition: fade in + slide up over 0.25s.
                    if ls.index as i32 != self.lyric_cur_idx {
                        self.lyric_cur_idx = ls.index as i32;
                        self.lyric_line_changed_at = elapsed;
                    }
                    let tt = ((elapsed - self.lyric_line_changed_at) / 0.25).clamp(0.0, 1.0);
                    let ease = tt * tt * (3.0 - 2.0 * tt);
                    let alpha = 0.35 + 0.65 * ease;
                    let y_off = (1.0 - ease) * -24.0;
                    let cur = &ldata.lines[ls.index].text;
                    let prev = if w.show_prev_next && ls.index > 0 {
                        Some(ldata.lines[ls.index - 1].text.as_str())
                    } else {
                        None
                    };
                    let next = if w.show_prev_next {
                        ldata.lines.get(ls.index + 1).map(|l| l.text.as_str())
                    } else {
                        None
                    };
                    let words = &ldata.lines[ls.index].words;
                    let word_idx = ls.word.min(words.len().saturating_sub(1));
                    // Colours: colors[0]=base(未唱/上下行) colors[1]=已唱 colors[2]=当前字 colors[3]=辉光
                    let style = LyricStyle {
                        font_size: w.font_size,
                        base: w.colors.first().copied().unwrap_or([0.85, 0.9, 1.0, 1.0]),
                        sung: w.colors.get(1).copied().unwrap_or([1.0, 0.78, 0.35, 1.0]),
                        cur: w.colors.get(2).copied().unwrap_or([1.0, 1.0, 1.0, 1.0]),
                        glow: w.colors.get(3).copied().unwrap_or([0.7, 0.53, 1.0, 0.75]),
                        show_prev_next: w.show_prev_next,
                    };
                    // Full-precision progress in the signature: the karaoke bar advances
                    // every frame (rasterised on the worker thread) instead of jumping
                    // in coarse buckets, so the lit portion glides smoothly.
                    let sig = format!(
                        "{slot}|{}|{}|{}|{}|{}|{}|{:.4}|{:.3}|{}|{}|{:.3}",
                        self.lyric_key,
                        cur,
                        prev.unwrap_or(""),
                        next.unwrap_or(""),
                        word_idx,
                        words.len(),
                        ls.progress,
                        tt,
                        w.font_size,
                        w.show_prev_next,
                        alpha,
                    );
                    // Drain finished renders first (results arrive in order; only the
                    // newest request per slot stays in `pending`, so applying is safe).
                    while let Ok((rseq, rimg)) = self.lyric_raster_rx.try_recv() {
                        if let Some(pos) = self.lyric_raster_pending.iter().position(|(s, _, _)| *s == rseq) {
                            let (_, rslot, rsig) = self.lyric_raster_pending.remove(pos);
                            self.lyric_cache[rslot] = Some((rsig, rimg));
                        }
                    }
                    if self.lyric_raster_pending.len() > 16 {
                        self.lyric_raster_pending.drain(0..8);
                    }
                    let rendered: Option<ImageData> = match &self.lyric_cache[slot] {
                        Some((cs, im)) if *cs == sig => Some(im.clone()),
                        _ => {
                            // Cache miss: keep showing the stale banner, request async.
                            self.lyric_raster_seq += 1;
                            let seq = self.lyric_raster_seq;
                            let req = LyricRasterReq {
                                seq,
                                font: self.font.clone(),
                                prev: prev.map(str::to_string),
                                current: cur.clone(),
                                next: next.map(str::to_string),
                                words: words.clone(),
                                word_idx,
                                progress: ls.progress,
                                style,
                                alpha,
                                y_off,
                            };
                            let _ = self.lyric_raster_tx.send((seq, req));
                            // A new request supersedes any older in-flight one for this slot.
                            self.lyric_raster_pending.retain(|(_, s, _)| *s != slot);
                            self.lyric_raster_pending.push((seq, slot, sig));
                            self.lyric_cache[slot].as_ref().map(|(_, im)| im.clone())
                        }
                    };
                    if let Some(img) = rendered {
                        // Atlas slots are 1024px max — scale the banner down to fit.
                        let img = fit_slot(img);
                        let (iw, ih) = (img.w as f32, img.h as f32);
                        data[o + 6] = tex_index as f32;
                        data[o + 11] = ih / iw; // aspect
                        self.texture_slots[tex_index as usize] = Some(img);
                        self.texture_dirty[tex_index as usize] = true;
                        tex_index += 1;
                    } else {
                        continue;
                    }
                }
            }
        }
        data
    }

    fn get_image(&mut self, path: &str) -> Option<&ImageData> {
        // Simple cache; expand ~ in path.
        let expanded = path.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
        if let Some(pos) = self.image_cache.iter().position(|(p, _)| *p == expanded) {
            return Some(&self.image_cache[pos].1);
        }
        if let Some(img) = load_png(&expanded) {
            self.image_cache.push((expanded, std::sync::Arc::new(img)));
            return self.image_cache.last().map(|(_, d)| d.as_ref());
        }
        None
    }

    /// Refresh MPRIS music info (throttled by the cover thread cadence: cheap anyway).
    fn poll_music(&mut self) {
        let run = |args: &[&str]| -> Option<String> {
            std::process::Command::new("playerctl")
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let title = run(&["metadata", "xesam:title"]);
        let artist = run(&["metadata", "xesam:artist"]);
        // `playerctl position` prints seconds as a float ("5.834005"); some builds
        // print raw microseconds — handle both.
        let pos_us = run(&["position"]).and_then(|s| {
            let v: f64 = s.trim().parse().ok()?;
            Some(if v.abs() > 100_000.0 { v / 1_000_000.0 } else { v })
        });
        let status = run(&["status"]);
        if let Some(t) = title {
            let changed = self.music.title != t;
            if changed {
                // Track changed: try the local dir + disk cache instantly (no network);
                // fall back to an async online fetch so lyrics appear without waiting
                // on a round-trip for songs we have heard before.
                let home = std::env::var("HOME").unwrap_or_default();
                let cfg_dir = std::env::var("XDG_CONFIG_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from(&home).join(".config"))
                    .join("pulse-ring")
                    .join("lyrics");
                let cache_dir = std::env::var("XDG_CACHE_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from(&home).join(".cache"))
                    .join("pulse-ring")
                    .join("lyrics");
                let key = format!("{}\u{1}{}", t, artist.as_deref().unwrap_or(""));
                self.lyric_key = key.clone();
                let instant = lyrics::fetch_local_or_cache(
                    &t,
                    artist.as_deref().unwrap_or(""),
                    &cfg_dir.to_string_lossy(),
                    &cache_dir.to_string_lossy(),
                )
                .map(|text| lyrics::parse_lrc(&text));
                if instant.is_some() {
                    self.lyric_data = instant;
                    log::info!(
                        "lyric: instant cache hit ({} lines)",
                        self.lyric_data.as_ref().map_or(0, |d| d.lines.len())
                    );
                } else {
                    self.lyric_data = None;
                    let _ = self.lyric_tx.send(key);
                }
                self.music.title = t;
            }
        }
        if let Some(a) = artist {
            self.music.artist = a;
        }
        if let Some(us) = pos_us {
            self.music.position_sec = us as f32;
            self.lyric_pos_poll_elapsed = self.start.elapsed().as_secs_f32();
        }
        self.music.playing = status.as_deref() == Some("Playing");
        // Drain lyric fetch results; only accept the one matching the current track.
        while let Ok((key, data)) = self.lyric_rx.try_recv() {
            if key == self.lyric_key {
                self.lyric_data = data;
                log::info!("lyric: loaded {} lines", self.lyric_data.as_ref().map_or(0, |d| d.lines.len()));
            }
        }
    }

    /// Current lyric playback time: MPRIS position advanced by elapsed wall time while playing.
    fn lyric_time(&self) -> Option<f32> {
        if self.lyric_data.is_none() {
            return None;
        }
        let t = if self.music.playing {
            let dt = (self.start.elapsed().as_secs_f32() - self.lyric_pos_poll_elapsed).max(0.0);
            self.music.position_sec + dt
        } else {
            self.music.position_sec
        };
        Some(t)
    }

    /// Ask each plugin to render its RGBA texture, then store into texture_slots for
    /// `type: "plugin"` widgets (each plugin owns slot = 8 + plugin index).
    fn render_plugin_textures(&mut self) {
        let n = self.plugins.len();
        self.plugin_tex.resize(n, None);
        let (screen_w, screen_h) = self
            .outputs
            .first()
            .map(|o| (o.width, o.height))
            .unwrap_or((1920, 1080));
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
                // Keep the previous texture (if any); nothing new to upload.
                continue;
            }
            let w = req.width.min(512);
            let h = req.height.min(512);
            // Plugin writes a w×h image at the start of the buffer with row stride = w.
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let si = ((y * w + x) * 4) as usize;
                    rgba.extend_from_slice(&self.plugin_buf[si..si + 4]);
                }
            }
            self.plugin_tex[i] = Some((w, h, rgba));
        }
        // write plugin textures into texture_slots (so prepare_widgets picks them up)
        for (i, tex) in self.plugin_tex.iter().enumerate() {
            if let Some((w, h, rgba)) = tex {
                let ti = 8 + i;
                let img = ImageData { w: *w, h: *h, rgba: rgba.clone() };
                self.texture_slots[ti] = Some(img);
                self.texture_dirty[ti] = true;
            }
        }
    }

    /// Timed tick: render only the configured screen (or all if render_screen < 0).
    fn tick(&mut self) {
        let t0 = std::time::Instant::now();
        self.pull_audio();
        self.profile_mark("pull_audio", t0);
        // Adaptive frame rate: idle (quiet for 2s) drops to 5fps; audio resumes instantly.
        let energy_max = self.bands.iter().copied().fold(0.0f32, f32::max);
        let idle = energy_max < 0.002;
        let now = self.start.elapsed().as_secs_f32();
        self.idle_since = if idle {
            Some(self.idle_since.unwrap_or(now))
        } else {
            None
        };
        let is_idle = self.idle_since.map(|t| now - t > 2.0).unwrap_or(false);
        // Default: always render at max_fps (smooth). Only drop when explicitly opted in.
        let fps = match (self.idle_fps, is_idle) {
            (Some(ifps), true) => ifps,
            _ => self.max_fps,
        };
        self.interval = std::time::Duration::from_millis(frame_interval_ms(fps));
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
        // Every renderer has now seen this frame's texture changes; reset for the next.
        self.texture_dirty.fill(false);
        self.cover_dirty = false;
        self.profile_maybe_log();
    }

    fn pull_audio(&mut self) {
        while let Ok(b) = self.audio_rx.try_recv() {
            self.bands = b;
        }
    }

    /// Record a timing checkpoint for the profiling summary (PULSE_RING_PROFILE=1).
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

    /// Compute the full scene ONCE per tick (audio, Lua, plugins, particles, widgets).
    /// Every output consumes the same SceneFrame, so CPU work no longer scales with monitor count.
    fn compute_scene(&mut self) -> SceneFrame {
        let t_lua = std::time::Instant::now();
        let elapsed = self.start.elapsed().as_secs_f32();
        if elapsed - self.last_music_poll > 1.0 {
            self.last_music_poll = elapsed;
            self.poll_music();
        }
        // Lua hooks: let the script transform bands and tweak config each frame.
        // NOTE: transforms operate on a copy; self.bands stays the raw audio data so the
        // transforms never feed back into themselves (which caused cumulative amplification).
        let mut render_bands = self.lua_state.transform_bands(&self.bands);
        self.lua_state.frame(&mut self.cfg, &self.bands, elapsed, &self.music);
        self.profile_mark("lua", t_lua);
        // Rust plugins: per-frame update + band transform chain.
        let t_plugins = std::time::Instant::now();
        let (h, m, s, _) = main_now_hmsparts();
        let cfg_ptr = &self.cfg as *const crate::config::Config;
        for p in self.plugins.iter_mut() {
            let mut bridge = plugin::HostBridge {
                cfg: &mut self.cfg,
                bands: &self.bands,
                log_cb: |msg| log::info!("[plugin] {msg}"),
                now_hms: (h, m, s),
            };
            let ctx = bridge.make_ctx();
            p.set_ctx(ctx);
            p.bind_state(&self.bands, cfg_ptr);
            p.call_update(elapsed);
        }
        for p in &self.plugins {
            let out = p.call_transform(&render_bands);
            // Time-smooth the plugin output (strong low-pass) into the render copy.
            for i in 0..128 {
                let v = out[i];
                let s = self.plugin_smooth_bands[i];
                let sm = if v > s { s * 0.5 + v * 0.5 } else { s * 0.85 + v * 0.15 };
                self.plugin_smooth_bands[i] = sm;
                render_bands[i] = sm;
            }
        }
        self.profile_mark("plugins", t_plugins);
        let t_ptex = std::time::Instant::now();
        self.render_plugin_textures();
        self.profile_mark("plugin_tex", t_ptex);
        let spawn_scale = spawn_scale_for(&self.cfg, elapsed);
        let spawn_t = (elapsed / (self.cfg.spawn_duration.max(1.0) / 1000.0)).min(1.0);
        let spawn_effect = match self.cfg.spawn_effect {
            crate::config::SpawnEffect::None => 0u32,
            crate::config::SpawnEffect::Expand => 1u32,
            crate::config::SpawnEffect::Zoom => 2u32,
            crate::config::SpawnEffect::Magic => 3u32,
        };
        let spawn_rot = (self.cfg.spawn_rotate * (1.0 - spawn_t)).to_radians();
        let rotate_rad = (self.cfg.rotate + self.cfg.auto_rotate * elapsed).to_radians();
        let amp_avg = render_bands.iter().copied().sum::<f32>() / NBANDS as f32;
        // Time-domain low-pass: the ring band follows the music smoothly, so the particle
        // orbit swells and settles gently instead of twitching in/out.
        self.ring_amp_smooth = self.ring_amp_smooth * 0.90 + amp_avg * 0.10;
        // Particle math needs a screen size; use the first configured output.
        let (sw, sh) = self
            .outputs
            .iter()
            .find(|o| o.width > 0)
            .map(|o| (o.width, o.height))
            .unwrap_or((1920, 1080));
        let t_particles = std::time::Instant::now();
        let particles = compute_particles(&self.cfg, elapsed, sw, sh, self.ring_amp_smooth);
        self.profile_mark("particles", t_particles);
        // Widgets need &mut self (cover poll, clock raster cache); once per frame.
        let t_widgets = std::time::Instant::now();
        let widgets_cfg: Vec<crate::config::WidgetConfig> =
            self.cfg.widgets.iter().take(32).cloned().collect();
        let widgets = self.prepare_widgets(&widgets_cfg);
        self.profile_mark("widgets", t_widgets);
        let bar_energy = compute_bar_energy(&render_bands);
        let overall = compute_overall_energy(&render_bands);
        SceneFrame {
            render_bands,
            spawn_scale,
            spawn_t,
            spawn_effect,
            spawn_rot,
            rotate_rad,
            amp_avg,
            particles,
            widgets,
            bar_energy,
            overall,
            widgets_cfg,
        }
    }

    /// Render ONE output from a shared scene: upload textures to this renderer's atlas,
    /// set uniforms, draw, damage the surface and commit. Cheap per-output work only.
    fn render_output(&mut self, idx: usize, scene: &SceneFrame) {
        let t_render = std::time::Instant::now();
        let (layer, width, height, closed) = {
            let o = &mut self.outputs[idx];
            (o.layer.clone(), o.width, o.height, o.closed)
        };
        if closed || width == 0 || height == 0 {
            return;
        }
        // Local mutable copy so per-renderer atlas UVs can be patched in.
        let mut widgets = scene.widgets;
        let renderer = &mut self.outputs[idx].renderer;
        // Cover texture: upload only when it changed (multi-monitor safe — each
        // renderer owns its own atlas, so every output gets the upload once).
        if self.cover_dirty {
            if let Some(img) = &self.current_cover {
                if let Some((ux, uy, uw, uh)) = renderer.upload_texture(self.cover_tex_index, &img.rgba, img.w, img.h) {
                    log::info!("cover: uploaded slot={} uv=({:.3},{:.3},{:.3},{:.3})", self.cover_slot, ux, uy, uw, uh);
                    self.widget_uvs[self.cover_slot] = (ux, uy, uw, uh);
                    // also write into the local widgets array so this frame sees it
                    let wo = self.cover_slot * 40;
                    widgets[wo + 7] = ux;
                    widgets[wo + 8] = uy;
                    widgets[wo + 9] = uw;
                    widgets[wo + 10] = uh;
                }
            }
        }
        // Upload only the texture slots that changed this frame (multi-monitor safe):
        // each renderer owns its own atlas, so no shared queue that one monitor drains.
        for (ti, img) in self.texture_slots.iter().enumerate() {
            if !self.texture_dirty[ti] {
                continue;
            }
            if let Some(img) = img {
                if let Some((ux, uy, uw, uh)) = renderer.upload_texture(ti, &img.rgba, img.w, img.h) {
                    // find the widget slot(s) referencing this texture index
                    for s in 0..32 {
                        let wo = s * 40;
                        if (widgets[wo + 6] - ti as f32).abs() < 0.01 {
                            if ti >= 8 {
                                log::info!("plugin tex {} -> widget slot {} uv=({:.3},{:.3},{:.3},{:.3})", ti, s, ux, uy, uw, uh);
                            }
                            widgets[wo + 7] = ux;
                            widgets[wo + 8] = uy;
                            widgets[wo + 9] = uw;
                            widgets[wo + 10] = uh;
                            self.widget_uvs[s] = (ux, uy, uw, uh);
                        }
                    }
                }
            }
        }
        renderer.set_widgets(&widgets);
        let widget_bounds = compute_widget_bounds(&scene.widgets_cfg, width, height);
        renderer.set_widget_bounds(&widget_bounds);
        renderer.resize(width, height);
        renderer.set_auto_rotate(scene.rotate_rad);
        renderer.set_bar_energy(&scene.bar_energy);
        renderer.set_overall_energy(scene.overall);
        let pcount = self.cfg.particles.len().min(32) as u32;
        renderer.set_particle_count(pcount);
        // Particle band centre (px): ring base + half growth + halo + typical offset.
        let band_r = (self.cfg.base_radius + self.cfg.growth * 0.5 + self.cfg.halo_size * 0.5
            + self.cfg.particles.first().map(|p| p.x).unwrap_or(0.012)) * (width.min(height) as f32);
        renderer.set_particle_band(band_r);
        renderer.set_render_scale(self.cfg.render_scale);
        renderer.render(
            &scene.render_bands,
            scene.spawn_scale,
            scene.spawn_effect,
            scene.spawn_t,
            scene.spawn_rot,
            &scene.particles,
            self.start.elapsed().as_secs_f32(),
        );
        self.profile_mark("render", t_render);

        let surface = layer.wl_surface();
        // Damage only the region where the rings/widgets actually live (centre band +
        // widget zones) instead of the full frame — niri only recomposites damaged
        // regions, so a full-screen damage makes the whole desktop re-composite every frame.
        let dw = width as i32;
        let dh = height as i32;
        // rings occupy the central ~46% height; widgets live near edges — be generous but
        // still far smaller than the full frame.
        let rx0 = (dw / 2 - dw * 4 / 10).max(0);
        let rx1 = (dw / 2 + dw * 4 / 10).min(dw);
        let ry0 = (dh / 2 - dh * 4 / 10).max(0);
        let ry1 = (dh / 2 + dh * 4 / 10).min(dh);
        surface.damage_buffer(rx0, ry0, rx1 - rx0, ry1 - ry0);
        // widgets near the edges
        for s in 0..32 {
            let wo = s * 40;
            let wtype = widgets[wo];
            if wtype > 0.5 && widgets[wo + 4] > 0.004 {
                let wx = (widgets[wo + 1] * width as f32) as i32;
                let wy = (widgets[wo + 2] * height as f32) as i32;
                let ws = (widgets[wo + 3] * (width.min(height)) as f32) as i32;
                surface.damage_buffer((wx - ws).max(0), (wy - ws).max(0), ws * 2, ws * 2);
            }
        }
        layer.commit();
    }
}
#[cfg(test)]
mod tests {
    use crate::config::{Config, parse_for_test};

    #[test]
    fn parse_widgets_works() {
        let qml = r##"
PulseRing {
    widgets: [
        Widget { type: "clock"; x: 0.5; y: 0.22; fontSize: 56; color: "#EADDFF"; alpha: 0.9 }
    ]
}
"##;
        let cfg = parse_for_test(qml);
        println!("widgets.len = {}", cfg.widgets.len());
        for w in &cfg.widgets {
            println!("widget: {:?} x={} y={} size={} alpha={}", w.widget_type, w.x, w.y, w.size, w.alpha);
        }
    }

    #[test]
    fn parse_lyric_widget_works() {
        use crate::config::WidgetType;
        let qml = r##"
PulseRing {
    widgets: [
        Widget { type: "lyric"; x: 0.5; y: 0.82; size: 0.7; fontSize: 44; showPrevNext: false; colors: ["#EADDFF", "#FFD740"] }
    ]
}
"##;
        let cfg = parse_for_test(qml);
        assert_eq!(cfg.widgets.len(), 1);
        let w = &cfg.widgets[0];
        assert_eq!(w.widget_type, WidgetType::Lyric);
        assert_eq!(w.font_size, 44.0);
        assert!(!w.show_prev_next);
        assert_eq!(w.colors.len(), 2);
    }

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

    #[test]
    fn bar_energy_and_overall_are_correct() {
        let mut bands = [0.0f32; super::NBANDS];
        bands[40] = 1.0; // a mid band (inside 16..96)
        let be = super::compute_bar_energy(&bands);
        // bin 20 covers bands 40..42 -> mean 0.5
        assert!((be[20] - 0.5).abs() < 1e-6, "be[20]={}", be[20]);
        assert_eq!(be[0], 0.0);
        let ov = super::compute_overall_energy(&bands);
        // 1.0 / 80 over mid bands 16..96
        assert!((ov - 1.0 / 80.0).abs() < 1e-6, "ov={ov}");
    }

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

    #[test]
    fn frame_interval_adapts_to_energy() {
        use super::frame_interval_ms;
        assert_eq!(frame_interval_ms(30), 33); // 30fps
        assert_eq!(frame_interval_ms(60), 16); // 60fps
        assert_eq!(frame_interval_ms(20), 50); // 20fps
    }

    #[test]
    fn lyric_raster_has_karaoke_clip() {
        use crate::{load_font, rasterize_lyric_image, LyricStyle};
        let font = load_font();
        let st = LyricStyle {
            font_size: 40.0,
            base: [0.85, 0.9, 1.0, 1.0],
            sung: [1.0, 0.78, 0.35, 1.0],
            cur: [1.0, 1.0, 1.0, 1.0],
            glow: [0.7, 0.53, 1.0, 0.75],
            show_prev_next: false,
        };
        let img = rasterize_lyric_image(&font, None, "hello world", None, &[], 0, 0.5, &st, 1.0, 0.0)
            .expect("rasterize");
        assert!(img.w > 10 && img.h > 4);
        // Scan the row at the text's vertical centre: the lit (sung) half should show
        // the golden gradient, the unlit half the dimmed base colour.
        let mid = img.h / 2;
        let mut found_dim = false;
        let mut found_karaoke = false;
        for x in 0..img.w {
            let o = ((mid * img.w + x) * 4) as usize;
            let (r, g, b, a) = (img.rgba[o], img.rgba[o + 1], img.rgba[o + 2], img.rgba[o + 3]);
            if a > 40 {
                // karaoke gradient: gold-ish (r high, mid g, low b)
                if r > 190 && g > 120 && g < 235 && b < 130 {
                    found_karaoke = true;
                }
                // unlit dim base: muted lavender (dimmer than the lit part)
                if r > 110 && r < 190 && b > 140 && g > 110 {
                    found_dim = true;
                }
            }
        }
        assert!(found_dim, "no dim unlit pixels");
        assert!(found_karaoke, "no lit karaoke pixels (clip missing)");
    }

    #[test]
    fn lyric_raster_words_have_three_states() {
        use crate::{load_font, rasterize_lyric_image, LyricStyle};
        let font = load_font();
        let st = LyricStyle {
            font_size: 40.0,
            base: [0.5, 0.5, 0.6, 1.0],   // unsung: dim blue-grey
            sung: [1.0, 0.78, 0.35, 1.0], // sung: orange
            cur: [0.9, 0.1, 0.1, 1.0],    // current: red
            glow: [0.0, 0.0, 0.0, 0.0],
            show_prev_next: false,
        };
        let words = vec![
            (0.0, 1.0, "AAA".to_string()),
            (1.0, 2.0, "BBB".to_string()),
            (2.0, 3.0, "CCC".to_string()),
        ];
        let img = rasterize_lyric_image(&font, None, "AAABBBCCC", None, &words, 1, 0.0, &st, 1.0, 0.0)
            .expect("rasterize");
        let mid = img.h / 2;
        let mut sung = 0;
        let mut cur = 0;
        let mut unsung = 0;
        for x in 0..img.w {
            let o = ((mid * img.w + x) * 4) as usize;
            let (r, g, b, a) = (img.rgba[o], img.rgba[o + 1], img.rgba[o + 2], img.rgba[o + 3]);
            if a > 60 {
                if r > 200 && g < 140 && b < 140 {
                    cur += 1; // red-ish
                } else if r > 200 && g > 140 && g < 230 && b < 120 {
                    sung += 1; // orange
                } else if r < 160 {
                    unsung += 1; // dim blue-grey
                }
            }
        }
        assert!(cur > 0, "current word (red) missing");
        assert!(sung > 0, "sung word (orange) missing");
        assert!(unsung > 0, "unsung word (dim) missing");
    }

}

