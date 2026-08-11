//! QML-configuration parser for pulse-ring.
//!
//! A recursive-descent parser for the literal subset of QML used as configuration:
//!   `Type { key: value, key2: [ Item { a: 1 }, ... ] }`
//! Values may be numbers, strings, bools, hex colours, arrays and nested objects.
//! Unknown keys are ignored, so the file may contain extra QML structure.

use std::path::Path;

/// A placed widget on the wallpaper layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetType {
    /// The music-reactive ring (uses the global style settings).
    Ring,
    /// A static image (PNG), `source` required.
    Image,
    /// A clock showing the current time.
    Clock,
    /// A vertical spectrum bar visualiser.
    Bars,
    /// The current music album cover (MPRIS), with a border and beat-scaling.
    Cover,
    /// An analog (hand-dial) clock.
    Analog,
    /// A plugin-rendered texture (the named Rust plugin draws into it each frame).
    Plugin,
}

#[derive(Debug, Clone)]
pub struct WidgetConfig {
    pub widget_type: WidgetType,
    /// Position as a fraction of the screen (0.5 = centre).
    pub x: f32,
    pub y: f32,
    /// Scale multiplier (1.0 = normal size).
    pub size: f32,
    /// Opacity 0..1.
    pub alpha: f32,
    /// Rotation in degrees.
    pub rotate: f32,
    /// Image file path (image widgets).
    pub source: Option<String>,
    /// Text colour (clock widgets), RGBA.
    pub color: [f32; 4],
    /// Font size in pixels (clock widgets).
    pub font_size: f32,
    // ---- ring widget style (independent per widget) ----
    pub shape: Shape,
    pub corners: f32,
    pub spikiness: f32,
    pub color_mode: ColorMode,
    pub colors: Vec<[f32; 4]>,
    pub ring_width: f32,
    pub base_radius: f32,
    pub growth: f32,
    pub halo_strength: f32,
    pub halo_size: f32,
    pub dash_count: f32,
    pub dash_ratio: f32,
    pub ring_alpha: f32,
    /// Show mid/inner rings on this ring widget too.
    pub with_rings: bool,
    /// Which frequency band this ring responds to.
    pub band_mode: BandMode,
    // ---- bars widget ----
    pub bar_count: f32,
    pub bar_height: f32,
    pub bar_gap: f32,
    pub bar_mirror: bool,
    // ---- cover widget ----
    /// Border width (fraction of the shorter edge).
    pub border_width: f32,
    /// Beat-scaling amplitude (0 = static).
    pub cover_growth: f32,
    /// Plugin name for `type: "plugin"` widgets.
    pub plugin: Option<String>,
    // ---- analog clock ----
    /// Number of hour ticks (12 or 24).
    pub tick_count: f32,
    /// Dial border width (fraction of the shorter edge).
    pub dial_border: f32,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            widget_type: WidgetType::Ring,
            x: 0.5,
            y: 0.5,
            size: 1.0,
            alpha: 1.0,
            rotate: 0.0,
            source: None,
            color: [1.0, 1.0, 1.0, 1.0],
            font_size: 48.0,
            shape: Shape::Ring,
            corners: 5.0,
            spikiness: 0.35,
            color_mode: ColorMode::Hue,
            colors: vec![],
            ring_width: 6.0,
            base_radius: 0.13,
            growth: 0.18,
            halo_strength: 0.18,
            halo_size: 0.12,
            dash_count: 0.0,
            dash_ratio: 0.75,
            ring_alpha: 1.0,
            with_rings: false,
            band_mode: BandMode::Full,
            bar_count: 32.0,
            bar_height: 0.15,
            bar_gap: 0.25,
            bar_mirror: false,
            border_width: 0.004,
            cover_growth: 0.08,
            plugin: None,
            tick_count: 12.0,
            dial_border: 0.004,
        }
    }
}

/// Which part of the spectrum a ring widget reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandMode {
    /// Full spectrum, angle-mapped (default).
    Full,
    /// Low frequencies (bass).
    Bass,
    /// Mid frequencies.
    Mid,
    /// High frequencies (treble).
    Treble,
    /// Overall energy (uniform breathing, "power meter" style).
    Energy,
}

/// The shape of the music-reactive outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Ring,
    Square,
    Diamond,
    Hexagon,
    Triangle,
    Star,
    Flower,
}

