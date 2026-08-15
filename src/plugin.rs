//! Rust plugin system (C ABI dynamic libraries).
//!
//! Plugins are compiled as `cdylib` and dropped into
//! `~/.config/pulse-ring/plugins/*.so` (or `$XDG_CONFIG_HOME/pulse-ring/plugins`).
//! Each plugin must export a `pulse_plugin_v1` symbol:
//!
//! ```rust,ignore
//! #[no_mangle]
//! pub static pulse_plugin_v1: PulsePluginV1 = PulsePluginV1 {
//!     name: c"my-plugin".as_ptr(),
//!     version: 1,
//!     on_load: Some(on_load),
//!     on_update: Some(on_update),
//!     transform_bands: Some(transform_bands),
//!     on_unload: None,
//! };
//! ```

use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;

use libloading::{Library, Symbol};

/// C ABI exported by every plugin. All fields optional; the plugin can implement any subset.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PulsePluginV1 {
    /// Static plugin name (UTF-8).
    pub name: *const c_char,
    /// Interface version, must be 1.
    pub version: u32,
    /// Called once when loaded, before any updates.
    pub on_load: Option<extern "C" fn(ctx: *mut PluginCtx)>,
    /// Called every frame with dt (seconds).
    pub on_update: Option<extern "C" fn(ctx: *mut PluginCtx, dt: f32)>,
    /// Called every frame; plugin may read `in_bands[128]` and write `out_bands[128]`.
    pub transform_bands: Option<extern "C" fn(ctx: *mut PluginCtx, in_bands: *const f32, out_bands: *mut f32)>,
    /// Called every frame; the plugin may draw RGBA pixels into `RenderRequest`.
    pub render_texture: Option<extern "C" fn(ctx: *mut PluginCtx, req: *mut RenderRequest)>,
    /// Called when the plugin is unloaded.
    pub on_unload: Option<extern "C" fn(ctx: *mut PluginCtx)>,
}

/// Render request: the host allocates a buffer; the plugin fills it with RGBA.
/// `update` must be set true by the plugin to mark the texture dirty (host uploads it).
#[repr(C)]
pub struct RenderRequest {
    /// Texture slot index (0..7) this plugin renders into.
    pub slot: u32,
    /// Allocated buffer size in bytes (width*height*4).
    pub buf_len: usize,
    /// Plugin fills this with RGBA8.
    pub buf: *mut u8,
    /// Set true by the plugin to request an upload.
    pub update: bool,
    /// Width of the rendered content (plugin sets this).
    pub width: u32,
    /// Height of the rendered content (plugin sets this).
    pub height: u32,
    /// Host resolution hint (screen width/height) so plugins can scale.
    pub screen_w: u32,
    pub screen_h: u32,
}

/// Opaque handle passed to plugin callbacks. The plugin uses the function pointers to
/// interact with the host (read/write config, read bands, log, query music/time).
#[repr(C)]
pub struct PluginCtx {
    pub user: *mut c_void,
    pub get_config_f32: Option<extern "C" fn(user: *mut c_void, key: *const c_char) -> f32>,
    pub set_config_f32: Option<extern "C" fn(user: *mut c_void, key: *const c_char, val: f32)>,
    pub get_band: Option<extern "C" fn(user: *mut c_void, idx: u32) -> f32>,
    pub log: Option<extern "C" fn(user: *mut c_void, msg: *const c_char)>,
    pub get_time_hms: Option<extern "C" fn(user: *mut c_void, out: *mut [i32; 3])>,
}

pub struct LoadedPlugin {
    _lib: Library,
    name: String,
    plugin: PulsePluginV1,
    ctx: PluginCtx,
    enabled: bool,
}

impl LoadedPlugin {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Load all plugins and immediately attach a minimal host ctx (log + time available).
pub fn load_plugins_with_log() -> Vec<LoadedPlugin> {
    let mut plugins = load_plugins();
    for p in plugins.iter_mut() {
        let ctx = PluginCtx {
            user: std::ptr::null_mut(),
            get_config_f32: None,
            set_config_f32: None,
            get_band: None,
            log: Some(host_log),
            get_time_hms: Some(host_time),
        };
        p.set_ctx(ctx);
    }
    plugins
}

/// Load all plugins from `~/.config/pulse-ring/plugins/*.so`.
pub fn load_plugins() -> Vec<LoadedPlugin> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    let dir = base.join("pulse-ring").join("plugins");
    let dir = if dir.exists() { dir } else { return vec![] };
    let mut out = Vec::new();
    let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return vec![],
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().map(|e| e == "so").unwrap_or(false) {
            match load_plugin(&path) {
                Ok(p) => {
                    log::info!("plugin: loaded {} ({})", p.name(), path.display());
                    out.push(p);
                }
                Err(e) => log::warn!("plugin: failed to load {}: {e}", path.display()),
            }
        }
    }
    out
}

