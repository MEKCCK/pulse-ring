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
mod transitions;
mod video_wallpaper;
mod wallpaper_pack;
mod web_wallpaper;
use audio::NBANDS;
use draw::RingRenderer;

const MAX_PARTICLES: usize = 96;
const PARTICLE_STRIDE: usize = 12;
const GENERAL_TEXTURE_SLOTS: [usize; 7] = [0, 1, 2, 4, 5, 6, 7];
const COVER_TEXTURE_SLOT: usize = 3;
const PLUGIN_TEXTURE_START: usize = 8;
const MAX_PLUGIN_TEXTURES: usize = draw::ATLAS_CAPACITY - PLUGIN_TEXTURE_START;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TextureKind {
    #[default]
    General,
    Lyric,
    Cover,
    Plugin,
}

#[derive(Default)]
struct TextureSlotState {
    key: String,
    image: Option<std::sync::Arc<ImageData>>,
    revision: u64,
    kind: TextureKind,
}

impl TextureSlotState {
    fn update(
        &mut self,
        key: String,
        image: std::sync::Arc<ImageData>,
        kind: TextureKind,
    ) -> bool {
        if self.image.is_some() && self.key == key {
            return false;
        }
        self.key = key;
        self.image = Some(image);
        self.kind = kind;
        self.revision = self.revision.wrapping_add(1).max(1);
        true
    }
}

fn texture_needs_upload(slot_revision: u64, uploaded_revision: u64) -> bool {
    slot_revision != 0 && slot_revision != uploaded_revision
}

fn lyric_request_needed(
    cached_signature: Option<&str>,
    pending_signature: Option<&str>,
    desired_signature: &str,
) -> bool {
    cached_signature != Some(desired_signature) && pending_signature != Some(desired_signature)
}

fn lyric_result_is_current(
    pending: Option<&(u64, String)>,
    generation: u64,
    result_signature: &str,
    desired_signature: &str,
) -> bool {
    pending.is_some_and(|(pending_generation, pending_signature)| {
        *pending_generation == generation
            && pending_signature == result_signature
            && result_signature == desired_signature
    })
}

fn texture_slot_layout(
    widgets: &[crate::config::WidgetConfig],
) -> ([Option<usize>; 32], usize) {
    use crate::config::WidgetType;
    let mut layout = [None; 32];
    let mut next = 0usize;
    let mut overflow = 0usize;
    for (widget_slot, widget) in widgets.iter().take(32).enumerate() {
        if !matches!(widget.widget_type, WidgetType::Image | WidgetType::Clock | WidgetType::Lyric) {
            continue;
        }
        if let Some(&texture_slot) = GENERAL_TEXTURE_SLOTS.get(next) {
            layout[widget_slot] = Some(texture_slot);
            next += 1;
        } else {
            overflow += 1;
        }
    }
    (layout, overflow)
}

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
    pub max_frame: f32,
    pub lyric_requests: u64,
    pub lyric_deduped: u64,
    pub lyric_stale_results: u64,
    pub lyric_uploads: u64,
}

