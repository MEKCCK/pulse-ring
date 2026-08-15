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
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    child: Option<Child>,
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

/// Locate the Electron helper: prefer a system `electron`, else `node` with the
/// electron npm package resolved from the repo's electron-wallpaper dir.
fn electron_binary() -> Option<std::path::PathBuf> {
    for c in ["electron"] {
        if let Ok(out) = std::process::Command::new(c).arg("--version").output() {
            if out.status.success() {
                return Some(std::path::PathBuf::from(c));
            }
        }
    }
    None
}

/// Start rendering `html_path` at `width`x`height` via Electron offscreen.
pub fn start_web_wallpaper(html_path: &str, width: u32, height: u32) -> Result<WebWallpaperPlayer, String> {
    let electron = electron_binary().ok_or("electron not found (install the 'electron' package)".to_string())?;
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn electron failed: {e}"))?;

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
            loop {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                if pending.is_none() {
                    // Read the 8-byte header.
                    let mut hdr = [0u8; 8];
                    let mut got = 0;
                    while got < 8 {
                        match reader.read(&mut hdr[got..]) {
                            Ok(0) => return,
                            Ok(n) => got += n,
                            Err(_) => return,
                        }
                    }
                    let w = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                    let h = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
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
                if tx.send(WebFrame { rgba: buf.clone(), width: w, height: h }).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(WebWallpaperPlayer {
        rx,
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
