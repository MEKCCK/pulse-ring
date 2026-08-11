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
mod plugin;
use audio::NBANDS;
use draw::RingRenderer;

const MAX_PARTICLES: usize = 96;
const PARTICLE_STRIDE: usize = 12;

/// One full rendering instance per output (layer surface + wgpu surface + renderer).
struct OutputSurfaces {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    renderer: RingRenderer,
    width: u32,
    height: u32,
    configured: bool,
    closed: bool,
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
    };

    loop {
        event_queue.blocking_dispatch(&mut app).unwrap();
        // Exit when every per-output surface has been closed.
        if !app.outputs.is_empty() && app.outputs.iter().all(|o| o.closed) {
            break;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _f: i32) {}
    fn transform_changed(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _t: wl_output::Transform) {}
    fn surface_enter(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _o: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _c: &Connection, _qh: &QueueHandle<Self>, _s: &wl_surface::WlSurface, _o: &wl_output::WlOutput) {}

    fn frame(&mut self, _c: &Connection, qh: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _t: u32) {
        // Find the output whose layer surface requested this frame callback.
        let idx = self
            .outputs
            .iter()
            .position(|o| o.layer.wl_surface() == surface);
        log::info!("frame callback for {:?} -> idx {idx:?}", surface.id());
        if let Some(idx) = idx {
            self.draw_one(qh, idx);
        }
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
            if first {
                self.draw_one(qh, idx);
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

/// Scale an image down (bilinear-ish) to fit a 256x256 atlas slot, keeping aspect.
fn fit_slot(img: ImageData) -> ImageData {
    const MAX: u32 = 512;
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

impl App {
    /// Compute widget uniform data (12 f32 each). Returns the 96-float layout.
    fn prepare_widgets(&mut self, width: u32, height: u32) -> [f32; 1280] {
        use crate::config::WidgetType;
        let mut data = [0.0f32; 1280];
        let mut tex_index = 0u32;
        // Reserve slot 3 for the album cover (clocks/images use 0..2).
        self.cover_tex_index = 3;
        let widgets: Vec<crate::config::WidgetConfig> = self.cfg.widgets.iter().take(32).cloned().collect();
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
            }
        }
        for (si, w) in widgets.iter().enumerate() {
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
        let out = std::process::Command::new("playerctl")
            .args(["metadata", "xesam:title"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(t) = out {
            if self.music.title != t {
                self.music.title = t;
            }
        }
        let out = std::process::Command::new("playerctl")
            .args(["metadata", "xesam:artist"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(a) = out {
            self.music.artist = a;
        }
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
        for (i, p) in self.plugins.iter().enumerate() {
            let slot = (8 + i) as u32;
            // allocate a 512x512 buffer per plugin (host-owned)
            let mut buf = vec![0u8; 512 * 512 * 4];
            let mut req = plugin::RenderRequest {
                slot,
                buf_len: buf.len(),
                buf: buf.as_mut_ptr(),
                update: false,
                width: 0,
                height: 0,
                screen_w,
                screen_h,
            };
            p.bind_state(&self.bands, &self.cfg as *const crate::config::Config);
            p.call_render(&mut req);
            if req.update && req.width > 0 && req.height > 0 {
                let w = req.width.min(512);
                let h = req.height.min(512);
                // Plugin writes a w×h image at the start of the buffer with row stride = w.
                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h {
                    for x in 0..w {
                        let si = ((y * w + x) * 4) as usize;
                        rgba.extend_from_slice(&buf[si..si + 4]);
                    }
                }
                self.plugin_tex[i] = Some((w, h, rgba));
            }
        }
        // write plugin textures into texture_slots (so prepare_widgets picks them up)
        for (i, tex) in self.plugin_tex.iter().enumerate() {
            if let Some((w, h, rgba)) = tex {
                let ti = 8 + i;
                let img = ImageData { w: *w, h: *h, rgba: rgba.clone() };
                self.texture_slots[ti] = Some(img);
            }
        }
    }

    fn pull_audio(&mut self) {
        while let Ok(b) = self.audio_rx.try_recv() {
            self.bands = b;
        }
    }

    fn draw_one(&mut self, qh: &QueueHandle<Self>, idx: usize) {
        self.pull_audio();

        let (layer, width, height, closed) = {
            let o = &mut self.outputs[idx];
            (o.layer.clone(), o.width, o.height, o.closed)
        };
        if closed || width == 0 || height == 0 {
            log::info!("draw_one({idx}) SKIPPED: closed={closed} size={width}x{height}");
            return;
        }

        let elapsed = self.start.elapsed().as_secs_f32();
        if elapsed - self.last_music_poll > 2.0 {
            self.last_music_poll = elapsed;
            self.poll_music();
        }
        // Lua hooks: let the script transform bands and tweak config each frame.
        // NOTE: transforms operate on a copy; self.bands stays the raw audio data so the
        // transforms never feed back into themselves (which caused cumulative amplification).
        let mut render_bands = self.lua_state.transform_bands(&self.bands);
        self.lua_state.frame(&mut self.cfg, &self.bands, elapsed, &self.music);
        // Rust plugins: per-frame update + band transform chain.
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
        self.render_plugin_textures();
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
        let particles = compute_particles(&self.cfg, elapsed, width, height, self.ring_amp_smooth);

        // Widgets need &mut self; do it before borrowing the renderer.
        let mut widgets = self.prepare_widgets(width, height);
        let renderer = &mut self.outputs[idx].renderer;
        // Cover texture: upload to every renderer independently (multi-monitor safe).
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
        // Upload every texture slot to THIS renderer every frame (multi-monitor safe):
        // each renderer owns its own atlas, so no shared queue that one monitor drains.
        for (ti, img) in self.texture_slots.iter().enumerate() {
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
        renderer.resize(width, height);
        renderer.set_auto_rotate(rotate_rad);
        renderer.render(&render_bands, spawn_scale, spawn_effect, spawn_t, spawn_rot, &particles, elapsed);

        let surface = layer.wl_surface();
        log::info!("draw_one({idx}) rendered {width}x{height}");
        // wgpu's present attaches a new buffer but may not mark it damaged; niri only
        // recomposites damaged regions, so a missing damage freezes the surface on frame 1.
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(qh, FrameCallbackData(surface.clone()));
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
}
