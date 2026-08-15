//! Lua scripting support.
//!
//! A user script (configured via `luaScript` in the QML file) runs every frame and can:
//! - transform the 128 band magnitudes (`transformBands(bands) -> bands`)
//! - run custom logic each frame (`onUpdate(dt)`)
//! - read/modify the live `config` table (shape, colours, sizes, …) — changes apply immediately
//! - read `music` (MPRIS title/artist/album) and `time` (h/m/s) tables

/// Fetch the live Config from the Lua registry (stored as a usize pointer).
fn get_cfg(lua: &Lua) -> mlua::Result<&mut crate::config::Config> {
    let ptr = lua.named_registry_value::<usize>("pulse_cfg")?;
    Ok(unsafe { &mut *(ptr as *mut crate::config::Config) })
}

use mlua::{Lua, Table};

use crate::config::{Config, Shape, ColorMode, ParticleMode};

pub struct LuaState {
    lua: Option<Lua>,
    script_path: String,
    cfg_ptr: *mut Config,
}

// LuaState is used single-threaded on the main thread.
unsafe impl Send for LuaState {}

impl LuaState {
    pub fn new(script: Option<&str>, cfg: &mut Config) -> Self {
        let cfg_ptr: *mut Config = cfg;
        let script = match script {
            Some(s) => s.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1),
            None => return Self { lua: None, script_path: String::new(), cfg_ptr },
        };
        let lua = Lua::new();
        lua.set_named_registry_value("pulse_cfg", cfg_ptr as usize)
            .expect("registry");
        let result = (|| -> mlua::Result<()> {
            // config table: populated each frame from Rust, and read back after onUpdate.
            let cfg_table = lua.create_table()?;
            lua.globals().set("config", cfg_table)?;
            // music info (updated periodically)
            lua.globals().set("music", lua.create_table()?)?;
            // time info
            lua.globals().set("time", lua.create_table()?)?;
            // bands table (128 entries, replaced each frame)
            let bands = lua.create_table()?;
            lua.globals().set("bands", bands)?;
            // simple log helper
            let log_fn = lua.create_function(|_, msg: String| {
                log::info!("[lua] {msg}");
                Ok(())
            })?;
            lua.globals().set("log", log_fn)?;
            // pulse table: runtime widget control.
            let pulse = lua.create_table()?;
            let add = lua.create_function(|lua, (_ty, x, y): (String, f32, f32)| {
                let cfg = get_cfg(lua)?;
                let mut w = crate::config::WidgetConfig::default();
                match _ty.as_str() {
                    "clock" => { w.widget_type = crate::config::WidgetType::Clock; w.size = 0.12; w.x = x; w.y = y; }
                    "image" => { w.widget_type = crate::config::WidgetType::Image; w.size = 0.18; w.x = x; w.y = y; }
                    "bars" => { w.widget_type = crate::config::WidgetType::Bars; w.size = 0.5; w.x = x; w.y = y; }
                    "cover" => { w.widget_type = crate::config::WidgetType::Cover; w.size = 0.18; w.x = x; w.y = y; }
                    "analog" => { w.widget_type = crate::config::WidgetType::Analog; w.size = 0.22; w.x = x; w.y = y; }
                    "lyric" | "lyrics" => { w.widget_type = crate::config::WidgetType::Lyric; w.size = 0.6; w.font_size = 40.0; w.x = x; w.y = y; }
                    _ => { w.widget_type = crate::config::WidgetType::Ring; w.size = 0.6; w.x = x; w.y = y; }
                }
                cfg.widgets.push(w);
                Ok(cfg.widgets.len())
            })?;
            let remove = lua.create_function(|lua, idx: usize| {
                let cfg = get_cfg(lua)?;
                if idx >= 1 && idx <= cfg.widgets.len() {
                    cfg.widgets.remove(idx - 1);
                }
                Ok(())
            })?;
            let get_w = lua.create_function(|lua, idx: usize| {
                let cfg = get_cfg(lua)?;
                if idx >= 1 && idx <= cfg.widgets.len() {
                    let w = &cfg.widgets[idx - 1];
                    let t = lua.create_table()?;
                    t.set("x", w.x)?;
                    t.set("y", w.y)?;
                    t.set("size", w.size)?;
                    t.set("alpha", w.alpha)?;
                    t.set("rotate", w.rotate)?;
                    Ok(Some(t))
                } else {
                    Ok(None)
                }
            })?;
            let set_w = lua.create_function(|lua, (idx, key, val): (usize, String, f32)| {
                let cfg = get_cfg(lua)?;
                if idx >= 1 && idx <= cfg.widgets.len() {
                    let w = &mut cfg.widgets[idx - 1];
                    match key.as_str() {
                        "x" => w.x = val,
                        "y" => w.y = val,
                        "size" => w.size = val,
                        "alpha" => w.alpha = val,
                        "rotate" => w.rotate = val,
                        "barHeight" => w.bar_height = val,
                        "barCount" => w.bar_count = val,
                        "barGap" => w.bar_gap = val,
                        "fontSize" => w.font_size = val,
                        "borderWidth" => w.border_width = val,
                        "coverGrowth" => w.cover_growth = val,
                        _ => {}
                    }
                }
                Ok(())
            })?;
            pulse.set("addWidget", add)?;
            pulse.set("removeWidget", remove)?;
            pulse.set("getWidget", get_w)?;
            pulse.set("setWidget", set_w)?;
            lua.globals().set("pulse", pulse)?;
            lua.load(&std::fs::read_to_string(&script).map_err(|e| mlua::Error::RuntimeError(format!("read {script}: {e}")))?)
                .set_name("pulse-ring.lua")
                .exec()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                log::info!("lua: loaded {}", script);
                Self { lua: Some(lua), script_path: script, cfg_ptr }
            }
            Err(e) => {
                log::warn!("lua: failed to load {}: {e}", script);
                Self { lua: None, script_path: script, cfg_ptr }
            }
        }
    }

    /// Per-frame: sync config + bands + music/time into Lua, call onUpdate, read config back.
    pub fn frame(&mut self, cfg: &mut Config, bands: &[f32; 128], elapsed: f32, music: &MusicInfo) {
        let lua = match &self.lua {
            Some(l) => l,
            None => return,
        };
        // Refresh the config pointer each call: cfg lives inside App and moves with it.
        let ptr: usize = cfg as *mut Config as usize;
        if lua.set_named_registry_value("pulse_cfg", ptr).is_err() {
            self.lua = None;
            return;
        }
        let res = (|| -> mlua::Result<()> {
            sync_config(lua, cfg)?;
            sync_bands(lua, bands)?;
            sync_music_time(lua, music, elapsed)?;
            // call onUpdate(dt) if defined
            if let Ok(update) = lua.globals().get::<mlua::Function>("onUpdate") {
                update.call::<()>(elapsed)?;
            }
            read_config(lua, cfg)?;
            Ok(())
        })();
        if let Err(e) = res {
            // Don't spam every frame; disable script on first error.
            log::warn!("lua: runtime error, disabling: {e}");
            self.lua = None;
        }
    }

    /// Ask Lua to transform the bands (used before rendering).
    pub fn transform_bands(&mut self, bands: &[f32; 128]) -> [f32; 128] {
        let lua = match &self.lua {
            Some(l) => l,
            None => return *bands,
        };
        let res: mlua::Result<[f32; 128]> = (|| {
            sync_bands(lua, bands)?;
            if let Ok(f) = lua.globals().get::<mlua::Function>("transformBands") {
                let ret: mlua::Table = f.call(())?;
                let mut out = *bands;
                for i in 0..128 {
                    if let Ok(v) = ret.get::<f32>(i + 1) {
                        out[i] = v;
                    }
                }
                Ok(out)
            } else {
                Ok(*bands)
            }
        })();
        res.unwrap_or(*bands)
    }

    pub fn is_enabled(&self) -> bool {
        self.lua.is_some()
    }
}

