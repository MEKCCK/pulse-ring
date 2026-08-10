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
}

fn main() {
    env_logger::init();

    let cfg = config::Config::load(&config::config_path());
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

impl App {
    fn pull_audio(&mut self) {
        while let Ok(b) = self.audio_rx.try_recv() {
            self.bands = b;
        }
    }

    fn draw_one(&mut self, qh: &QueueHandle<Self>, idx: usize) {
        self.pull_audio();

        let (layer, renderer, width, height, closed) = {
            let o = &mut self.outputs[idx];
            (
                o.layer.clone(),
                &mut o.renderer,
                o.width,
                o.height,
                o.closed,
            )
        };
        if closed || width == 0 || height == 0 {
            log::info!("draw_one({idx}) SKIPPED: closed={closed} size={width}x{height}");
            return;
        }

        let elapsed = self.start.elapsed().as_secs_f32();
        let spawn_scale = spawn_scale_for(&self.cfg, elapsed);
        let rotate_rad = (self.cfg.rotate + self.cfg.auto_rotate * elapsed).to_radians();
        let amp_avg = self.bands.iter().copied().sum::<f32>() / NBANDS as f32;
        let particles = compute_particles(&self.cfg, elapsed, width, height, amp_avg);
        if self.start.elapsed().as_secs_f32() < 1.0 {
            log::info!("cfg.particles len={} mode={:?}", self.cfg.particles.len(), self.cfg.particle_mode);
        }
        if particles[2] > 0.0 && self.start.elapsed().as_secs_f32() < 2.0 {
            log::info!("particle[0] = pos({}, {}) size {} alpha {} color({},{},{},{}) mode={:?}",
                particles[0], particles[1], particles[2], particles[3],
                particles[4], particles[5], particles[6], particles[7],
                self.cfg.particle_mode);
        }

        renderer.resize(width, height);
        renderer.set_auto_rotate(rotate_rad);
        renderer.render(&self.bands, spawn_scale, &particles, elapsed);

        let surface = layer.wl_surface();
        log::info!("draw_one({idx}) rendered {width}x{height}");
        // wgpu's present attaches a new buffer but may not mark it damaged; niri only
        // recomposites damaged regions, so a missing damage freezes the surface on frame 1.
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        layer.commit();
    }
}