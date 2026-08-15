//! Video wallpaper via GStreamer (borrowing Kaleidux's playbin + appsink approach).
//!
//! A `playbin` pipeline (video+audio — the audio plays through the default sink so
//! the ring/bars keep reacting) pushes decoded RGBA frames into an appsink; a
//! callback forwards each frame through a bounded channel. The render loop uploads
//! the newest frame into the wallpaper texture every frame.
//!
//! The pipeline is owned by a dedicated thread; dropping the `VideoPlayer` stops the
//! pipeline cleanly (needed when the rotation moves on to the next wallpaper).

use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gst::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// One decoded RGBA frame.
#[derive(Clone)]
pub struct VideoFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A running video wallpaper. Dropping it stops the pipeline (and the audio).
pub struct VideoPlayer {
    pub rx: Receiver<VideoFrame>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start playing `path` as a wallpaper video. Loops. When `audio` is true the sound
/// routes to the default sink (so the visualisation reacts to it); when false the
/// video plays silently.
pub fn start_video_wallpaper(path: &str, audio: bool) -> Result<VideoPlayer, String> {
    gst::init().map_err(|e| format!("gstreamer init failed: {e}"))?;

    let uri = if path.contains("://") {
        path.to_string()
    } else {
        let abs = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            format!(
                "{}/{}",
                std::env::current_dir().map_err(|e| e.to_string())?.display(),
                path
            )
        };
        format!("file://{abs}")
    };

    let (frame_tx, frame_rx) = channel::<VideoFrame>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let handle = std::thread::Builder::new()
        .name("pulse-ring-video".into())
        .spawn(move || {
            let pipeline: gst::Element = match gst::ElementFactory::make("playbin")
                .name("playbin")
                .build()
            {
                Ok(p) => p,
                Err(_) => return,
            };
            pipeline.set_property("uri", &uri);
            pipeline.set_property_from_str("flags", if audio { "video+audio" } else { "video" });

            let appsink: gst_app::AppSink = match gst::ElementFactory::make("appsink")
                .name("video-sink")
                .build()
                .map(|e| e.downcast())
            {
                Ok(Ok(s)) => s,
                _ => return,
            };

            let caps = gst::Caps::builder("video/x-raw").field("format", "RGBA").build();
            appsink.set_caps(Some(&caps));
            appsink.set_sync(true);
            appsink.set_drop(true);
            appsink.set_max_buffers(1);

            let tx = frame_tx.clone();
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = match sink.pull_sample() {
                            Ok(s) => s,
                            Err(_) => return Err(gst::FlowError::Error),
                        };
                        let buffer = match sample.buffer() {
                            Some(b) => b.to_owned(),
                            None => return Err(gst::FlowError::Error),
                        };
                        let caps = match sample.caps() {
                            Some(c) => c,
                            None => return Err(gst::FlowError::Error),
                        };
                        let info = match gst_video::VideoInfo::from_caps(&caps) {
                            Ok(i) => i,
                            Err(_) => return Err(gst::FlowError::Error),
                        };
                        let map = match buffer.map_readable() {
                            Ok(m) => m,
                            Err(_) => return Err(gst::FlowError::Error),
                        };
                        let frame = VideoFrame {
                            rgba: map.as_slice().to_vec(),
                            width: info.width(),
                            height: info.height(),
                        };
                        if tx.send(frame).is_err() {
                            return Err(gst::FlowError::Eos);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
            pipeline.set_property("video-sink", &appsink);

            // Bus watcher: loop on EOS, log errors.
            let bus = pipeline.bus().expect("bus");
            let loop_pipeline = pipeline.clone();
            let bus_stop = stop_thread.clone();
            let bus_thread = std::thread::Builder::new()
                .name("pulse-ring-video-bus".into())
                .spawn(move || {
                    use gst::MessageView;
                    while !bus_stop.load(Ordering::SeqCst) {
                        match bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                            Some(msg) => match msg.view() {
                                MessageView::Eos(_) => {
                                    let _ = loop_pipeline.seek_simple(
                                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                        gst::ClockTime::ZERO,
                                    );
                                }
                                MessageView::Error(err) => {
                                    log::error!(
                                        "video wallpaper error: {}",
                                        err.error().message().to_string()
                                    );
                                    break;
                                }
                                MessageView::Warning(w) => log::warn!(
                                    "video wallpaper warning: {}",
                                    w.error().message().to_string()
                                ),
                                _ => {}
                            },
                            None => {}
                        }
                    }
                })
                .ok();

            let _ = pipeline.set_state(gst::State::Playing);
            // Park until stopped; the pipeline (and audio) lives in this thread.
            while !stop_thread.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = pipeline.set_state(gst::State::Null);
            if let Some(b) = bus_thread {
                let _ = b.join();
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(VideoPlayer {
        rx: frame_rx,
        stop,
        handle: Some(handle),
    })
}

/// Drain the video frame channel, returning the newest frame if any arrived.
pub fn drain_video(rx: &Receiver<VideoFrame>) -> Option<VideoFrame> {
    let mut newest = None;
    loop {
        match rx.try_recv() {
            Ok(f) => newest = Some(f),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    newest
}

/// True when a wallpaper path is a video file (by extension).
pub fn is_video_path(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "mp4" | "webm" | "mkv" | "mov" | "avi" | "gif" | "m4v")
}
