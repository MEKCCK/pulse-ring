//! Video wallpaper via GStreamer (borrowing Kaleidux's playbin + appsink approach).
//!
//! A `playbin` pipeline (video+audio — the audio plays through the default sink so
//! the ring/bars keep reacting) pushes decoded RGBA frames into an appsink; a
//! callback forwards each frame through a bounded channel. The render loop uploads
//! the newest frame into the wallpaper texture every frame.

use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gst::prelude::*;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

/// One decoded RGBA frame.
#[derive(Clone)]
pub struct VideoFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Start playing `path` as a wallpaper video. Returns a receiver of RGBA frames.
/// The video loops; audio is routed to the default sink (so the visualisation reacts).
pub fn start_video_wallpaper(path: &str) -> Result<Receiver<VideoFrame>, String> {
    gst::init().map_err(|e| format!("gstreamer init failed: {e}"))?;

    let (frame_tx, frame_rx) = channel::<VideoFrame>();
    let pipeline: Arc<gst::Element> = Arc::new(
        gst::ElementFactory::make("playbin")
            .name("playbin")
            .build()
            .map_err(|e| format!("playbin create failed: {e}"))?,
    );

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
    pipeline.set_property("uri", &uri);
    pipeline.set_property_from_str("flags", "video+audio");

    let appsink: gst_app::AppSink = gst::ElementFactory::make("appsink")
        .name("video-sink")
        .build()
        .map_err(|e| e.to_string())?
        .downcast()
        .map_err(|_| "appsink downcast failed".to_string())?;

    // RGBA, drop late frames, 1-buffer queue — minimal latency, no accumulation.
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

    // Bus watcher: log errors, and loop the video by seeking back to 0 on EOS.
    let bus = pipeline.bus().expect("bus");
    let loop_pipeline = pipeline.clone();
    std::thread::Builder::new()
        .name("pulse-ring-video-bus".into())
        .spawn(move || loop {
            use gst::MessageView;
            match bus.timed_pop(gst::ClockTime::from_seconds(1)) {
                Some(msg) => match msg.view() {
                    MessageView::Eos(_) => {
                        // Loop: seek back to the start.
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
        })
        .ok();

    // Keep pipeline + appsink alive for the process lifetime.
    let _ = pipeline.set_state(gst::State::Playing);
    std::mem::forget(Arc::new((pipeline, appsink)));

    Ok(frame_rx)
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