fn load_plugin(path: &PathBuf) -> anyhow::Result<LoadedPlugin> {
    // SAFETY: loading a user-provided .so is inherently unsafe; documented as such.
    let lib = unsafe { Library::new(path) }?;
    // SAFETY: symbol must be a valid PulsePluginV1.
    let symbol: Symbol<'_, *const PulsePluginV1> = unsafe { lib.get(b"pulse_plugin_v1\0") }?;
    let plugin = unsafe { **symbol };
    if plugin.version != 1 {
        anyhow::bail!("unsupported plugin version {}", plugin.version);
    }
    let name = if plugin.name.is_null() {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    } else {
        // SAFETY: plugin promises a valid NUL-terminated UTF-8 string.
        unsafe { CStr::from_ptr(plugin.name) }.to_string_lossy().into_owned()
    };
    let mut loaded = LoadedPlugin {
        _lib: lib,
        name,
        plugin,
        ctx: PluginCtx {
            user: std::ptr::null_mut(),
            get_config_f32: None,
            set_config_f32: None,
            log: None,
            get_band: None,
            get_time_hms: None,
        },
        enabled: true,
    };
    // call on_load
    if let Some(f) = loaded.plugin.on_load {
        // SAFETY: plugin promises a valid ctx.
        unsafe { f(&mut loaded.ctx) };
    }
    Ok(loaded)
}

impl LoadedPlugin {
    pub(crate) fn ctx_ptr(&self) -> *mut PluginCtx {
        &self.ctx as *const PluginCtx as *mut PluginCtx
    }

    /// Set the host callback table (called once by the host each frame before updates).
    pub fn set_ctx(&mut self, ctx: PluginCtx) {
        self.ctx = ctx;
    }

    /// Point the host callbacks at live state (must be called before call_update/call_transform).
    pub fn bind_state(&self, bands: &[f32; 128], cfg: *const crate::config::Config) {
        BANDS_PTR.with(|b| b.set(bands.as_ptr()));
        CFG_PTR.with(|c| c.set(cfg as *const c_void));
    }

    pub fn call_update(&self, dt: f32) {
        if let Some(f) = self.plugin.on_update {
            // SAFETY: plugin promises valid ctx.
            unsafe { f(self.ctx_ptr(), dt) };
        }
    }

    pub fn call_render(&self, req: &mut RenderRequest) {
        if let Some(f) = self.plugin.render_texture {
            // SAFETY: host provides a valid RenderRequest with a valid buffer.
            unsafe { f(self.ctx_ptr(), req) };
        }
    }

    pub fn call_transform(&self, input: &[f32; 128]) -> [f32; 128] {
        let mut out = *input;
        if let Some(f) = self.plugin.transform_bands {
            // SAFETY: both slices are 128 f32.
            unsafe { f(self.ctx_ptr(), input.as_ptr(), out.as_mut_ptr()) };
        }
        out
    }

    pub fn unload(&mut self) {
        if let Some(f) = self.plugin.on_unload {
            // SAFETY: plugin promises valid ctx.
            unsafe { f(self.ctx_ptr()) };
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

// Per-call host state: current bands pointer (set before plugin calls).
thread_local! {
    static BANDS_PTR: std::cell::Cell<*const f32> = std::cell::Cell::new(std::ptr::null());
    static CFG_PTR: std::cell::Cell<*const c_void> = std::cell::Cell::new(std::ptr::null());
}

/// Host callbacks so plugins can poke at the live Config / bands.
pub struct HostBridge<'a> {
    pub cfg: &'a mut crate::config::Config,
    pub bands: &'a [f32; 128],
    pub log_cb: fn(&str),
    pub now_hms: (i32, i32, i32),
}

impl<'a> HostBridge<'a> {
    pub fn make_ctx(&mut self) -> PluginCtx {
        // We need raw pointers into cfg/bands; safe because the host only calls
        // plugins synchronously on the main thread.
        let cfg_ptr = self.cfg as *mut crate::config::Config as *mut c_void;
        let bands_ptr = self.bands.as_ptr() as *const f32 as *mut c_void;
        PluginCtx {
            user: std::ptr::null_mut(),
            get_config_f32: Some(host_get_config),
            set_config_f32: Some(host_set_config),
            get_band: Some(host_get_band),
            log: Some(host_log),
            get_time_hms: Some(host_time),
        }
    }
}

extern "C" fn host_get_config(_user: *mut c_void, key: *const c_char) -> f32 {
    if key.is_null() {
        return 0.0;
    }
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    CFG_PTR.with(|c| {
        let p = c.get();
        if p.is_null() {
            return 0.0;
        }
        // SAFETY: host guarantees valid cfg pointer during the call.
        let cfg = unsafe { &*(p as *const crate::config::Config) };
        match key.as_ref() {
            "baseRadius" => cfg.base_radius,
            "growth" => cfg.growth,
            "alpha" => cfg.alpha,
            "sensitivity" => cfg.sensitivity,
            "ringWidth" => cfg.ring_width,
            "haloStrength" => cfg.halo_strength,
            "haloSize" => cfg.halo_size,
            "decay" => cfg.decay,
            "smoothness" => cfg.smoothness,
            _ => 0.0,
        }
    })
}

extern "C" fn host_set_config(_user: *mut c_void, _key: *const c_char, _val: f32) {}

extern "C" fn host_get_band(_user: *mut c_void, idx: u32) -> f32 {
    BANDS_PTR.with(|b| {
        let p = b.get();
        if p.is_null() || idx >= 128 {
            0.0
        } else {
            // SAFETY: host guarantees the pointer is valid for 128 f32 during the call.
            unsafe { *p.add(idx as usize) }
        }
    })
}

extern "C" fn host_log(_user: *mut c_void, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    // SAFETY: plugin promises valid string.
    let s = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    log::info!("[plugin] {s}");
}

extern "C" fn host_time(_user: *mut c_void, out: *mut [i32; 3]) {
    if out.is_null() {
        return;
    }
    // SAFETY: host promises valid out.
    unsafe {
        let (h, m, s, _) = crate::main_now_hmsparts();
        (*out)[0] = h;
        (*out)[1] = m;
        (*out)[2] = s;
    }
}