pub struct MusicInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// MPRIS position in seconds (from the last poll).
    pub position_sec: f32,
    /// True while the player is in the Playing state.
    pub playing: bool,
}

impl Default for MusicInfo {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            position_sec: 0.0,
            playing: false,
        }
    }
}

fn sync_bands(lua: &Lua, bands: &[f32; 128]) -> mlua::Result<()> {
    let t: Table = lua.globals().get("bands")?;
    for (i, &b) in bands.iter().enumerate() {
        t.set(i + 1, b)?;
    }
    Ok(())
}

fn sync_music_time(lua: &Lua, music: &MusicInfo, elapsed: f32) -> mlua::Result<()> {
    let m: Table = lua.globals().get("music")?;
    m.set("title", music.title.clone())?;
    m.set("artist", music.artist.clone())?;
    m.set("album", music.album.clone())?;
    m.set("position", music.position_sec)?;
    m.set("playing", music.playing)?;
    let t: Table = lua.globals().get("time")?;
    let (h, mi, s, _) = crate::main_now_hmsparts();
    t.set("hour", h)?;
    t.set("min", mi)?;
    t.set("sec", s)?;
    t.set("elapsed", elapsed)?;
    Ok(())
}

fn sync_config(lua: &Lua, cfg: &Config) -> mlua::Result<()> {
    let t: Table = lua.globals().get("config")?;
    t.set("shape", match cfg.shape {
        Shape::Ring => "ring",
        Shape::Square => "square",
        Shape::Diamond => "diamond",
        Shape::Hexagon => "hexagon",
        Shape::Triangle => "triangle",
        Shape::Star => "star",
        Shape::Flower => "flower",
    })?;
    t.set("corners", cfg.corners)?;
    t.set("spikiness", cfg.spikiness)?;
    t.set("rotate", cfg.rotate)?;
    t.set("autoRotate", cfg.auto_rotate)?;
    t.set("colorMode", match cfg.color_mode {
        ColorMode::Hue => "hue",
        ColorMode::Solid => "solid",
        ColorMode::Gradient => "gradient",
    })?;
    t.set("ringWidth", cfg.ring_width)?;
    t.set("baseRadius", cfg.base_radius)?;
    t.set("growth", cfg.growth)?;
    t.set("haloStrength", cfg.halo_strength)?;
    t.set("haloSize", cfg.halo_size)?;
    t.set("alpha", cfg.alpha)?;
    t.set("sensitivity", cfg.sensitivity)?;
    t.set("decay", cfg.decay)?;
    t.set("smoothness", cfg.smoothness)?;
    t.set("xOffset", cfg.x_offset)?;
    t.set("yOffset", cfg.y_offset)?;
    t.set("innerRing", cfg.inner_ring)?;
    t.set("innerRadius", cfg.inner_radius)?;
    t.set("innerGrowth", cfg.inner_growth)?;
    t.set("innerWidth", cfg.inner_width)?;
    t.set("midRing", cfg.mid_ring)?;
    t.set("midRadius", cfg.mid_radius)?;
    t.set("midGrowth", cfg.mid_growth)?;
    t.set("midWidth", cfg.mid_width)?;
    t.set("particleMode", match cfg.particle_mode {
        ParticleMode::Burst => "burst",
        ParticleMode::Orbit => "orbit",
        ParticleMode::Ring => "ring",
        ParticleMode::None => "none",
    })?;
    t.set("particleLoop", cfg.particle_loop)?;
    t.set("idleBreathe", cfg.idle_breathe)?;
    t.set("spawnEffect", match cfg.spawn_effect {
        crate::config::SpawnEffect::None => "none",
        crate::config::SpawnEffect::Expand => "expand",
        crate::config::SpawnEffect::Zoom => "zoom",
        crate::config::SpawnEffect::Magic => "magic",
    })?;
    t.set("spawnDuration", cfg.spawn_duration)?;
    t.set("spawnEase", match cfg.spawn_ease {
        crate::config::SpawnEase::OutCubic => "outCubic",
        crate::config::SpawnEase::OutBack => "outBack",
        crate::config::SpawnEase::Elastic => "elastic",
        crate::config::SpawnEase::Bounce => "bounce",
    })?;
    t.set("spawnRotate", cfg.spawn_rotate)?;
    // particles: expose as an array of tables so Lua can read/tweak them
    let ps = lua.create_table()?;
    for (i, p) in cfg.particles.iter().enumerate() {
        let item = lua.create_table()?;
        item.set("x", p.x)?;
        item.set("y", p.y)?;
        item.set("angle", p.angle)?;
        item.set("speed", p.speed)?;
        item.set("size", p.size)?;
        item.set("life", p.life)?;
        item.set("delay", p.delay)?;
        item.set("twinkle", p.twinkle)?;
        ps.set(i + 1, item)?;
    }
    t.set("particles", ps)?;
    Ok(())
}

