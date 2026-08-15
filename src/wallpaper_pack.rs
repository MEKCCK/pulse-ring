//! Wallpaper packaging: a wallpaper is a FOLDER containing a `project.json` manifest
//! plus its resources (HTML/JS/CSS, video, images). pulse-ring resolves a folder path
//! to a spec and loads the right type, passing manifest params to web wallpapers.

use serde::Deserialize;

/// Manifest of a packaged wallpaper (`project.json`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WallpaperSpec {
    /// "web" | "video" | "image" (defaults: web).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Resource file relative to the folder (defaults: index.html / video.mp4 / image.jpg).
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Whether the wallpaper follows the music (web wallpapers get the audio API).
    #[serde(default)]
    pub audio: Option<bool>,
    /// Arbitrary params; forwarded to the web page via `window.pulseRing.onConfig`.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Target resolution "WxH" (optional; defaults to the configured web size).
    #[serde(default)]
    pub resolution: Option<String>,
}

/// A resolved wallpaper: the concrete resource file plus the spec.
pub struct ResolvedWallpaper {
    pub file: String,
    pub spec: WallpaperSpec,
    /// Serialized params JSON (for the web page), or "{}".
    pub params_json: String,
}

/// If `path` is a directory containing `project.json`, resolve it to a wallpaper
/// spec + resource file. Returns None when it's not a packaged wallpaper.
pub fn resolve_pack(path: &str) -> Option<ResolvedWallpaper> {
    let p = std::path::Path::new(path);
    if !p.is_dir() {
        return None;
    }
    let manifest = p.join("project.json");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let spec: WallpaperSpec = serde_json::from_str(&text).ok()?;
    let default_file = match spec.kind.as_str() {
        "video" => "video.mp4",
        "image" => "image.jpg",
        _ => "index.html",
    };
    let file = spec
        .file
        .clone()
        .unwrap_or_else(|| default_file.to_string());
    let file_path = p.join(&file);
    if !file_path.is_file() {
        return None;
    }
    let params_json = spec
        .params
        .clone()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());
    Some(ResolvedWallpaper {
        file: file_path.to_string_lossy().to_string(),
        spec,
        params_json,
    })
}

/// Resolution from the spec "WxH" string (or None).
pub fn spec_resolution(res: &Option<String>) -> Option<(u32, u32)> {
    let r = res.as_deref()?;
    let (w, h) = r.split_once('x')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    Some((w.max(1), h.max(1)))
}