impl ProfileStats {
    pub fn format_line(s: &Self, frames: u32) -> String {
        let frames = frames.max(1) as f32;
        let total = s.pull_audio + s.lua + s.plugins + s.plugin_tex + s.particles + s.widgets + s.render;
        format!(
            "[profile avg/{frames:.0}] pull_audio={:.2}ms lua={:.2}ms plugins={:.2}ms plugin_tex={:.2}ms particles={:.2}ms widgets={:.2}ms render={:.2}ms total={:.2}ms max_frame={:.2}ms lyric{{requests={},deduped={},stale={},uploads={}}}",
            s.pull_audio * 1000.0 / frames,
            s.lua * 1000.0 / frames,
            s.plugins * 1000.0 / frames,
            s.plugin_tex * 1000.0 / frames,
            s.particles * 1000.0 / frames,
            s.widgets * 1000.0 / frames,
            s.render * 1000.0 / frames,
            total * 1000.0 / frames,
            s.max_frame * 1000.0,
            s.lyric_requests,
            s.lyric_deduped,
            s.lyric_stale_results,
            s.lyric_uploads,
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
    band_energy: [f32; 4],
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
    uploaded_texture_revisions: [u64; draw::ATLAS_CAPACITY],
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
    texture_slots: Vec<TextureSlotState>,
    cover_rx: std::sync::mpsc::Receiver<ImageData>,
    last_cover_path: String,
    cover_loaded: bool,
    cover_aspect: f32,
    /// Full-screen image wallpaper (behind everything). None = transparent.
    wallpaper_image: Option<ImageData>,
    /// Set when the wallpaper changed and every renderer's texture needs an upload.
    wallpaper_dirty: bool,
    /// Video wallpaper player (None when the current wallpaper is an image).
    video_player: Option<video_wallpaper::VideoPlayer>,
    /// True on the first frame of a new video session — it should trigger a
    /// crossfade transition (upload_wallpaper promotes the old frame to prev).
    video_first_frame: bool,
    /// Web wallpaper player (Electron offscreen HTML renderer). Frames flow like video.
    web_player: Option<web_wallpaper::WebWallpaperPlayer>,
    web_first_frame: bool,
    /// Persistent scene wallpaper player (never rotated).
    scene_player: Option<web_wallpaper::WebWallpaperPlayer>,
    scene_first_frame: bool,
    /// Force a full wallpaper upload (promote + mipmap) once, for the first video frame.
    wallpaper_force_upload: bool,
    /// Only log the first wallpaper upload (video re-uploads every frame).
    log_once_wallpaper: bool,
    /// Rotation state: wallpaper list, current index, switch/transition timers.
    wallpaper_list: Vec<String>,
    wallpaper_idx: usize,
    wallpaper_switch_at: f32,
    wallpaper_transition_start: f32,
    wallpaper_progress: f32,
    texture_overflow_warned: bool,
    plugin_overflow_warned: bool,
    lua_state: lua::LuaState,
    plugins: Vec<plugin::LoadedPlugin>,
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
    lyric_cache: Vec<Option<(String, std::sync::Arc<ImageData>)>>,
    lyric_raster_tx: std::sync::mpsc::Sender<LyricRasterReq>,
    lyric_raster_rx: std::sync::mpsc::Receiver<LyricRasterResult>,
    lyric_raster_seq: u64,
    /// Latest in-flight request per widget slot: (generation, signature).
    lyric_raster_pending: Vec<Option<(u64, String)>>,
    /// Completed worker results awaiting comparison with this frame's desired signature.
    lyric_raster_ready: Vec<Option<LyricRasterResult>>,
    /// Line-change transition state.
    lyric_cur_idx: i32,
    lyric_line_changed_at: f32,
    /// Last lyric clock value (monotonic clamp against MPRIS poll snap-backs).
    lyric_t_prev: f32,
    /// Background MPRIS state (title/artist/position/status).
    music_rx: std::sync::mpsc::Receiver<MusicSnapshot>,
}

fn main() {
    env_logger::init();

    // `pulse-ring --install-wallpaper <folder>`：把壁纸打包成文件夹安装到壁纸库
    // (~/.config/pulse-ring/wallpapers/<name>/)，之后配置里按名字引用即可。
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--install-wallpaper") {
        if let Some(src) = args.get(pos + 1) {
            match install_wallpaper_pack(src) {
                Ok(name) => {
                    println!("installed wallpaper pack '{name}'");
                    println!("use it via: wallpapers: [\"{name}\"]  or  sceneWallpaper: \"{name}\"");
                    return;
                }
                Err(e) => {
                    eprintln!("install failed: {e}");
                    std::process::exit(1);
                }
            }
        } else {
            eprintln!("usage: pulse-ring --install-wallpaper <folder-with-project.json>");
            std::process::exit(1);
        }
    }

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
    let music_rx = spawn_music_thread();
    let wallpaper_list = cfg.wallpapers.clone();
    // Image wallpaper: load once at startup (None = transparent / compositor wallpaper).
    // 配置了场景壁纸时，初始不预加载轮换图（场景首帧到达前保持透明，避免闪一下静态图）。
    let wallpaper_image = if cfg.scene_wallpaper.is_some() {
        None
    } else if !wallpaper_list.is_empty() {
        let rwp = resolve_wallpaper(&cfg.wallpapers[0]);
        (rwp.kind != "web" && rwp.kind != "video").then(|| rwp.file).and_then(|f| load_image_raw(&f))
    } else {
        cfg.image_wallpaper.as_deref().map(resolve_wallpaper).and_then(|rwp| load_image_raw(&rwp.file))
    };
    let wallpaper_dirty = wallpaper_image.is_some();
    // A standalone videoWallpaper (no rotation list) starts the video immediately.
    // In a rotation list, entries are started by tick_wallpaper_rotation instead.
    let mut video_player = None;
    if cfg.wallpapers.is_empty() {
        let video_audio = cfg.video_wallpaper_audio;
        if let Some(vpath) = &cfg.video_wallpaper {
            if video_wallpaper::is_video_path(vpath) {
                log::info!("video wallpaper: starting {vpath} (audio={video_audio})");
                match video_wallpaper::start_video_wallpaper(vpath, video_audio) {
                    Ok(p) => video_player = Some(p),
                    Err(e) => log::warn!("video wallpaper failed ({e})"),
                }
            }
        }
    }
    let video_first_frame = video_player.is_some();
    // SCENE wallpaper: a living environment, persistent (never rotated away).
    let mut scene_player = None;
    let mut scene_first_frame = false;
    if let Some(scene) = &cfg.scene_wallpaper {
        let rwp = resolve_wallpaper(scene);
        if rwp.kind == "web" {
            let (w, h) = cfg.web_wallpaper_size;
            log::info!("scene wallpaper: starting {}", rwp.file);
            match web_wallpaper::start_web_wallpaper(&rwp.file, w, h) {
                Ok(mut p) => {
                    p.send_config(&rwp.params);
                    scene_first_frame = true;
                    scene_player = Some(p);
                }
                Err(e) => log::warn!("scene wallpaper failed ({e})"),
            }
        }
    }
    // Web wallpaper (HTML) — standalone, takes precedence over image but co-exists
    // with the rotation list handling (entries starting with .html).
    let mut web_player = None;
    let mut web_first_frame = false;
    if cfg.wallpapers.is_empty() {
        if let Some(html) = &cfg.web_wallpaper {
            let rwp = resolve_wallpaper(html);
            if rwp.kind == "web" {
                let (w, h) = cfg.web_wallpaper_size;
                log::info!("web wallpaper: starting {}", rwp.file);
                match web_wallpaper::start_web_wallpaper(&rwp.file, w, h) {
                    Ok(mut p) => {
                        p.send_config(&rwp.params);
                        web_first_frame = true;
                        web_player = Some(p);
                    }
                    Err(e) => log::warn!("web wallpaper failed ({e})"),
                }
            }
        }
    }
    let wallpaper_dirty = wallpaper_dirty || video_player.is_some() || web_player.is_some();
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
        texture_slots: (0..draw::ATLAS_CAPACITY)
            .map(|_| TextureSlotState::default())
            .collect(),
        cover_rx: spawn_cover_thread(),
        last_cover_path: String::new(),
        cover_loaded: false,
        cover_aspect: 1.0,
        wallpaper_image,
        wallpaper_dirty,
        video_player,
        video_first_frame,
        web_player,
        web_first_frame,
        scene_player,
        scene_first_frame,
        wallpaper_force_upload: false,
        log_once_wallpaper: true,
        wallpaper_list,
        wallpaper_idx: 0,
        wallpaper_switch_at: 0.0,
        wallpaper_transition_start: 0.0,
        wallpaper_progress: 1.0,
        texture_overflow_warned: false,
        plugin_overflow_warned: false,
        lua_state,
        plugins: plugin::load_plugins_with_log(),
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
        lyric_raster_pending: vec![None; 32],
        lyric_raster_ready: (0..32).map(|_| None).collect(),
        lyric_cur_idx: -1,
        lyric_line_changed_at: 0.0,
        lyric_t_prev: -1000.0,
        music_rx,
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
            uploaded_texture_revisions: [0; draw::ATLAS_CAPACITY],
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
            let is_target = self.wallpaper_image.is_some()
                || self.cfg.render_screen < 0
                || self.cfg.render_screen == idx as i32;
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

/// Precompute the averages used by widget band modes so fragment shaders never
/// sum dozens of spectrum bins for every covered pixel.
fn compute_band_energy(bands: &[f32; NBANDS]) -> [f32; 4] {
    fn mean(values: &[f32]) -> f32 {
        values.iter().copied().sum::<f32>() / values.len().max(1) as f32
    }

    [
        mean(&bands[0..32]),
        mean(&bands[32..96]),
        mean(&bands[96..128]),
        mean(bands),
    ]
}

/// Per-widget conservative half-extent in pixels, used by the shader to skip
/// pixels outside each widget's square before running type-specific math.
fn compute_widget_bounds(widgets: &[crate::config::WidgetConfig], width: u32, height: u32) -> [f32; 32] {
    use crate::config::WidgetType;
    let mut out = [0.0f32; 32];
    let min_d = width.min(height) as f32;
    for (i, w) in widgets.iter().take(32).enumerate() {
        let b = match w.widget_type {
            WidgetType::Ring => (w.base_radius + w.growth + w.halo_size + 0.05) * w.size * min_d,
            WidgetType::Bars => {
                let half_h = if w.bar_mirror { w.bar_height * 0.5 } else { w.bar_height };
                (w.size * 0.5).max(half_h) * min_d + 2.0
            }
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

/// Snapshot of MPRIS state polled on a background thread.
struct MusicSnapshot {
    title: String,
    artist: String,
    position_sec: f32,
    /// Track duration in seconds (mpris:length), used to validate lyric matches.
    duration_sec: f32,
    playing: bool,
}

/// Poll MPRIS (title/artist/position/status) on a background thread once per second.
/// Spawning subprocesses on the main thread stalled the render loop for tens of
/// milliseconds every second — the cause of the intermittent all-screen stutter.
fn spawn_music_thread() -> std::sync::mpsc::Receiver<MusicSnapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let run = |args: &[&str]| -> Option<String> {
                std::process::Command::new("playerctl")
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            let snap = MusicSnapshot {
                title: run(&["metadata", "xesam:title"]).unwrap_or_default(),
                artist: run(&["metadata", "xesam:artist"]).unwrap_or_default(),
                // `playerctl position` prints seconds as a float ("5.834005"); some
                // builds print raw microseconds — handle both.
                position_sec: run(&["position"])
                    .and_then(|s| {
                        let v: f64 = s.trim().parse().ok()?;
                        Some(if v.abs() > 100_000.0 { v / 1_000_000.0 } else { v })
                    })
                    .unwrap_or(0.0) as f32,
                // mpris:length is in microseconds.
                duration_sec: run(&["metadata", "mpris:length"])
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|v| (v / 1_000_000.0) as f32)
                    .unwrap_or(0.0),
                playing: run(&["status"]).as_deref() == Some("Playing"),
            };
            if tx.send(snap).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    });
    rx
}

/// Background lyric fetcher. The App sends a track key (`title\u{1}artist\u{1}duration_sec`)
/// when the track changes; the thread resolves local -> cache -> QQ -> Lrclib and
/// replies with the parsed lyric data tagged with the same key.
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
            let mut parts = key.split('\u{1}');
            let title = parts.next().unwrap_or("");
            let artist = parts.next().unwrap_or("");
            let duration_hint = parts.next().and_then(|d| d.parse::<f32>().ok()).filter(|d| *d > 0.0);
            let data = lyrics::fetch_lyrics(
                title,
                artist,
                &cfg_dir.to_string_lossy(),
                &cache_dir.to_string_lossy(),
                duration_hint,
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


/// A versioned lyric banner rasterisation request. The worker coalesces queued
/// requests per widget slot so rapid seeks cannot build an unbounded render backlog.
struct LyricRasterReq {
    generation: u64,
    slot: usize,
    signature: String,
    font: std::sync::Arc<rusttype::Font<'static>>,
    prev: Option<String>,
    current: String,
    next: Option<String>,
    style: LyricStyle,
}

struct LyricRasterResult {
    generation: u64,
    slot: usize,
    signature: String,
    image: ImageData,
}

/// Spawn the lyric banner rasteriser worker. Returns (request sender, result receiver).
fn spawn_lyric_raster_thread() -> (
    std::sync::mpsc::Sender<LyricRasterReq>,
    std::sync::mpsc::Receiver<LyricRasterResult>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<LyricRasterReq>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<LyricRasterResult>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut latest: [Option<LyricRasterReq>; 32] = std::array::from_fn(|_| None);
            if first.slot < latest.len() {
                let first_slot = first.slot;
                latest[first_slot] = Some(first);
            }
            while let Ok(req) = rx.try_recv() {
                if req.slot < latest.len() {
                    let slot = req.slot;
                    latest[slot] = Some(req);
                }
            }
            for req in latest.into_iter().flatten() {
                let img = rasterize_lyric_image(
                    &req.font,
                    req.prev.as_deref(),
                    &req.current,
                    req.next.as_deref(),
                    &req.style,
                )
                .map(fit_slot);
                if let Some(image) = img {
                    let result = LyricRasterResult {
                        generation: req.generation,
                        slot: req.slot,
                        signature: req.signature,
                        image,
                    };
                    if res_tx.send(result).is_err() {
                        return;
                    }
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
/// Decode an image file to RGBA keeping the original aspect (no crop / no resize).
fn load_image_raw(path: &str) -> Option<ImageData> {
    let expanded = path.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
    let bytes = std::fs::read(&expanded).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    Some(ImageData { w, h, rgba: rgba.into_raw() })
}

/// Cover art loader: decode then centre-crop to a square (album art is often
/// rectangular; the cover widget shows a SQUARE, so crop here instead of stretching).
fn load_image_path(path: &str) -> Option<ImageData> {
    let img = load_image_raw(path)?;
    let (w, h) = (img.w, img.h);
    let side = w.min(h);
    let rgba = if side < w || side < h {
        let x0 = (w - side) / 2;
        let y0 = (h - side) / 2;
        image::imageops::crop_imm(&image::RgbaImage::from_raw(w, h, img.rgba)?, x0, y0, side, side).to_image()
    } else {
        image::RgbaImage::from_raw(w, h, img.rgba)?
    };
    Some(fit_slot(ImageData { w: side, h: side, rgba: rgba.into_raw() }))
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
    const MAX: u32 = draw::ATLAS_SLOT_SIZE;
    if img.w <= MAX && img.h <= MAX {
        return img;
    }
    let scale = (MAX as f32 / img.w as f32).min(MAX as f32 / img.h as f32);
    let nw = ((img.w as f32 * scale).floor() as u32).max(1);
    let nh = ((img.h as f32 * scale).floor() as u32).max(1);
    let mut out = ImageData { w: nw, h: nh, rgba: vec![0u8; (nw * nh * 4) as usize] };
    // Bilinear sampling: smooth edges instead of blocky point-sampling when a
    // banner is scaled down to fit the atlas slot.
    for y in 0..nh {
        for x in 0..nw {
            let fx = ((x as f32 + 0.5) / scale - 0.5).clamp(0.0, img.w as f32 - 1.0001);
            let fy = ((y as f32 + 0.5) / scale - 0.5).clamp(0.0, img.h as f32 - 1.0001);
            let x0 = fx.floor() as usize;
            let y0 = fy.floor() as usize;
            let x1 = (x0 + 1).min(img.w as usize - 1);
            let y1 = (y0 + 1).min(img.h as usize - 1);
            let tx = fx - x0 as f32;
            let ty = fy - y0 as f32;
            let i00 = (y0 * img.w as usize + x0) * 4;
            let i01 = (y0 * img.w as usize + x1) * 4;
            let i10 = (y1 * img.w as usize + x0) * 4;
            let i11 = (y1 * img.w as usize + x1) * 4;
            let di = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                let top = img.rgba[i00 + c] as f32 * (1.0 - tx) + img.rgba[i01 + c] as f32 * tx;
                let bot = img.rgba[i10 + c] as f32 * (1.0 - tx) + img.rgba[i11 + c] as f32 * tx;
                out.rgba[di + c] = (top * (1.0 - ty) + bot * ty).round() as u8;
            }
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
                        img.rgba[o] = (cr * 255.0) as u8;
                        img.rgba[o + 1] = (cg * 255.0) as u8;
                        img.rgba[o + 2] = (cb * 255.0) as u8;
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
#[derive(Clone, Copy)]
struct LyricStyle {
    font_size: f32,
    /// Unsung words + prev/next base colour.
    base: [f32; 4],
    /// Already-sung words.
    sung: [f32; 4],
    /// Current word highlight.
    cur: [f32; 4],
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
                    // NON-premultiplied storage: RGB = the display colour, alpha separate.
                    // The shader does `tc.rgb * tc.a`, so premultiplying here (rgb*a)
                    // would multiply the edge alpha twice and turn glyph edges black.
                    img.rgba[o] = (color[0] * 255.0) as u8;
                    img.rgba[o + 1] = (color[1] * 255.0) as u8;
                    img.rgba[o + 2] = (color[2] * 255.0) as u8;
                    img.rgba[o + 3] = (a * 255.0) as u8;
                }
            });
        }
    }
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
                    img.rgba[o] = (c[0] * 255.0) as u8;
                    img.rgba[o + 1] = (c[1] * 255.0) as u8;
                    img.rgba[o + 2] = (c[2] * 255.0) as u8;
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
    st: &LyricStyle,
) -> Option<ImageData> {
    // Line-level highlighting: the CURRENT line is baked fully lit (gold->white
    // gradient), prev/next are dimmed. No per-line or per-word progress — the line
    // simply lights up while it is being sung. Rasterised ONCE per line.
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
    let cur_w = line_w(current, cur_scale);
    let w = cur_w
        .max(if show_prev { line_w(prev_line, sub_scale) } else { 0 })
        .max(if show_next { line_w(next_line, sub_scale) } else { 0 })
        .max(8);
    let h = (cur_h
        + if sub_lines > 0 {
            sub_h * sub_lines as u32 + gap * 2
        } else {
            0
        })
    .max(4)
        + 2;
    let mut img = ImageData {
        w,
        h,
        rgba: vec![0u8; (w * h * 4) as usize],
    };
    let center_x = |text: &str, sc: rusttype::Scale| -> f32 {
        ((w as i64 - line_w(text, sc) as i64) / 2).max(0) as f32
    };
    // Vertical layout: [prev] [gap] [current] [gap] [next], current centered.
    let cur_top = if show_prev { sub_h + gap } else { 0 };
    let next_top = cur_top + cur_h + gap;
    if show_prev {
        blit_text(&mut img, font, prev_line, sub_scale, center_x(prev_line, sub_scale),
            sub_ascent + cur_top as f32 - sub_h as f32 - gap as f32, dim_color(st.base), 1.0, None);
    }
    let base_x = ((w as i64 - cur_w as i64) / 2).max(0) as f32;
    let baseline = cur_ascent + cur_top as f32;
    let stops = [st.sung, mix_color(st.sung, st.cur, 0.55), st.cur];
    blit_gradient_clipped(&mut img, font, current, cur_scale, base_x, baseline, &stops, cur_w as f32, f32::MAX, 1.0);
    if show_next {
        blit_text(&mut img, font, next_line, sub_scale, center_x(next_line, sub_scale),
            sub_ascent + next_top as f32, dim_color(st.base), 1.0, None);
    }
    Some(img)
}


/// Copy a wallpaper folder (with project.json) into the wallpaper library.
fn install_wallpaper_pack(src: &str) -> Result<String, String> {
    let src_path = std::path::Path::new(src);
    if !src_path.is_dir() {
        return Err(format!("'{src}' is not a folder"));
    }
    if !src_path.join("project.json").is_file() {
        return Err("folder must contain project.json".to_string());
    }
    let name = src_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or("invalid folder name")?;
    let lib = wallpaper_pack::library_dir();
    std::fs::create_dir_all(&lib).map_err(|e| e.to_string())?;
    let dst = lib.join(&name);
    if dst.exists() {
        std::fs::remove_dir_all(&dst).map_err(|e| e.to_string())?;
    }
    copy_dir(src_path, &dst).map_err(|e| e.to_string())?;
    Ok(name)
}

/// Recursive directory copy (no extra deps).
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 解析后的壁纸（pack 或裸路径）。
struct ResolvedWp {
    file: String,
    kind: String,
    params: String,
    qml: Option<String>,
    lua: Option<String>,
}

/// 应用壁纸包内的 QML 样式与 Lua 行为（保留壁纸相关配置字段）。
fn apply_pack_style(cfg: &mut config::Config, rwp: &ResolvedWp) {
    if let Some(q) = &rwp.qml {
        if let Ok(src) = std::fs::read_to_string(q) {
            let parsed = config::parse_for_test(&src);
            let wp = cfg.image_wallpaper.clone();
            let vw = cfg.video_wallpaper.clone();
            let ww = cfg.web_wallpaper.clone();
            let sw = cfg.scene_wallpaper.clone();
            let wp_list = cfg.wallpapers.clone();
            *cfg = parsed;
            cfg.image_wallpaper = wp;
            cfg.video_wallpaper = vw;
            cfg.web_wallpaper = ww;
            cfg.scene_wallpaper = sw;
            cfg.wallpapers = wp_list;
            log::info!("pack: applied QML style {q}");
        }
    }
    if let Some(l) = &rwp.lua {
        cfg.lua_script = Some(l.clone());
        log::info!("pack: applied Lua behavior {l}");
    }
}

fn resolve_wallpaper(path: &str) -> ResolvedWp {
    // 壁纸库：裸名字或相对路径先查 ~/.config/pulse-ring/wallpapers/<name>
    let resolved_path = if std::path::Path::new(path).is_absolute() || std::path::Path::new(path).exists() {
        path.to_string()
    } else {
        wallpaper_pack::resolve_library_path(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string())
    };
    let path = resolved_path.as_str();
    if let Some(pack) = wallpaper_pack::resolve_pack(path) {
        let kind = pack.spec.kind.to_ascii_lowercase();
        let kind = if kind == "video" { "video" } else if kind == "image" { "image" } else { "web" };
        ResolvedWp { file: pack.file, kind: kind.to_string(), params: pack.params_json, qml: pack.qml, lua: pack.lua }
    } else if web_wallpaper::is_html_path(path) {
        ResolvedWp { file: path.to_string(), kind: "web".into(), params: "{}".into(), qml: None, lua: None }
    } else if video_wallpaper::is_video_path(path) {
        ResolvedWp { file: path.to_string(), kind: "video".into(), params: "{}".into(), qml: None, lua: None }
    } else {
        ResolvedWp { file: path.to_string(), kind: "image".into(), params: "{}".into(), qml: None, lua: None }
    }
}

impl App {
    fn texture_slot_matches(&self, slot: usize, key: &str) -> bool {
        self.texture_slots
            .get(slot)
            .is_some_and(|state| state.image.is_some() && state.key == key)
    }

    fn set_texture_slot(
        &mut self,
        slot: usize,
        key: String,
        image: std::sync::Arc<ImageData>,
        kind: TextureKind,
    ) -> bool {
        let Some(state) = self.texture_slots.get_mut(slot) else {
            log::warn!("texture slot {} is outside atlas capacity", slot);
            return false;
        };
        state.update(key, image, kind)
    }

    fn replace_texture_slot(
        &mut self,
        slot: usize,
        key: String,
        image: ImageData,
        kind: TextureKind,
    ) -> bool {
        self.set_texture_slot(slot, key, std::sync::Arc::new(image), kind)
    }

    fn drain_lyric_raster_results(&mut self) {
        while let Ok(result) = self.lyric_raster_rx.try_recv() {
            if result.slot >= self.lyric_raster_ready.len() {
                continue;
            }
            let slot = result.slot;
            if self.lyric_raster_ready[slot].replace(result).is_some() && self.profile_enabled {
                self.profile.lyric_stale_results += 1;
            }
        }
    }

    fn accept_ready_lyric(&mut self, slot: usize, desired_signature: &str) {
        let Some(result) = self.lyric_raster_ready[slot].take() else {
            return;
        };
        if lyric_result_is_current(
            self.lyric_raster_pending[slot].as_ref(),
            result.generation,
            &result.signature,
            desired_signature,
        ) {
            self.lyric_cache[slot] = Some((
                result.signature,
                std::sync::Arc::new(result.image),
            ));
            self.lyric_raster_pending[slot] = None;
        } else if self.profile_enabled {
            self.profile.lyric_stale_results += 1;
        }
    }

    /// Compute widget uniform data (12 f32 each). Returns the 96-float layout.
    /// `widgets` is the per-frame snapshot taken once in compute_scene.
    fn prepare_widgets(&mut self, widgets: &[crate::config::WidgetConfig], min_d: f32) -> [f32; 1280] {
        use crate::config::WidgetType;
        let mut data = [0.0f32; 1280];
        self.drain_lyric_raster_results();
        while let Ok(img) = self.cover_rx.try_recv() {
            self.cover_loaded = true;
            self.cover_aspect = img.h as f32 / img.w as f32;
            let key = format!(
                "cover:{}",
                self.texture_slots[COVER_TEXTURE_SLOT].revision.wrapping_add(1)
            );
            self.replace_texture_slot(
                COVER_TEXTURE_SLOT,
                key,
                fit_slot(img),
                TextureKind::Cover,
            );
        }
        let (texture_layout, texture_overflow) = texture_slot_layout(widgets);
        if texture_overflow > 0 && !self.texture_overflow_warned {
            log::warn!(
                "{} texture-backed widgets exceed the {} general atlas slots and will be hidden",
                texture_overflow,
                GENERAL_TEXTURE_SLOTS.len(),
            );
            self.texture_overflow_warned = true;
        }
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
                    let pidx = w
                        .plugin
                        .as_ref()
                        .and_then(|n| self.plugins.iter().position(|p| p.name() == n))
                        .or_else(|| (!self.plugins.is_empty()).then_some(0));
                    if let Some(pidx) = pidx.filter(|idx| *idx < MAX_PLUGIN_TEXTURES) {
                        data[o + 6] = (PLUGIN_TEXTURE_START + pidx) as f32;
                        data[o + 11] = 1.0;
                    } else {
                        data[o + 4] = 0.0;
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
                    data[o + 6] = COVER_TEXTURE_SLOT as f32;
                    // 18=border width, 19=cover growth
                    data[o + 18] = w.border_width.max(0.0);
                    data[o + 19] = w.cover_growth.max(0.0);
                    data[o + 11] = self.cover_aspect;
                    // border colour from widget.color -> colors[0]
                    for (ci, ch) in w.color.iter().enumerate() {
                        data[o + 23 + ci] = *ch;
                    }
                    if !self.cover_loaded {
                        data[o + 4] = 0.0;
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
                    let Some(texture_slot) = texture_layout[slot] else {
                        data[o + 4] = 0.0;
                        continue;
                    };
                    let src = match &w.source {
                        Some(s) => s.clone(),
                        None => {
                            data[o + 4] = 0.0;
                            continue;
                        }
                    };
                    let key = format!("image:{src}");
                    if !self.texture_slot_matches(texture_slot, &key) {
                        if let Some(img) = self.get_image(&src) {
                            self.replace_texture_slot(
                                texture_slot,
                                key.clone(),
                                fit_slot((*img).clone()),
                                TextureKind::General,
                            );
                        }
                    }
                    if self.texture_slot_matches(texture_slot, &key) {
                        let img = self.texture_slots[texture_slot]
                            .image
                            .as_ref()
                            .expect("matching texture slot has an image");
                        let (iw, ih) = (img.w as f32, img.h as f32);
                        data[o + 6] = texture_slot as f32;
                        data[o + 11] = ih / iw; // aspect
                    } else {
                        data[o + 4] = 0.0;
                    }
                }
                WidgetType::Clock => {
                    let Some(texture_slot) = texture_layout[slot] else {
                        data[o + 4] = 0.0;
                        continue;
                    };
                    let txt = chrono_now();
                    let key = format!(
                        "clock:{txt}|{}|{:?}",
                        w.font_size.to_bits(),
                        w.color.map(f32::to_bits),
                    );
                    if !self.texture_slot_matches(texture_slot, &key) {
                        // 3x supersampling: sharper text when downscaled on screen.
                        let img = fit_slot(rasterize_text(&self.font, &txt, w.font_size * 3.0, w.color));
                        self.replace_texture_slot(
                            texture_slot,
                            key,
                            img,
                            TextureKind::General,
                        );
                    }
                    if let Some(img) = self.texture_slots[texture_slot].image.as_ref() {
                        let (iw, ih) = (img.w, img.h);
                        data[o + 11] = ih as f32 / iw as f32;
                    } else {
                        data[o + 4] = 0.0;
                    }
                    data[o + 6] = texture_slot as f32;
                }
                WidgetType::Lyric => {
                    // Start hidden. It becomes visible only when a valid banner (current
                    // or same-track stale fallback) is bound, preventing stale UV flashes.
                    data[o + 4] = 0.0;
                    let Some(texture_slot) = texture_layout[slot] else { continue };
                    let Some(lt) = self.lyric_time() else { continue };
                    let Some(ldata) = &self.lyric_data else { continue };
                    let Some(ls) = lyrics::line_state(ldata, lt + w.lyric_offset) else { continue };
                    // Instrumental gap (empty timed line): HOLD the last real line as the
                    // current one until the next section starts — the standard lyric-app
                    // behaviour. Only when there is no previous line at all (gap before
                    // the first lyric) is the rail hidden.
                    let mut idx = ls.index;
                    if ldata.lines[idx].text.trim().is_empty() {
                        match lyrics::prev_real_line(&ldata.lines, idx) {
                            Some(p) => idx = p,
                            None => {
                                // Gap before any lyric: hide the widget cleanly.
                                data[o + 4] = 0.0;
                                continue;
                            }
                        }
                    }
                    let cur = ldata.lines[idx].text.clone();
                    // Prev/next are the nearest REAL lines (skip empty instrumental gaps).
                    let prev = if w.show_prev_next {
                        lyrics::prev_real_line(&ldata.lines, idx)
                            .map(|i| ldata.lines[i].text.clone())
                    } else {
                        None
                    };
                    let next = if w.show_prev_next {
                        lyrics::next_real_line(&ldata.lines, idx)
                            .map(|i| ldata.lines[i].text.clone())
                    } else {
                        None
                    };
                    // Colours: colors[0]=base(上下行) colors[1]=点亮色 colors[2]=高光 colors[3]=辉光
                    let style = LyricStyle {
                        font_size: w.font_size,
                        base: w.colors.first().copied().unwrap_or([0.85, 0.9, 1.0, 1.0]),
                        sung: w.colors.get(1).copied().unwrap_or([1.0, 0.78, 0.35, 1.0]),
                        cur: w.colors.get(2).copied().unwrap_or([1.0, 1.0, 1.0, 1.0]),
                        show_prev_next: w.show_prev_next,
                    };
                    let sig = format!(
                        "{slot}|{}|{}|{}|{}|{}|{}|{:?}|{:?}|{:?}",
                        self.lyric_key,
                        cur,
                        prev.as_deref().unwrap_or(""),
                        next.as_deref().unwrap_or(""),
                        w.font_size.to_bits(),
                        w.show_prev_next,
                        style.base.map(f32::to_bits),
                        style.sung.map(f32::to_bits),
                        style.cur.map(f32::to_bits),
                    );
                    self.accept_ready_lyric(slot, &sig);
                    let cached_signature = self.lyric_cache[slot]
                        .as_ref()
                        .map(|(cached_sig, _)| cached_sig.as_str());
                    let pending_signature = self.lyric_raster_pending[slot]
                        .as_ref()
                        .map(|(_, pending_sig)| pending_sig.as_str());
                    if lyric_request_needed(cached_signature, pending_signature, &sig) {
                        self.lyric_raster_seq = self.lyric_raster_seq.wrapping_add(1).max(1);
                        let generation = self.lyric_raster_seq;
                        let req = LyricRasterReq {
                            generation,
                            slot,
                            signature: sig.clone(),
                            font: self.font.clone(),
                            prev,
                            current: cur,
                            next,
                            style,
                        };
                        self.lyric_raster_pending[slot] = Some((generation, sig));
                        if self.lyric_raster_tx.send(req).is_ok() {
                            if self.profile_enabled {
                                self.profile.lyric_requests += 1;
                            }
                        } else {
                            self.lyric_raster_pending[slot] = None;
                        }
                    } else if cached_signature != Some(sig.as_str()) {
                        if self.profile_enabled {
                            self.profile.lyric_deduped += 1;
                        }
                    }
                    let rendered = self.lyric_cache[slot]
                        .as_ref()
                        .map(|(cached_sig, img)| (cached_sig.clone(), img.clone()));
                    if let Some((cached_sig, img)) = rendered {
                        let (iw, ih) = (img.w as f32, img.h as f32);
                        // Render the banner at its NATIVE size: the quad width tracks the
                        // rasterised width, so the text is always `fontSize` on screen.
                        data[o + 3] = (iw / min_d).clamp(0.01, 0.95);
                        data[o + 4] = w.alpha;
                        data[o + 6] = texture_slot as f32;
                        data[o + 11] = ih / iw;
                        self.set_texture_slot(
                            texture_slot,
                            cached_sig,
                            img,
                            TextureKind::Lyric,
                        );
                    }
                }
            }
            if matches!(
                w.widget_type,
                WidgetType::Image
                    | WidgetType::Clock
                    | WidgetType::Cover
                    | WidgetType::Plugin
                    | WidgetType::Lyric
            ) && data[o + 4] > 0.0
            {
                let texture_slot = data[o + 6] as usize;
                let image = self
                    .texture_slots
                    .get(texture_slot)
                    .and_then(|state| state.image.as_ref());
                if let Some(image) = image {
                    if let Some((ux, uy, uw, uh)) =
                        draw::atlas_content_uv(texture_slot, image.w, image.h)
                    {
                        data[o + 7] = ux;
                        data[o + 8] = uy;
                        data[o + 9] = uw;
                        data[o + 10] = uh;
                    } else {
                        data[o + 4] = 0.0;
                    }
                } else {
                    data[o + 4] = 0.0;
                }
            }
        }
        data
    }

    fn get_image(&mut self, path: &str) -> Option<std::sync::Arc<ImageData>> {
        // Simple cache; expand ~ in path.
        let expanded = path.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
        if let Some(pos) = self.image_cache.iter().position(|(p, _)| *p == expanded) {
            return Some(self.image_cache[pos].1.clone());
        }
        if let Some(img) = load_png(&expanded) {
            self.image_cache.push((expanded, std::sync::Arc::new(img)));
            return self.image_cache.last().map(|(_, d)| d.clone());
        }
        None
    }

    /// Refresh MPRIS music info (throttled by the cover thread cadence: cheap anyway).
    fn poll_music(&mut self) {
        // Drain the background MPRIS thread (non-blocking — the main thread never
        // spawns subprocesses, which used to stall every second of rendering).
        let mut last: Option<MusicSnapshot> = None;
        while let Ok(snap) = self.music_rx.try_recv() {
            last = Some(snap);
        }
        let Some(snap) = last else { return };
        let title = (!snap.title.is_empty()).then_some(snap.title.clone());
        let artist = (!snap.artist.is_empty()).then_some(snap.artist.clone());
        if let Some(t) = title {
            let changed = self.music.title != t;
            if changed {
                // Never reuse a previous track's banner while the new track is loading.
                self.lyric_cache.fill(None);
                self.lyric_raster_pending.fill(None);
                self.lyric_raster_ready.iter_mut().for_each(|ready| *ready = None);
                self.lyric_cur_idx = -1;
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
                let key = format!("{}\u{1}{}\u{1}{:.1}", t, artist.as_deref().unwrap_or(""), snap.duration_sec);
                self.lyric_key = key.clone();
                // Reset the monotonic lyric clock for the new track.
                self.lyric_t_prev = -1000.0;
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
        // Detect real backward seeks (progress-bar drags): the polled position drops
        // well below the previous poll — allow the lyric clock to jump instantly
        // instead of crawling back line-by-line through the monotonic clamp.
        if snap.position_sec < self.music.position_sec - 0.8 {
            log::info!("lyric: seek backward detected ({}s)", snap.position_sec);
            self.lyric_t_prev = snap.position_sec - 1.0;
            // Also snap the current line so the transition doesn't re-trigger.
            self.lyric_cur_idx = -1;
        }
        self.music.position_sec = snap.position_sec;
        self.lyric_pos_poll_elapsed = self.start.elapsed().as_secs_f32();
        self.music.playing = snap.playing;
        // Drain lyric fetch results; only accept the one matching the current track.
        while let Ok((key, data)) = self.lyric_rx.try_recv() {
            if key == self.lyric_key {
                self.lyric_data = data;
                log::info!(
                    "lyric: loaded {} lines",
                    self.lyric_data.as_ref().map_or(0, |d| d.lines.len())
                );
            }
        }
    }


    /// Current lyric playback time: MPRIS position advanced by elapsed wall time while playing.
    fn lyric_time(&mut self) -> Option<f32> {
        if self.lyric_data.is_none() {
            return None;
        }
        let t = if self.music.playing {
            let dt = (self.start.elapsed().as_secs_f32() - self.lyric_pos_poll_elapsed).max(0.0);
            self.music.position_sec + dt
        } else {
            self.music.position_sec
        };
        // Clamp: never run more than 0.25s backward. MPRIS poll corrections can snap
        // the extrapolated time back a little; without this the karaoke bar visibly
        // shrinks and the line-change transition can re-trigger (flash).
        let clamped = t.max(self.lyric_t_prev - 0.25);
        self.lyric_t_prev = clamped;
        Some(clamped)
    }

    /// Ask each plugin to render its RGBA texture, then store into texture_slots for
    /// `type: "plugin"` widgets (each plugin owns slot = 8 + plugin index).
    fn render_plugin_textures(&mut self) {
        let n = self.plugins.len();
        if n > MAX_PLUGIN_TEXTURES && !self.plugin_overflow_warned {
            log::warn!(
                "{} plugins exceed the {} atlas plugin slots; extras will not render",
                n,
                MAX_PLUGIN_TEXTURES,
            );
            self.plugin_overflow_warned = true;
        }
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
        let mut updates = Vec::new();
        for (i, p) in self.plugins.iter().take(MAX_PLUGIN_TEXTURES).enumerate() {
            let slot = (PLUGIN_TEXTURE_START + i) as u32;
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
            updates.push((
                PLUGIN_TEXTURE_START + i,
                format!("plugin:{}:{}", p.name(), self.start.elapsed().as_nanos()),
                ImageData { w, h, rgba },
            ));
        }
        for (slot, key, image) in updates {
            self.replace_texture_slot(slot, key, image, TextureKind::Plugin);
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
        // Video wallpaper: pull the newest decoded frame and mark the wallpaper dirty
        // so every renderer re-uploads it (video frames need no mipmaps).
        if let Some(player) = &self.video_player {
            if let Some(frame) = video_wallpaper::drain_video(&player.rx) {
                let img = ImageData {
                    w: frame.width,
                    h: frame.height,
                    rgba: frame.rgba,
                };
                let first = self.video_first_frame;
                self.video_first_frame = false;
                self.wallpaper_image = Some(img);
                self.wallpaper_dirty = true;
                if first {
                    // First frame: full upload so the crossfade transition starts.
                    self.wallpaper_force_upload = true;
                }
            }
        }
        if let Some(player) = &mut self.scene_player {
            if let Some(frame) = web_wallpaper::drain_web(&player.rx) {
                self.wallpaper_image = Some(ImageData {
                    w: frame.width,
                    h: frame.height,
                    rgba: frame.rgba,
                });
                self.wallpaper_dirty = true;
                if self.scene_first_frame {
                    self.scene_first_frame = false;
                    self.wallpaper_force_upload = true;
                }
            }
        }
        if let Some(player) = &mut self.web_player {
            if let Some(frame) = web_wallpaper::drain_web(&player.rx) {
                self.wallpaper_image = Some(ImageData {
                    w: frame.width,
                    h: frame.height,
                    rgba: frame.rgba,
                });
                self.wallpaper_dirty = true;
                if self.web_first_frame {
                    self.web_first_frame = false;
                    self.wallpaper_force_upload = true;
                }
            }
        }
        self.tick_wallpaper_rotation();
        let scene = self.compute_scene();
        // With an image wallpaper configured, every monitor shows it (wallpaper-engine
        // behaviour) — render all outputs regardless of the renderScreen visualisation cap.
        let target = if self.wallpaper_image.is_some() { -1 } else { self.cfg.render_screen };
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
        // Every renderer has seen the wallpaper this frame; clear the change flag.
        self.wallpaper_dirty = false;
        if self.profile_enabled {
            self.profile.max_frame = self.profile.max_frame.max(t0.elapsed().as_secs_f32());
        }
        self.profile_maybe_log();
    }

    /// Advance the rotating-wallpaper transition and switch to the next image when the
    /// interval elapses. Video wallpaper bypasses rotation (holds progress at 1.0).
    fn tick_wallpaper_rotation(&mut self) {
        // A scene is the living wallpaper — rotation only applies to image/video lists.
        if self.scene_player.is_some() || self.wallpaper_list.is_empty() {
            self.wallpaper_progress = 1.0;
            return;
        }
        let elapsed = self.start.elapsed().as_secs_f32();
        let dur = self.cfg.wallpaper_transition.max(0.1);
        if self.wallpaper_switch_at == 0.0 {
            self.wallpaper_switch_at = elapsed + self.cfg.wallpaper_interval;
        }
        let mut progress = ((elapsed - self.wallpaper_transition_start) / dur).clamp(0.0, 1.0);
        if progress >= 1.0 && elapsed >= self.wallpaper_switch_at {
            self.wallpaper_idx = (self.wallpaper_idx + 1) % self.wallpaper_list.len();
            let path = &self.wallpaper_list[self.wallpaper_idx];
            self.video_player = None;
            self.web_player = None;
            let rwp = resolve_wallpaper(path);
            match rwp.kind.as_str() {
                "web" => {
                    let (w, h) = self.cfg.web_wallpaper_size;
                    match web_wallpaper::start_web_wallpaper(&rwp.file, w, h) {
                        Ok(mut p) => {
                            p.send_config(&rwp.params);
                            self.web_player = Some(p);
                            self.web_first_frame = true;
                        }
                        Err(e) => log::warn!("web wallpaper failed ({e})"),
                    }
                }
                "video" => {
                    match video_wallpaper::start_video_wallpaper(&rwp.file, self.cfg.video_wallpaper_audio) {
                        Ok(p) => {
                            self.video_player = Some(p);
                            self.video_first_frame = true;
                        }
                        Err(e) => log::warn!("video wallpaper failed ({e})"),
                    }
                }
                _ => {
                    if let Some(img) = load_image_raw(&rwp.file) {
                        self.wallpaper_image = Some(img);
                        self.wallpaper_dirty = true;
                    }
                }
            }
            self.wallpaper_transition_start = elapsed;
            self.wallpaper_switch_at = elapsed + self.cfg.wallpaper_interval;
            progress = 0.0;
        }
        self.wallpaper_progress = progress;
    }

    
/// Resolve a wallpaper path: a packaged folder (project.json) -> (file, kind, params_json);
/// otherwise the raw path with a kind guessed from the extension.


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
            log::info!("{}", ProfileStats::format_line(&self.profile, 60));
            self.profile = ProfileStats::default();
        }
    }

    /// Compute the full scene ONCE per tick (audio, Lua, plugins, particles, widgets).
    /// Every output consumes the same SceneFrame, so CPU work no longer scales with monitor count.
    fn compute_scene(&mut self) -> SceneFrame {
        let t_lua = std::time::Instant::now();
        let elapsed = self.start.elapsed().as_secs_f32();
        // MPRIS state arrives on a background thread; draining is cheap, do it every frame.
        self.poll_music();
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
        let widgets = self.prepare_widgets(&widgets_cfg, sw.min(sh) as f32);
        self.profile_mark("widgets", t_widgets);
        let bar_energy = compute_bar_energy(&render_bands);
        let overall = compute_overall_energy(&render_bands);
        let band_energy = compute_band_energy(&render_bands);
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
            band_energy,
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
        let widgets = scene.widgets;
        let (renderer, uploaded_revisions) = {
            let output = &mut self.outputs[idx];
            (&mut output.renderer, &mut output.uploaded_texture_revisions)
        };
        let mut lyric_uploads = 0u64;
        // A renderer uploads a slot only when its local revision lags behind the
        // application slot. Late-added monitors therefore populate their atlas once.
        for (ti, state) in self.texture_slots.iter().enumerate() {
            if !texture_needs_upload(state.revision, uploaded_revisions[ti]) {
                continue;
            }
            if let Some(img) = state.image.as_ref() {
                if renderer.upload_texture(ti, &img.rgba, img.w, img.h).is_some() {
                    uploaded_revisions[ti] = state.revision;
                    if state.kind == TextureKind::Lyric {
                        lyric_uploads += 1;
                    }
                }
            }
        }
        if self.profile_enabled {
            self.profile.lyric_uploads += lyric_uploads;
        }
        // Image wallpaper: upload once per change to each renderer (behind everything).
        renderer.set_wallpaper_progress(self.wallpaper_progress);
        renderer.set_transition_name(&self.cfg.wallpaper_transition_effect);
        if self.wallpaper_dirty {
            if let Some(img) = &self.wallpaper_image {
                if (self.video_player.is_some() || self.web_player.is_some() || self.scene_player.is_some())
                    && !self.wallpaper_force_upload
                {
                    // Video/web: reuse the texture (same size), no mipmap generation per frame.
                    renderer.update_wallpaper(&img.rgba, img.w, img.h);
                } else {
                    renderer.upload_wallpaper(&img.rgba, img.w, img.h);
                }
                self.wallpaper_force_upload = false;
                if self.log_once_wallpaper {
                    self.log_once_wallpaper = false;
                    log::info!("wallpaper: uploaded {}x{} to output {}", img.w, img.h, idx);
                }
            }
            renderer.set_wallpaper_mode(match self.cfg.image_wallpaper_mode {
                crate::config::WallpaperMode::Contain => 1,
                crate::config::WallpaperMode::Stretch => 2,
                _ => 0,
            });
        }
        renderer.set_widgets(&widgets);
        let widget_bounds = compute_widget_bounds(&scene.widgets_cfg, width, height);
        renderer.set_widget_bounds(&widget_bounds);
        renderer.resize(width, height);
        renderer.set_auto_rotate(scene.rotate_rad);
        renderer.set_bar_energy(&scene.bar_energy);
        renderer.set_overall_energy(scene.overall);
        renderer.set_band_energy(&scene.band_energy);
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
    use crate::config::parse_for_test;

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
        p.max_frame = 0.012;
        p.lyric_requests = 2;
        p.lyric_deduped = 3;
        p.lyric_stale_results = 4;
        p.lyric_uploads = 5;
        let s = super::ProfileStats::format_line(&p, 2);
        assert!(s.contains("pull_audio=0.50ms"), "got: {s}");
        assert!(s.contains("render=3.50ms"), "got: {s}");
        assert!(s.contains("total=14.00ms"), "got: {s}");
        assert!(s.contains("max_frame=12.00ms"), "got: {s}");
        assert!(s.contains("requests=2,deduped=3,stale=4,uploads=5"), "got: {s}");
    }

    #[test]
    fn texture_slot_layout_is_stable_and_bounded() {
        use crate::config::{WidgetConfig, WidgetType};
        let mut widgets = Vec::new();
        for kind in [
            WidgetType::Image,
            WidgetType::Clock,
            WidgetType::Lyric,
            WidgetType::Image,
            WidgetType::Clock,
            WidgetType::Lyric,
            WidgetType::Image,
            WidgetType::Clock,
        ] {
            let mut widget = WidgetConfig::default();
            widget.widget_type = kind;
            widgets.push(widget);
        }
        let (layout, overflow) = super::texture_slot_layout(&widgets);
        assert_eq!(&layout[..7], &[Some(0), Some(1), Some(2), Some(4), Some(5), Some(6), Some(7)]);
        assert_eq!(layout[7], None);
        assert_eq!(overflow, 1);
        assert_eq!(super::COVER_TEXTURE_SLOT, 3);
        assert_eq!(super::PLUGIN_TEXTURE_START, 8);
        assert_eq!(super::MAX_PLUGIN_TEXTURES, 8);
    }

    #[test]
    fn texture_revisions_change_only_with_content() {
        let image = std::sync::Arc::new(super::ImageData {
            w: 1,
            h: 1,
            rgba: vec![255, 255, 255, 255],
        });
        let mut state = super::TextureSlotState::default();
        assert!(state.update("a".into(), image.clone(), super::TextureKind::General));
        let revision = state.revision;
        assert!(!state.update("a".into(), image.clone(), super::TextureKind::General));
        assert_eq!(state.revision, revision);
        assert!(state.update("b".into(), image, super::TextureKind::Lyric));
        assert!(state.revision > revision);
        assert!(super::texture_needs_upload(state.revision, 0));
        assert!(!super::texture_needs_upload(state.revision, state.revision));
        // A late-added output begins at revision 0 and must upload current content.
        assert!(super::texture_needs_upload(state.revision, 0));
    }

    #[test]
    fn lyric_requests_deduplicate_and_results_are_versioned() {
        assert!(super::lyric_request_needed(None, None, "line-a"));
        assert!(!super::lyric_request_needed(None, Some("line-a"), "line-a"));
        assert!(!super::lyric_request_needed(Some("line-a"), None, "line-a"));
        assert!(super::lyric_request_needed(Some("line-a"), Some("line-b"), "line-c"));

        let pending = (9u64, "line-b".to_string());
        assert!(super::lyric_result_is_current(Some(&pending), 9, "line-b", "line-b"));
        assert!(!super::lyric_result_is_current(Some(&pending), 8, "line-b", "line-b"));
        assert!(!super::lyric_result_is_current(Some(&pending), 9, "line-b", "line-c"));
        assert!(!super::lyric_result_is_current(None, 9, "line-b", "line-b"));
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
        let energy = super::compute_band_energy(&bands);
        assert_eq!(energy[0], 0.0);
        assert!((energy[1] - 1.0 / 64.0).abs() < 1e-6);
        assert_eq!(energy[2], 0.0);
        assert!((energy[3] - 1.0 / 128.0).abs() < 1e-6);
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

        let mut bars = WidgetConfig::default();
        bars.widget_type = WidgetType::Bars;
        bars.size = 0.55;
        bars.bar_height = 0.14;
        let b = super::compute_widget_bounds(&[bars], 1920, 1080);
        // Half the 594px width plus the 2px AA margin, not the old 624px radius.
        assert!((b[0] - 299.0).abs() < 1.0, "bars bound={}", b[0]);
    }

    #[test]
    fn frame_interval_adapts_to_energy() {
        use super::frame_interval_ms;
        assert_eq!(frame_interval_ms(30), 33); // 30fps
        assert_eq!(frame_interval_ms(60), 16); // 60fps
        assert_eq!(frame_interval_ms(20), 50); // 20fps
    }

    #[test]
    fn lyric_raster_current_line_is_lit() {
        use crate::{load_font, rasterize_lyric_image, LyricStyle};
        let font = load_font();
        let st = LyricStyle {
            font_size: 40.0,
            base: [0.85, 0.9, 1.0, 1.0],
            sung: [1.0, 0.78, 0.35, 1.0],
            cur: [1.0, 1.0, 1.0, 1.0],
            show_prev_next: false,
        };
        // Current line is baked fully lit (gold -> white gradient across its width).
        let img = rasterize_lyric_image(&font, None, "hello world", None, &st).expect("rasterize");
        assert!(img.w > 10 && img.h > 4);
        let mid = img.h / 2;
        let mut found_gold = false;
        let mut found_white = false;
        for x in 0..img.w {
            let o = ((mid * img.w + x) * 4) as usize;
            let (r, g, b, a) = (img.rgba[o], img.rgba[o + 1], img.rgba[o + 2], img.rgba[o + 3]);
            if a > 40 {
                if r > 190 && g > 120 && g < 235 && b < 130 {
                    found_gold = true;
                }
                if r > 230 && g > 230 && b > 230 {
                    found_white = true;
                }
            }
        }
        assert!(found_gold, "no gold gradient start");
        assert!(found_white, "no white gradient end");
    }

    fn lyric_raster_prev_next_are_dim() {
        use crate::{load_font, rasterize_lyric_image, LyricStyle};
        let font = load_font();
        let st = LyricStyle {
            font_size: 40.0,
            base: [0.85, 0.9, 1.0, 1.0],
            sung: [1.0, 0.78, 0.35, 1.0],
            cur: [1.0, 1.0, 1.0, 1.0],
            show_prev_next: true,
        };
        // prev + current + next: the current line must be far brighter than the dim
        // prev/next lines (line-level highlighting).
        let img = rasterize_lyric_image(&font, Some("prev"), "CURRENT", Some("next"), &st).expect("rasterize");
        assert!(img.h > 30, "banner should have 3 lines, got h={}", img.h);
        let mut lit = 0usize;
        let mut dim = 0usize;
        for y in 0..img.h {
            for x in 0..img.w {
                let o = ((y * img.w + x) * 4) as usize;
                let a = img.rgba[o + 3] as usize;
                if a > 120 {
                    lit += 1;
                } else if a > 40 {
                    dim += 1;
                }
            }
        }
        assert!(lit > 40, "current line should have many fully-lit pixels, got {lit}");
        assert!(dim > 0, "prev/next dim pixels expected");
    }

}