fn read_config(lua: &Lua, cfg: &mut Config) -> mlua::Result<()> {
    let t: Table = lua.globals().get("config")?;
    // particles: replace the whole array if Lua set it
    if let Ok(ps) = t.get::<mlua::Table>("particles") {
        let len = ps.len()?;
        if len > 0 {
            cfg.particles.clear();
            for i in 1..=len {
                if let Ok(pt) = ps.get::<mlua::Table>(i) {
                    let mut p = crate::config::ParticleConfig::default();
                    if let Ok(v) = pt.get::<f32>("x") { p.x = v; }
                    if let Ok(v) = pt.get::<f32>("y") { p.y = v; }
                    if let Ok(v) = pt.get::<f32>("angle") { p.angle = v; }
                    if let Ok(v) = pt.get::<f32>("speed") { p.speed = v; }
                    if let Ok(v) = pt.get::<f32>("size") { p.size = v; }
                    if let Ok(v) = pt.get::<f32>("life") { p.life = v; }
                    if let Ok(v) = pt.get::<f32>("delay") { p.delay = v; }
                    if let Ok(v) = pt.get::<f32>("twinkle") { p.twinkle = v; }
                    cfg.particles.push(p);
                }
            }
        }
    }
    if let Ok(v) = t.get::<f32>("baseRadius") { cfg.base_radius = v; }
    if let Ok(v) = t.get::<f32>("growth") { cfg.growth = v; }
    if let Ok(v) = t.get::<f32>("ringWidth") { cfg.ring_width = v; }
    if let Ok(v) = t.get::<f32>("haloStrength") { cfg.halo_strength = v; }
    if let Ok(v) = t.get::<f32>("haloSize") { cfg.halo_size = v; }
    if let Ok(v) = t.get::<f32>("alpha") { cfg.alpha = v; }
    if let Ok(v) = t.get::<f32>("sensitivity") { cfg.sensitivity = v; }
    if let Ok(v) = t.get::<f32>("decay") { cfg.decay = v; }
    if let Ok(v) = t.get::<f32>("smoothness") { cfg.smoothness = v; }
    if let Ok(v) = t.get::<f32>("xOffset") { cfg.x_offset = v; }
    if let Ok(v) = t.get::<f32>("yOffset") { cfg.y_offset = v; }
    if let Ok(v) = t.get::<f32>("innerRadius") { cfg.inner_radius = v; }
    if let Ok(v) = t.get::<f32>("innerGrowth") { cfg.inner_growth = v; }
    if let Ok(v) = t.get::<f32>("innerWidth") { cfg.inner_width = v; }
    if let Ok(v) = t.get::<bool>("innerRing") { cfg.inner_ring = v; }
    if let Ok(v) = t.get::<f32>("midRadius") { cfg.mid_radius = v; }
    if let Ok(v) = t.get::<f32>("midGrowth") { cfg.mid_growth = v; }
    if let Ok(v) = t.get::<f32>("midWidth") { cfg.mid_width = v; }
    if let Ok(v) = t.get::<bool>("midRing") { cfg.mid_ring = v; }
    if let Ok(v) = t.get::<f32>("idleBreathe") { cfg.idle_breathe = v; }
    if let Ok(v) = t.get::<f32>("spawnDuration") { cfg.spawn_duration = v; }
    if let Ok(v) = t.get::<f32>("spawnRotate") { cfg.spawn_rotate = v; }
    if let Ok(s) = t.get::<String>("spawnEffect") {
        cfg.spawn_effect = match s.as_str() {
            "none" => crate::config::SpawnEffect::None,
            "zoom" => crate::config::SpawnEffect::Zoom,
            "magic" => crate::config::SpawnEffect::Magic,
            _ => crate::config::SpawnEffect::Expand,
        };
    }
    if let Ok(s) = t.get::<String>("spawnEase") {
        cfg.spawn_ease = match s.as_str() {
            "outBack" => crate::config::SpawnEase::OutBack,
            "elastic" => crate::config::SpawnEase::Elastic,
            "bounce" => crate::config::SpawnEase::Bounce,
            _ => crate::config::SpawnEase::OutCubic,
        };
    }
    if let Ok(s) = t.get::<String>("particleMode") {
        cfg.particle_mode = match s.as_str() {
            "burst" => crate::config::ParticleMode::Burst,
            "orbit" => crate::config::ParticleMode::Orbit,
            "ring" => crate::config::ParticleMode::Ring,
            _ => crate::config::ParticleMode::None,
        };
    }
    if let Ok(v) = t.get::<f32>("corners") { cfg.corners = v; }
    if let Ok(v) = t.get::<f32>("spikiness") { cfg.spikiness = v; }
    if let Ok(v) = t.get::<f32>("rotate") { cfg.rotate = v; }
    if let Ok(v) = t.get::<f32>("autoRotate") { cfg.auto_rotate = v; }
    if let Ok(v) = t.get::<bool>("particleLoop") { cfg.particle_loop = v; }
    Ok(())
}