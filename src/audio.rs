use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Preferred sample rate / channel count for the capture stream. PipeWire's "pipewire" ALSA device
/// advertises absurd ranges (1 Hz..384000 Hz, 1..32 ch); we pin sane values so the FFT band mapping
/// is meaningful and the visual reacts to real audio rather than a 1 Hz probe.
const TARGET_RATE: u32 = 48000;
const TARGET_CHANNELS: u16 = 2;

/// Number of frequency bands (ring segments).
pub const NBANDS: usize = 128;
/// FFT window size.
const WINDOW: usize = 2048;

/// Start audio capture. Returns a channel producing smoothed magnitude arrays of length `NBANDS`
/// (values in 0.0..=1.0). Falls back to a silent synthetic pulse if no audio device is available.
///
/// `sensitivity` scales the input level (1.0 = default), `decay` is the per-frame fall-off
/// (0.0..1.0, higher = ring holds its height longer).
pub fn start_audio(sensitivity: f32, decay: f32) -> Receiver<[f32; NBANDS]> {
    match try_start_audio(sensitivity, decay) {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("audio capture failed ({e}); showing passive ring");
            silent_source()
        }
    }
}

fn try_start_audio(sensitivity: f32, decay: f32) -> anyhow::Result<Receiver<[f32; NBANDS]>> {
    // On PipeWire+ALSA, point cpal at the default sink's monitor so we react to real playback
    // rather than a microphone. We resolve the monitor node id with `pactl`/`pw-dump` lazily;
    // if that fails we fall back to whatever default input device cpal picks.
    ensure_pipewire_monitor_node();

    let host = cpal::default_host();
    let device = pick_device(&host)?;
    log::info!("audio device: {}", device.name().unwrap_or_default());

    let mut cfgs = device.supported_input_configs()?.collect::<Vec<_>>();
    if cfgs.is_empty() {
        anyhow::bail!("device has no supported input configs");
    }
    // Pick the config closest to our target: F32, 2 channels, 48 kHz.
    fn score(c: &cpal::SupportedStreamConfigRange) -> i64 {
        let fmt = if c.sample_format().is_float() { 0 } else { 100 };
        let ch = (c.channels() as i64 - TARGET_CHANNELS as i64).abs();
        let rate_lo = (c.min_sample_rate().0 as i64 - TARGET_RATE as i64).abs();
        let rate_hi = (c.max_sample_rate().0 as i64 - TARGET_RATE as i64).abs();
        fmt + ch * 4 + rate_lo.min(rate_hi) / 1000
    }
    cfgs.sort_by_key(|c| score(c));
    let supported = cfgs.into_iter().next().unwrap();
    let sample_format = supported.sample_format();
    let cfg = StreamConfig {
        channels: supported.channels().min(TARGET_CHANNELS.max(1)),
        sample_rate: cpal::SampleRate(
            supported.max_sample_rate().0.min(TARGET_RATE).max(supported.min_sample_rate().0),
        ),
        buffer_size: BufferSize::Fixed(1024),
    };
    log::info!(
        "audio stream: {} Hz, {} ch, {:?}, buf={:?}",
        cfg.sample_rate.0,
        cfg.channels,
        sample_format,
        cfg.buffer_size,
    );

    let (frame_tx, frame_rx) = bounded::<Vec<f32>>(8);
    let channels = cfg.channels as usize;

    let stream: Stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &cfg,
            move |data: &[f32], _| push_chunk(data, channels, &frame_tx),
            |e| log::error!("audio stream error: {e}"),
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            &cfg,
            move |data: &[i16], _| {
                let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                push_chunk(&f, channels, &frame_tx);
            },
            |e| log::error!("audio stream error: {e}"),
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            &cfg,
            move |data: &[u16], _| {
                let f: Vec<f32> = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect();
                push_chunk(&f, channels, &frame_tx);
            },
            |e| log::error!("audio stream error: {e}"),
            None,
        )?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    std::mem::forget(stream);

    let (mags_tx, mags_rx) = bounded::<[f32; NBANDS]>(4);
    std::thread::Builder::new()
        .name("pulse-ring-fft".into())
        .spawn(move || fft_loop(frame_rx, mags_tx, cfg.sample_rate.0, sensitivity, decay))?;

    Ok(mags_rx)
}

fn push_chunk(data: &[f32], channels: usize, frame_tx: &Sender<Vec<f32>>) {
    let mono = monoise(data, channels);
    let _ = frame_tx.try_send(mono);
}

fn monoise(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    let frames = data.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        let mut acc = 0.0f32;
        for c in 0..channels {
            acc += data[i * channels + c];
        }
        out.push(acc / channels as f32);
    }
    out
}