/// How the ring's colour is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Hue,
    Solid,
    Gradient,
}

/// Startup reveal animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnEffect {
    /// Expand from the centre (scales the whole shape).
    Expand,
    /// Zoom (same as expand for now; kept for forward compatibility).
    Zoom,
    /// Magic-circle: rings unfold in a delayed wave with rotation + a travelling light ring.
    Magic,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnEase {
    OutCubic,
    OutBack,
    Elastic,
    Bounce,
}

/// Particle sprite shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticleShape {
    Circle,
    Square,
    Diamond,
    Star,
}

/// Particle motion model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticleMode {
    /// Particles fly out from their (x, y) origin along `angle` at `speed`, fading over `life`.
    Burst,
    /// Particles orbit the centre; (x, y) sets the orbit radius offset, angle the start, speed in deg/s.
    Orbit,
    /// Particles orbit on a ring band outside the outer ring (Saturn-style); x sets the orbit
    /// radius (fraction of the shorter edge), angle the start, speed the angular velocity.
    Ring,
    None,
}

#[derive(Debug, Clone)]
pub struct ParticleConfig {
    /// Position relative to the screen centre, as a fraction of the shorter edge.
    pub x: f32,
    pub y: f32,
    /// Initial angle in degrees (burst direction / orbit start).
    pub angle: f32,
    /// Speed: burst = edges/sec, orbit = deg/sec.
    pub speed: f32,
    /// Particle diameter as a fraction of the shorter edge.
    pub size: f32,
    pub color: [f32; 4],
    /// Lifetime in seconds (burst fade-out period).
    pub life: f32,
    /// Delay before the particle starts, seconds.
    pub delay: f32,
    /// Gravity in shorter-edge fractions per second^2 (negative = upward).
    pub gravity: f32,
    /// Linear drag, 0..1 per second (velocity damping).
    pub drag: f32,
    /// Fade-in time in seconds.
    pub fade_in: f32,
    /// End size as a fraction of the shorter edge (0 = shrink to nothing).
    pub size_end: f32,
    /// Twinkle amplitude 0..1 (alpha flicker).
    pub twinkle: f32,
    /// Wave amplitude as a fraction of the shorter edge (lateral wobble).
    pub wave: f32,
    /// Spin speed in deg/s (visible for non-circle sprites).
    pub spin_speed: f32,
}

