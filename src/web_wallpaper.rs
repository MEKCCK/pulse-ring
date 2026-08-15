//! Web wallpaper via Electron offscreen rendering.
//!
//! Spawns the bundled Electron helper (`electron-wallpaper/main.js`) which renders an
//! HTML wallpaper offscreen and streams RGBA frames on stdout:
//! `[u32le w][u32le h][w*h*4 RGBA]`. This thread parses the stream and forwards each
//! frame through a channel — the render loop uploads them like video wallpaper frames.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

pub struct WebFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct WebWallpaperPlayer {
    pub rx: Receiver<WebFrame>,
    /// Child stdin: we push audio frames (and the manifest config) to the page.
    stdin: Option<std::process::ChildStdin>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    child: Option<Child>,
}

impl WebWallpaperPlayer {
    /// Send one audio frame: tag 0x00 + 128 f32 bands + 1 f32 energy (1+516 bytes, LE).
    /// The tag keeps the stream in sync with the Electron helper's parser.
    pub fn send_audio(&mut self, bands: &[f32; 128], energy: f32) {
        let Some(stdin) = self.stdin.as_mut() else { return };
        use std::io::Write;
        let mut buf = Vec::with_capacity(517);
        buf.push(0u8); // tag: audio frame
        for b in bands.iter().take(128) {
            buf.extend_from_slice(&b.to_le_bytes());
        }
        buf.extend_from_slice(&energy.to_le_bytes());
        let _ = stdin.write_all(&buf);
    }

    /// Send the wallpaper manifest (JSON) to the page via `window.pulseRing.onConfig`.
    pub fn send_config(&mut self, config_json: &str) {
        let Some(stdin) = self.stdin.as_mut() else { return };
        use std::io::Write;
        let bytes = config_json.as_bytes();
        let mut buf = Vec::with_capacity(4 + bytes.len());
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
        // Frame type tag: 0 = audio, 1 = config.
        let _ = stdin.write_all(&[1u8]);
        let _ = stdin.write_all(&buf);
    }
}

impl Drop for WebWallpaperPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Use the Electron version pinned by this project. A system-wide Electron can
/// have a different Chromium/Node ABI and must never affect wallpaper behavior.
fn electron_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("electron-wallpaper/node_modules/.bin/electron")
}

/// Start rendering `html_path` at `width`x`height` via Electron offscreen.
pub fn start_web_wallpaper(html_path: &str, width: u32, height: u32) -> Result<WebWallpaperPlayer, String> {
    let electron = electron_binary();
    if !electron.is_file() {
        return Err(format!(
            "project Electron not found at {} (run npm install in electron-wallpaper)",
            electron.display()
        ));
    }
    let helper = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/electron-wallpaper/main.js"
    );
    let abs_html = if std::path::Path::new(html_path).is_absolute() {
        html_path.to_string()
    } else {
        format!(
            "{}/{}",
            std::env::current_dir().map_err(|e| e.to_string())?.display(),
            html_path
        )
    };

    let mut child = Command::new(&electron)
        .args([helper, &abs_html, &width.to_string(), &height.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn electron failed: {e}"))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = channel::<WebFrame>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let handle = std::thread::Builder::new()
        .name("pulse-ring-web".into())
        .spawn(move || {
            let mut reader = stdout;
            let mut buf = Vec::new();
            let mut pending: Option<(u32, u32)> = None;
            let mut frames: u64 = 0;
            loop {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                if pending.is_none() {
                    // Read the 8-byte header. If the stream is desynced (Electron/
                    // Chromium printed junk on stdout at startup), resync by
                    // scanning for a plausible header instead of dying.
                    let mut hdr = [0u8; 8];
                    let mut got = 0;
                    while got < 8 {
                        match reader.read(&mut hdr[got..]) {
                            Ok(0) => return,
                            Ok(n) => got += n,
                            Err(_) => return,
                        }
                    }
                    let mut w = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                    let mut h = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
                    while !(w > 0 && w <= 8192 && h > 0 && h <= 8192 && (w * h * 4) < 256 * 1024 * 1024) {
                        // shift left by one byte and read one more
                        hdr.copy_within(1.., 0);
                        match reader.read(&mut hdr[7..]) {
                            Ok(0) => return,
                            Ok(n) if n > 0 => {}
                            Err(_) => return,
                            _ => {}
                        }
                        w = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                        h = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
                    }
                    if got > 8 || w != 960 {
                        log::info!("web reader: resynced to {w}x{h} (discarded {} junk bytes)", got.saturating_sub(8));
                    }
                    pending = Some((w, h));
                }
                let (w, h) = pending.unwrap();
                let len = (w as usize) * (h as usize) * 4;
                buf.clear();
                buf.resize(len, 0);
                let mut got = 0;
                while got < len {
                    match reader.read(&mut buf[got..]) {
                        Ok(0) => return,
                        Ok(n) => got += n,
                        Err(_) => return,
                    }
                }
                pending = None;
                frames += 1;
                if frames % 30 == 0 || frames <= 3 {
                    log::info!("web reader: frame #{frames} {w}x{h} ok");
                }
                if tx.send(WebFrame { rgba: buf.clone(), width: w, height: h }).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(WebWallpaperPlayer {
        rx,
        stdin,
        stop,
        handle: Some(handle),
        child: Some(child),
    })
}

/// Drain the newest web wallpaper frame (drop stale ones).
pub fn drain_web(rx: &Receiver<WebFrame>) -> Option<WebFrame> {
    let mut newest = None;
    loop {
        match rx.try_recv() {
            Ok(f) => newest = Some(f),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    newest
}

/// True when a wallpaper path is an HTML file (web wallpaper).
pub fn is_html_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".html") || path.to_ascii_lowercase().ends_with(".htm")
}
