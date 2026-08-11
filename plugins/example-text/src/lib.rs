//! 示例插件：演示 pulse-ring 插件 API。
//!
//! 编译后把 `libpulse_ring_text_plugin.so` 复制到
//! `~/.config/pulse-ring/plugins/` 即可加载。
//!
//! 插件可以自己实现任何东西（如用 ab_glyph / cairo 渲染中文字体，
//! 通过 transform_bands 或后续扩展的渲染回调接入）。

use std::ffi::{c_char, c_void};

unsafe impl Sync for PulsePluginV1 {}

#[repr(C)]
pub struct PulsePluginV1 {
    pub name: *const c_char,
    pub version: u32,
    pub on_load: Option<extern "C" fn(ctx: *mut PluginCtx)>,
    pub on_update: Option<extern "C" fn(ctx: *mut PluginCtx, dt: f32)>,
    pub transform_bands: Option<extern "C" fn(ctx: *mut PluginCtx, in_bands: *const f32, out_bands: *mut f32)>,
    pub render_texture: Option<extern "C" fn(ctx: *mut PluginCtx, req: *mut RenderRequest)>,
    pub on_unload: Option<extern "C" fn(ctx: *mut PluginCtx)>,
}

#[repr(C)]
pub struct RenderRequest {
    pub slot: u32,
    pub buf_len: usize,
    pub buf: *mut u8,
    pub update: bool,
    pub width: u32,
    pub height: u32,
    pub screen_w: u32,
    pub screen_h: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginCtx {
    pub user: *mut c_void,
    pub get_config_f32: Option<extern "C" fn(user: *mut c_void, key: *const c_char) -> f32>,
    pub set_config_f32: Option<extern "C" fn(user: *mut c_void, key: *const c_char, val: f32)>,
    pub get_band: Option<extern "C" fn(user: *mut c_void, idx: u32) -> f32>,
    pub log: Option<extern "C" fn(user: *mut c_void, msg: *const c_char)>,
    pub get_time_hms: Option<extern "C" fn(user: *mut c_void, out: *mut [i32; 3])>,
}

/// 插件渲染回调：往 req.buf 画 RGBA（插件可以在这里用自己的字体库渲染中文）。
/// 演示动态内容：圆环半径随低频能量变化（get_band 回调读取频段）。
extern "C" fn render_texture(ctx: *mut PluginCtx, req: *mut RenderRequest) {
    let req = unsafe { &mut *req };
    let w = 256u32;
    let h = 256u32;
    if req.buf_len < (w * h * 4) as usize {
        return;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(req.buf, (w * h * 4) as usize) };
    let c = unsafe { &*ctx };

    // 读取低频能量（bands 0..16 平均）驱动圆环半径
    let mut bass = 0.0f32;
    if let Some(bf) = c.get_band {
        for i in 0..16 {
            bass += bf(c.user, i);
        }
        bass /= 16.0;
    }
    let ring_r = (w as f32 * 0.30) + bass * (w as f32 * 0.15);

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let o = ((y * w + x) * 4) as usize;
            // 渐变圆盘
            let t = (d / (w as f32 / 2.0)).clamp(0.0, 1.0);
            let mut r = (255.0 * (1.0 - t)) as u8;
            let mut g = (100.0 + 100.0 * (1.0 - t)) as u8;
            let mut b = 255u8;
            let mut a = 255u8;
            // 随低频膨胀的白色圆环
            if d > ring_r - 2.0 && d < ring_r + 2.0 {
                r = 255; g = 255; b = 255;
            } else if d > ring_r + 2.0 {
                a = 0;
            }
            buf[o] = r;
            buf[o + 1] = g;
            buf[o + 2] = b;
            buf[o + 3] = a;
        }
    }
    req.width = w;
    req.height = h;
    req.update = true;
}

static mut CTX: Option<PluginCtx> = None;

unsafe fn ctx() -> &'static PluginCtx {
    CTX.as_ref().expect("ctx not set")
}

extern "C" fn on_load(ctx: *mut PluginCtx) {
    // 只保存 ctx，不调用回调（on_load 时 host 可能还没设置回调表）
    unsafe { CTX = Some(*ctx) };
}

extern "C" fn on_update(ctx: *mut PluginCtx, _dt: f32) {
    let c = unsafe { &*ctx };
    // 演示：读取配置 + 时间，不修改任何动态值（避免干扰 Lua/主逻辑）
    let _g = c.get_config_f32.map_or(0.0, |f| f(c.user, c"growth".as_ptr()));
    let mut hms = [0i32; 3];
    if let Some(tf) = c.get_time_hms {
        tf(c.user, &mut hms);
    }
    if let Some(lf) = c.log {
        if hms[2] % 10 == 0 {
            lf(c.user, c"plugin-update".as_ptr());
        }
    }
}

extern "C" fn transform_bands(_ctx: *mut PluginCtx, input: *const f32, out: *mut f32) {
    // 演示：低频增益 1.2x，高频 0.8x（平滑，无抖动）
    for i in 0..128 {
        unsafe {
            let v = *input.add(i);
            let gain = if i < 32 { 1.2 } else if i >= 96 { 0.8 } else { 1.0 };
            *out.add(i) = v * gain;
        }
    }
}

fn get_cfg(key: &str) -> f32 {
    let c = unsafe { ctx() };
    let k = std::ffi::CString::new(key).unwrap();
    (c.get_config_f32.unwrap())(c.user, k.as_ptr())
}

fn set_cfg(key: &str, val: f32) {
    let c = unsafe { ctx() };
    let k = std::ffi::CString::new(key).unwrap();
    (c.set_config_f32.unwrap())(c.user, k.as_ptr(), val);
}

fn time(out: &mut [i32; 3]) {
    let c = unsafe { ctx() };
    (c.get_time_hms.unwrap())(c.user, out as *mut [i32; 3]);
}

fn log_msg(msg: &str) {
    let c = unsafe { ctx() };
    let m = std::ffi::CString::new(msg).unwrap();
    (c.log.unwrap())(c.user, m.as_ptr());
}

#[no_mangle]
pub static pulse_plugin_v1: PulsePluginV1 = PulsePluginV1 {
    name: c"example-text".as_ptr(),
    version: 1,
    on_load: Some(on_load),
    on_update: Some(on_update),
    transform_bands: Some(transform_bands),
    render_texture: Some(render_texture),
    on_unload: None,
};