impl Default for ParticleConfig {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            angle: 0.0,
            speed: 0.5,
            size: 0.01,
            color: [1.0, 1.0, 1.0, 1.0],
            life: 2.0,
            delay: 0.0,
            gravity: 0.0,
            drag: 0.0,
            fade_in: 0.0,
            size_end: 0.0,
            twinkle: 0.0,
            wave: 0.0,
            spin_speed: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    // ---- shape ----
    pub shape: Shape,
    pub corners: f32,
    pub spikiness: f32,
    pub rotate: f32,
    // ---- double ring ----
    pub inner_ring: bool,
    pub inner_radius: f32,
    pub inner_growth: f32,
    pub inner_width: f32,
    pub inner_color: [f32; 4],
    // ---- appearance extras ----
    /// Dash segments around the shape (0 = solid outline).
    pub dash_count: f32,
    /// Dash duty cycle 0..1 (1 = fully solid).
    pub dash_ratio: f32,
    /// Auto-rotation speed in deg/s (0 = static).
    pub auto_rotate: f32,
    /// Idle breathing amplitude 0..1 (gentle pulse when there is no audio).
    pub idle_breathe: f32,
    /// Inner ring opacity multiplier.
    pub inner_alpha: f32,
    // ---- saturn ring band ----
    /// Width of the continuous halo band outside the outer ring (fraction of shorter edge,
    /// 0 = disabled).
    pub saturn_band: f32,
    /// Band opacity 0..1.
    pub saturn_alpha: f32,
    /// Band stripe contrast 0..1 (concentric ring striations).
    pub saturn_stripes: f32,
    // ---- middle ring ----
    pub mid_ring: bool,
    /// Middle ring radius = baseRadius * this.
    pub mid_radius: f32,
    /// Middle ring growth with overall energy.
    pub mid_growth: f32,
    pub mid_width: f32,
    pub mid_color: [f32; 4],
    // ---- particles ----
    pub particle_shape: ParticleShape,
    // ---- lua ----
    /// Optional Lua script path; the script can transform bands, tweak config and widgets
    /// at runtime via `onUpdate`, `transformBands`, etc.
    pub lua_script: Option<String>,
    // ---- widgets ----
    /// Extra placed widgets (rings / images / clocks). The main ring is always widget[0].
    pub widgets: Vec<WidgetConfig>,
    // ---- spawn effect ----
    pub spawn_effect: SpawnEffect,
    pub spawn_duration: f32,
    pub spawn_ease: SpawnEase,
    /// Extra rotation (degrees) applied during the spawn animation (magic effect).
    pub spawn_rotate: f32,
    // ---- particles ----
    pub particle_mode: ParticleMode,
    pub particle_loop: bool,
    pub particles: Vec<ParticleConfig>,
    // ---- colour ----
    pub color_mode: ColorMode,
    pub colors: Vec<[f32; 4]>,
    pub ring_width: f32,
    pub base_radius: f32,
    pub growth: f32,
    pub halo_strength: f32,
    pub halo_size: f32,
    pub alpha: f32,
    // ---- audio ----
    pub sensitivity: f32,
    pub decay: f32,
    pub smoothness: f32,
    // ---- position ----
    pub x_offset: f32,
    pub y_offset: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shape: Shape::Ring,
            corners: 5.0,
            spikiness: 0.35,
            rotate: 0.0,
            inner_ring: true,
            inner_radius: 0.58,
            inner_growth: 0.08,
            inner_width: 5.0,
            inner_color: [0.918, 0.847, 1.0, 0.9], // MD3 PrimaryContainer #EADDFF
            dash_count: 0.0,
            dash_ratio: 0.8,
            auto_rotate: 0.0,
            idle_breathe: 0.0,
            inner_alpha: 1.0,
            mid_ring: true,
            mid_radius: 0.78,
            mid_growth: 0.10,
            mid_width: 3.5,
            mid_color: [0.576, 0.545, 0.60, 0.75], // MD3 Secondary #938F99
            saturn_band: 0.028,
            saturn_alpha: 0.30,
            saturn_stripes: 0.35,
            particle_shape: ParticleShape::Circle,
            lua_script: None,
            widgets: vec![],
            spawn_effect: SpawnEffect::Expand,
            spawn_duration: 1400.0,
            spawn_ease: SpawnEase::OutCubic,
            spawn_rotate: 0.0,
            particle_mode: ParticleMode::Burst,
            particle_loop: true,
            particles: vec![],
            color_mode: ColorMode::Gradient,
            colors: vec![
                [0.404, 0.314, 0.643, 1.0], // MD3 Primary #6750A4
                [0.490, 0.322, 0.376, 1.0], // MD3 Tertiary #7D5260
                [0.816, 0.737, 1.0, 1.0],   // MD3 PrimaryContainer 亮 #D0BCFF
                [0.918, 0.847, 1.0, 1.0],   // MD3 PrimaryContainer #EADDFF
            ],
            ring_width: 7.0,
            base_radius: 0.135,
            growth: 0.18,
            halo_strength: 0.18,
            halo_size: 0.12,
            alpha: 1.0,
            sensitivity: 1.0,
            decay: 0.86,
            smoothness: 2.0,
            x_offset: 0.0,
            y_offset: 0.0,
        }
    }
}

impl Config {
    /// Embedded default QML/Lua configs (from the `config/` directory, kept in sync by CI/manually).
    pub const DEFAULT_QML: &'static str = include_str!("../config/pulse-ring.qml");
    pub const DEFAULT_LUA: &'static str = include_str!("../config/pulse-ring.lua");