/// If we're running under PipeWire (ALSA `pipewire` device present), try to set `PIPEWIRE_NODE`
/// to the id of the default sink's monitor. That makes cpal's ALSA backend tap the running audio
/// output instead of a microphone. Safe to fail — caller falls back to monitor-named devices.
fn ensure_pipewire_monitor_node() {
    use std::process::Command;
    let sink = match Command::new("pactl").arg("get-default-sink").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return,
    };
    if sink.is_empty() {
        return;
    }
    // Resolve the matching source object path whose name ends with ".monitor" for this sink.
    let out = match Command::new("pactl").args(["list", "short", "sources"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return,
    };
    let monitor_name = format!("{sink}.monitor");
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let id = it.next();
        let name = it.next();
        if let (Some(id), Some(name)) = (id, name) {
            if name == monitor_name || name.ends_with(".monitor") {
                if let Ok(n) = id.parse::<u32>() {
                    // SAFETY: this runs single-threaded at startup before any other thread that
                    // might read the environment is spawned.
                    unsafe {
                        std::env::set_var("PIPEWIRE_NODE", n.to_string());
                        std::env::set_var("PIPEWIRE_LATENCY", "1024/48000");
                    }
                    log::info!("set PIPEWIRE_NODE={n} ({name})");
                    return;
                }
            }
        }
    }
}

fn pick_device(host: &cpal::Host) -> anyhow::Result<cpal::Device> {
    // Prefer a loopback/monitor device (PipeWire exposes these; ALSA "hw:..." monitors too).
    if let Ok(devices) = host.input_devices() {
        let mut monitors: Vec<cpal::Device> = devices
            .filter(|d| {
                d.name()
                    .map(|n| n.to_lowercase().contains("monitor"))
                    .unwrap_or(false)
            })
            .collect();
        if let Some(d) = monitors.pop() {
            return Ok(d);
        }
    }
    if let Some(d) = host.default_input_device() {
        return Ok(d);
    }
    anyhow::bail!("no input audio device available")
}

fn fft_loop(
    frame_rx: Receiver<Vec<f32>>,
    mags_tx: Sender<[f32; NBANDS]>,
    sample_rate: u32,
    sensitivity: f32,
    decay: f32,
) {
    let mut planner = RealFftPlanner::<f32>::new();
    let r2c: Arc<dyn RealToComplex<f32>> = planner.plan_fft_forward(WINDOW);
    let window: Vec<f32> = (0..WINDOW)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / WINDOW as f32).cos()))
        .collect();
    let mut ring: Vec<f32> = vec![0.0; WINDOW];
    let mut pos = 0usize;
    let mut input = r2c.make_input_vec();
    let mut spectrum = r2c.make_output_vec();
    let band_edges = compute_band_edges(sample_rate as f64);
    let mut prev = [0.0f32; NBANDS];

    while let Ok(chunk) = frame_rx.recv() {
        for &s in &chunk {
            ring[pos] = s;
            pos = (pos + 1) % WINDOW;
            if pos == 0 {
                for i in 0..WINDOW {
                    input[i] = ring[i] * window[i];
                }
                if r2c.process(&mut input, &mut spectrum).is_ok() {
                    let bands = aggregate(&spectrum, &band_edges, &prev, sensitivity, decay);
                    prev = bands;
                    let _ = mags_tx.try_send(bands);
                }
            }
        }
    }
}

/// Build the (lo, hi) frequency-bin edges for `NBANDS` log-spaced bands.
fn compute_band_edges(sample_rate: f64) -> Vec<(usize, usize)> {
    let len = WINDOW as f64 / 2.0;
    let lo = 40.0_f64;
    let hi = (sample_rate * 0.45).min(16000.0).max(200.0);
    let mut edges = Vec::with_capacity(NBANDS + 1);
    for i in 0..=NBANDS {
        let t = i as f64 / NBANDS as f64;
        let f = lo * (hi / lo).powf(t);
        edges.push((f * len / sample_rate).round() as usize);
    }
    (0..NBANDS)
        .map(|i| (edges[i], edges[i + 1]))
        .collect()
}

fn aggregate(
    spectrum: &[Complex<f32>],
    band_edges: &[(usize, usize)],
    prev: &[f32],
    sensitivity: f32,
    decay: f32,
) -> [f32; NBANDS] {
    let mut out = [0.0f32; NBANDS];
    for (i, &(lo, hi)) in band_edges.iter().take(NBANDS).enumerate() {
        let lo = lo.max(1);
        let hi = hi.max(lo + 1);
        let mut sum = 0.0;
        let mut max = 0.0;
        let mut n = 0;
        for k in lo..hi.min(spectrum.len()) {
            let mag = spectrum[k].norm();
            sum += mag;
            if mag > max {
                max = mag;
            }
            n += 1;
        }
        let raw = if n > 0 { max * 0.6 + sum / n as f32 * 0.4 } else { 0.0 };
        let v = (raw / 100.0).powf(0.55) / 12.0;
        let v = (v * sensitivity).min(1.0);
        // rise fast, fall slowly — pleasant visual decay
        let p = prev[i];
        let v = if v > p { v } else { p * decay + v * (1.0 - decay) };
        out[i] = v;
    }
    out
}

/// Synthetic idle source so the visual stays alive even without audio.
fn silent_source() -> Receiver<[f32; NBANDS]> {
    let (tx, rx) = bounded::<[f32; NBANDS]>(4);
    std::thread::spawn(move || loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let mut bands = [0.0f32; NBANDS];
        let pulse = 0.12 + 0.10 * (now * 2.0 * std::f64::consts::PI / 3.0).sin() as f32;
        for i in 0..NBANDS {
            let ang = i as f32 * 2.0 * std::f32::consts::PI / NBANDS as f32;
            bands[i] = 0.5 * pulse * (1.0 + 0.6 * (ang * 2.0).sin()).max(0.0);
        }
        if tx.send(bands).is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    });
    rx
}