    pub fn load(path: &Path) -> Self {
        // First run: write the bundled default configs so the user can edit them.
        ensure_defaults();
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("config {} not readable ({e}); using defaults", path.display());
                return Self::default();
            }
        };
        match parse(&src) {
            Ok(c) => {
                log::info!("loaded config from {}", path.display());
                c
            }
            Err(e) => {
                log::warn!("config parse error: {e}; using defaults");
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------- tokeniser

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    Bool(bool),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    Comma,
}

fn tokenise(src: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            '{' => { toks.push(Tok::LBrace); i += 1; }
            '}' => { toks.push(Tok::RBrace); i += 1; }
            '[' => { toks.push(Tok::LBracket); i += 1; }
            ']' => { toks.push(Tok::RBracket); i += 1; }
            ':' => { toks.push(Tok::Colon); i += 1; }
            ',' | ';' => { toks.push(Tok::Comma); i += 1; }
            '"' | '\'' => {
                let q = c;
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] as char != q {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err("unterminated string".into());
                }
                toks.push(Tok::Str(src[start..i].to_string()));
                i += 1;
            }
            '#' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
                toks.push(Tok::Str(format!("#{}", &src[start..i])));
            }
            '0'..='9' | '-' | '.' => {
                let start = i;
                while i < bytes.len() && matches!(bytes[i] as char, '0'..='9' | '.' | 'e' | 'E' | '+' | '-') {
                    i += 1;
                }
                let s = &src[start..i];
                match s.parse::<f64>() {
                    Ok(n) => toks.push(Tok::Num(n)),
                    Err(_) => return Err(format!("bad number `{s}`")),
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() && (bytes[i] as char).is_alphanumeric() || i < bytes.len() && bytes[i] == b'_' {
                    i += 1;
                }
                let word = &src[start..i];
                match word {
                    "true" => toks.push(Tok::Bool(true)),
                    "false" => toks.push(Tok::Bool(false)),
                    _ => toks.push(Tok::Ident(word.to_string())),
                }
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------- values

#[derive(Debug, Clone)]
enum Val {
    Num(f32),
    Str(String),
    Bool(bool),
    Arr(Vec<Val>),
    Obj(Vec<(String, Val)>),
}

fn parse_expr(toks: &[Tok], i: &mut usize) -> Result<Val, String> {
    if *i >= toks.len() {
        return Err("unexpected end of config".into());
    }
    match &toks[*i] {
        Tok::LBrace => {
            *i += 1;
            let mut kv = Vec::new();
            while *i < toks.len() && toks[*i] != Tok::RBrace {
                if let Tok::Ident(k) = &toks[*i] {
                    let k = k.clone();
                    *i += 1;
                    if *i < toks.len() && toks[*i] == Tok::Colon {
                        *i += 1;
                    }
                    let v = parse_expr(toks, i)?;
                    kv.push((k, v));
                } else {
                    *i += 1;
                }
                if *i < toks.len() && toks[*i] == Tok::Comma {
                    *i += 1;
                }
            }
            if *i < toks.len() {
                *i += 1; // RBrace
            }
            Ok(Val::Obj(kv))
        }
        Tok::LBracket => {
            *i += 1;
            let mut arr = Vec::new();
            while *i < toks.len() && toks[*i] != Tok::RBracket {
                arr.push(parse_expr(toks, i)?);
                if *i < toks.len() && toks[*i] == Tok::Comma {
                    *i += 1;
                }
            }
            if *i < toks.len() {
                *i += 1; // RBracket
            }
            Ok(Val::Arr(arr))
        }
        Tok::Num(n) => { *i += 1; Ok(Val::Num(*n as f32)) }
        Tok::Bool(b) => { *i += 1; Ok(Val::Bool(*b)) }
        Tok::Str(s) => { *i += 1; Ok(Val::Str(s.clone())) }
        Tok::Ident(name) => {
            let name = name.clone();
            *i += 1;
            if *i < toks.len() && toks[*i] == Tok::LBrace {
                // Typed object: `Particle { ... }` — the type name is dropped.
                parse_expr(toks, i)
            } else {
                Ok(Val::Str(name))
            }
        }
        _ => Err("unexpected token".into()),
    }
}

// ---------------------------------------------------------------- apply

fn num(v: &Val) -> Option<f32> {
    match v {
        Val::Num(n) => Some(*n),
        Val::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn parse_colour(s: &str) -> Option<[f32; 4]> {
    let hex = s.strip_prefix('#')?;
    let (r, g, b, a) = match hex.len() {
        3 => (
            u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
            255,
        ),
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            255,
        ),
        8 => (
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            u8::from_str_radix(&hex[6..8], 16).ok()?,
            u8::from_str_radix(&hex[0..2], 16).ok()?,
        ),
        _ => return None,
    };
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0])
}

fn parse_widget(obj: &[(String, Val)]) -> Option<WidgetConfig> {
    let mut w = WidgetConfig::default();
    for (k, v) in obj {
        match k.as_str() {
            "type" => {
                if let Val::Str(s) = v {
                    w.widget_type = match s.to_ascii_lowercase().as_str() {
                        "image" => {
                            w.size = 0.2; // default: 20% of shorter edge wide
                            WidgetType::Image
                        }
                        "clock" => {
                            w.size = 0.12; // default: ~130px on 1080p
                            WidgetType::Clock
                        }
                        "bars" | "bar" | "spectrum" => WidgetType::Bars,
                        "cover" | "album" | "art" => WidgetType::Cover,
                        "analog" | "clock2" | "handclock" => WidgetType::Analog,
                        "plugin" | "custom" => WidgetType::Plugin,
                        _ => WidgetType::Ring,
                    };
                }
            }
            "x" => w.x = num(v)?,
            "y" => w.y = num(v)?,
            "size" | "scale" => w.size = num(v)?,
            "alpha" | "opacity" => w.alpha = num(v)?,
            "rotate" | "rotation" => w.rotate = num(v)?,
            "fontSize" => w.font_size = num(v)?,
            "source" | "src" => {
                if let Val::Str(s) = v {
                    w.source = Some(s.clone());
                }
            }
            "plugin" | "pluginName" => {
                if let Val::Str(s) = v {
                    w.plugin = Some(s.clone());
                }
            }
            "color" | "colour" => {
                if let Val::Str(s) = v {
                    if let Some(c) = parse_colour(s) {
                        w.color = c;
                    }
                }
            }
            "shape" => {
                if let Val::Str(s) = v {
                    w.shape = match s.to_ascii_lowercase().as_str() {
                        "square" => Shape::Square,
                        "diamond" => Shape::Diamond,
                        "hexagon" => Shape::Hexagon,
                        "triangle" => Shape::Triangle,
                        "star" => Shape::Star,
                        "flower" => Shape::Flower,
                        _ => Shape::Ring,
                    };
                }
            }
            "colorMode" => {
                if let Val::Str(s) = v {
                    w.color_mode = match s.to_ascii_lowercase().as_str() {
                        "solid" => ColorMode::Solid,
                        "gradient" => ColorMode::Gradient,
                        _ => ColorMode::Hue,
                    };
                }
            }
            "colors" => {
                if let Val::Arr(items) = v {
                    w.colors.clear();
                    for it in items {
                        if let Val::Str(s) = it {
                            if let Some(c) = parse_colour(s) {
                                w.colors.push(c);
                            }
                        }
                    }
                    if !w.colors.is_empty() {
                        w.color_mode = ColorMode::Gradient;
                    }
                }
            }
            "corners" => w.corners = num(v)?,
            "spikiness" => w.spikiness = num(v)?,
            "ringWidth" => w.ring_width = num(v)?,
            "baseRadius" => w.base_radius = num(v)?,
            "growth" => w.growth = num(v)?,
            "haloStrength" => w.halo_strength = num(v)?,
            "haloSize" => w.halo_size = num(v)?,
            "dashCount" => w.dash_count = num(v)?,
            "dashRatio" => w.dash_ratio = num(v)?,
            "ringAlpha" => w.ring_alpha = num(v)?,
            "withRings" => w.with_rings = num(v)? > 0.0,
            "bars" | "barCount" => w.bar_count = num(v)?,
            "barHeight" => w.bar_height = num(v)?,
            "barGap" => w.bar_gap = num(v)?,
            "mirror" => w.bar_mirror = num(v)? > 0.0,
            "borderWidth" => w.border_width = num(v)?,
            "tickCount" => w.tick_count = num(v)?,
            "dialBorder" => w.dial_border = num(v)?,
            "coverGrowth" => w.cover_growth = num(v)?,
            "bandMode" => {
                if let Val::Str(s) = v {
                    w.band_mode = match s.to_ascii_lowercase().as_str() {
                        "bass" | "low" => BandMode::Bass,
                        "mid" | "middle" => BandMode::Mid,
                        "treble" | "high" => BandMode::Treble,
                        "energy" | "power" => BandMode::Energy,
                        _ => BandMode::Full,
                    };
                }
            }
            _ => {}
        }
    }
    Some(w)
}

fn parse_particle(obj: &[(String, Val)]) -> Option<ParticleConfig> {
    let mut p = ParticleConfig::default();
    for (k, v) in obj {
        match k.as_str() {
            "x" => p.x = num(v)?,
            "y" => p.y = num(v)?,
            "angle" => p.angle = num(v)?,
            "speed" => p.speed = num(v)?,
            "size" => p.size = num(v)?,
            "life" | "lifetime" => p.life = num(v)?,
            "delay" => p.delay = num(v)?,
            "gravity" => p.gravity = num(v)?,
            "drag" => p.drag = num(v)?,
            "fadeIn" => p.fade_in = num(v)?,
            "sizeEnd" => p.size_end = num(v)?,
            "twinkle" => p.twinkle = num(v)?,
            "wave" => p.wave = num(v)?,
            "spinSpeed" => p.spin_speed = num(v)?,
            "color" | "colour" => {
                if let Val::Str(s) = v {
                    if let Some(c) = parse_colour(s) {
                        p.color = c;
                    }
                }
            }
            _ => {}
        }
    }
    Some(p)
}

fn apply(cfg: &mut Config, key: &str, v: &Val) {
    match key {
        "shape" => {
            if let Val::Str(s) = v {
                cfg.shape = match s.to_ascii_lowercase().as_str() {
                    "square" => Shape::Square,
                    "diamond" => Shape::Diamond,
                    "hexagon" => Shape::Hexagon,
                    "triangle" => Shape::Triangle,
                    "star" => Shape::Star,
                    "flower" => Shape::Flower,
                    _ => Shape::Ring,
                };
            }
        }
        "colorMode" => {
            if let Val::Str(s) = v {
                cfg.color_mode = match s.to_ascii_lowercase().as_str() {
                    "solid" => ColorMode::Solid,
                    "gradient" => ColorMode::Gradient,
                    _ => ColorMode::Hue,
                };
            } else if let Some(n) = num(v) {
                cfg.color_mode = if n == 0.0 { ColorMode::Hue } else if n == 1.0 { ColorMode::Solid } else { ColorMode::Gradient };
            }
        }
        "color" => {
            if let Val::Str(s) = v {
                if let Some(c) = parse_colour(s) {
                    cfg.colors = vec![c];
                    cfg.color_mode = ColorMode::Solid;
                }
            }
        }
        "innerColor" => {
            if let Val::Str(s) = v {
                if let Some(c) = parse_colour(s) {
                    cfg.inner_color = c;
                }
            }
        }
        "midColor" => {
            if let Val::Str(s) = v {
                if let Some(c) = parse_colour(s) {
                    cfg.mid_color = c;
                }
            }
        }
        "spawnEffect" => {
            if let Val::Str(s) = v {
                cfg.spawn_effect = match s.to_ascii_lowercase().as_str() {
                    "none" => SpawnEffect::None,
                    "zoom" => SpawnEffect::Zoom,
                    _ => SpawnEffect::Expand,
                };
            }
        }
        "spawnEase" => {
            if let Val::Str(s) = v {
                cfg.spawn_ease = match s.to_ascii_lowercase().as_str() {
                    "outBack" => SpawnEase::OutBack,
                    "elastic" => SpawnEase::Elastic,
                    "bounce" => SpawnEase::Bounce,
                    _ => SpawnEase::OutCubic,
                };
            }
        }
        "particleShape" => {
            if let Val::Str(s) = v {
                cfg.particle_shape = match s.to_ascii_lowercase().as_str() {
                    "square" => ParticleShape::Square,
                    "diamond" => ParticleShape::Diamond,
                    "star" => ParticleShape::Star,
                    _ => ParticleShape::Circle,
                };
            }
        }
        "particleMode" | "particlesMode" => {
            if let Val::Str(s) = v {
                cfg.particle_mode = match s.to_ascii_lowercase().as_str() {
                    "burst" => ParticleMode::Burst,
                    "orbit" => ParticleMode::Orbit,
                    "ring" => ParticleMode::Ring,
                    _ => ParticleMode::None,
                };
            }
        }
        "colors" | "gradient" => {
            if let Val::Arr(items) = v {
                cfg.colors.clear();
                for it in items {
                    if let Val::Str(s) = it {
                        if let Some(c) = parse_colour(s) {
                            cfg.colors.push(c);
                        }
                    }
                }
                if !cfg.colors.is_empty() {
                    cfg.color_mode = ColorMode::Gradient;
                }
            } else if let Val::Str(s) = v {
                if let Some(c) = parse_colour(s) {
                    cfg.colors = vec![c];
                    cfg.color_mode = ColorMode::Gradient;
                }
            }
        }
        "luaScript" | "lua" => {
            if let Val::Str(s) = v {
                cfg.lua_script = Some(s.clone());
            }
        }
        "widgets" => {
            if let Val::Arr(items) = v {
                cfg.widgets.clear();
                for it in items {
                    if let Val::Obj(obj) = it {
                        if let Some(w) = parse_widget(obj) {
                            cfg.widgets.push(w);
                        }
                    }
                }
            }
        }
        "particles" => {
            if let Val::Arr(items) = v {
                cfg.particles.clear();
                for it in items {
                    if let Val::Obj(obj) = it {
                        if let Some(p) = parse_particle(obj) {
                            cfg.particles.push(p);
                        }
                    }
                }
            }
        }
        _ => {
            if let Some(n) = num(v) {
                match key {
                    "ringWidth" | "ringWidthPx" | "width" => cfg.ring_width = n,
                    "baseRadius" => cfg.base_radius = n,
                    "growth" => cfg.growth = n,
                    "haloStrength" => cfg.halo_strength = n,
                    "haloSize" | "halo" => cfg.halo_size = n,
                    "alpha" | "opacity" => cfg.alpha = n,
                    "sensitivity" => cfg.sensitivity = n,
                    "decay" => cfg.decay = n,
                    "smoothness" => cfg.smoothness = n,
                    "xOffset" => cfg.x_offset = n,
                    "yOffset" => cfg.y_offset = n,
                    "corners" => cfg.corners = n,
                    "spikiness" => cfg.spikiness = n,
                    "rotate" | "rotation" => cfg.rotate = n,
                    "dashCount" => cfg.dash_count = n,
                    "dashRatio" => cfg.dash_ratio = n,
                    "saturnBand" => cfg.saturn_band = n,
                    "saturnAlpha" => cfg.saturn_alpha = n,
                    "saturnStripes" => cfg.saturn_stripes = n,
                    "midRing" => cfg.mid_ring = n > 0.0,
                    "midRadius" => cfg.mid_radius = n,
                    "midGrowth" => cfg.mid_growth = n,
                    "midWidth" => cfg.mid_width = n,
                    "autoRotate" => cfg.auto_rotate = n,
                    "idleBreathe" => cfg.idle_breathe = n,
                    "innerAlpha" => cfg.inner_alpha = n,
                    "innerRing" => cfg.inner_ring = n > 0.0,
                    "innerRadius" => cfg.inner_radius = n,
                    "innerGrowth" => cfg.inner_growth = n,
                    "innerWidth" => cfg.inner_width = n,
                    "spawnDuration" => cfg.spawn_duration = n,
                    "particleLoop" => cfg.particle_loop = n > 0.0,
                    _ => {}
                }
            }
        }
    }
}

fn parse(src: &str) -> Result<Config, String> {
    let toks = tokenise(src)?;
    let mut i = 0usize;
    let root = parse_expr(&toks, &mut i)?;
    let mut cfg = Config::default();
    if let Val::Obj(kv) = root {
        for (k, v) in kv {
            apply(&mut cfg, &k, &v);
        }
    }
    Ok(cfg)
}

/// Test helper: parse a QML string directly.
pub fn parse_for_test(src: &str) -> Config {
    parse(src).unwrap_or_default()
}

/// Locate the config file: `$XDG_CONFIG_HOME/pulse-ring/pulse-ring.qml` (or
/// `~/.config/...`), falling back to `./pulse-ring.qml`.
/// Write the bundled default QML/Lua configs to ~/.config/pulse-ring/ on first run.
fn ensure_defaults() {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    let dir = base.join("pulse-ring");
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let qml = dir.join("pulse-ring.qml");
    if !qml.exists() {
        if let Err(e) = std::fs::write(&qml, Config::DEFAULT_QML) {
            log::warn!("failed to write default config {}: {e}", qml.display());
        } else {
            log::info!("wrote default config to {}", qml.display());
        }
    }
    let lua = dir.join("pulse-ring.lua");
    if !lua.exists() {
        if let Err(e) = std::fs::write(&lua, Config::DEFAULT_LUA) {
            log::warn!("failed to write default lua {}: {e}", lua.display());
        } else {
            log::info!("wrote default lua to {}", lua.display());
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PULSE_RING_CONFIG") {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            return p;
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    let p = base.join("pulse-ring").join("pulse-ring.qml");
    if p.exists() {
        return p;
    }
    let local = std::path::PathBuf::from("pulse-ring.qml");
    if local.exists() {
        return local;
    }
    p
}