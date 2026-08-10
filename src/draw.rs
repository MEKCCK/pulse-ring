use std::num::NonZeroU32;

use bytemuck::{Pod, Zeroable};
use wgpu::wgt::CompositeAlphaMode;

use crate::audio::NBANDS;

/// GPU renderer for the pulsing ring. Owns the wgpu surface/pipeline and a uniform buffer
/// holding the latest 128 band magnitudes. CPU work per frame: a small buffer write + one draw.
/// All ring geometry / shading is computed in the fragment shader.
pub struct RingRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    configured: bool,
    ring_cfg: crate::config::Config,
    render_count: u64,
    fail_count: u64,
    id: u32,
    auto_rotate: f32,
}

/// Shader uniforms. Matches `struct Uniforms` in ring.wgsl.
/// Layout rules (storage address space): f32/vec2<f32> align 4/8, array stride 4.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Uniforms {
    bands: [f32; NBANDS], // offset 0, 512 bytes
    resolution: [f32; 2], // 512
    base_r: f32,          // 520
    half_thick: f32,      // 524
    growth: f32,          // 528
    halo: f32,            // 532
    aa: f32,              // 536
    halo_strength: f32,   // 540
    alpha: f32,           // 544
    x_off: f32,           // 548
    y_off: f32,           // 552
    smoothness: f32,      // 556
    color_mode: u32,      // 560
    colors: [f32; 16],    // 564..628 (4x RGBA)
    // ---- double ring ----
    bass: f32,            // 628
    inner_enabled: u32,   // 632
    inner_base_r: f32,    // 636
    inner_growth: f32,    // 640
    inner_half_thick: f32, // 644
    inner_color: [f32; 4], // 648..664
    // ---- middle ring ----
    mid_enabled: u32,     // 664
    mid_base_r: f32,      // 668
    mid_growth: f32,      // 672
    mid_half_thick: f32,  // 676
    mid_color: [f32; 4],  // 680..696
    // ---- shape ----
    shape: u32,           // 664
    corners: f32,         // 668
    spikiness: f32,       // 672
    rotate: f32,          // 676
    // ---- spawn / particles ----
    spawn_scale: f32,     // 680
    particle_mode: u32,   // 684
    particle_loop: u32,   // 688
    // ---- appearance extras ----
    dash_count: f32,      // 692
    dash_ratio: f32,      // 696
    idle_breathe: f32,    // 700
    inner_alpha: f32,     // 704
    particle_shape: u32,  // 708
    time: f32,            // 712
    // ---- saturn band ----
    saturn_band: f32,     // 716
    saturn_alpha: f32,    // 720
    saturn_stripes: f32,  // 724
    // 32 particles x 12 f32 (x, y, size, alpha, r, g, b, a, spin, vx, vy, pad) — 720..
    particles: [f32; 1152],
}

impl RingRenderer {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: wgpu::Surface<'static>,
        adapter: &wgpu::Adapter,
        cfg: &crate::config::Config,
        id: u32,
    ) -> Self {
        let caps = surface.get_capabilities(adapter);

        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb))
            .unwrap_or(caps.formats[0]);

        let alpha_mode = if caps.alpha_modes.contains(&CompositeAlphaMode::PreMultiplied) {
            CompositeAlphaMode::PreMultiplied
        } else {
            CompositeAlphaMode::Auto
        };
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: 64,
            height: 64,
            desired_maximum_frame_latency: 2,
            present_mode,
            alpha_mode,
            view_formats: vec![],
        };
        log::info!("wgpu surface: format={format:?}, alpha={alpha_mode:?}, present={present_mode:?}");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ring"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ring uniforms"),
            size: 5392,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ring bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU32::new(5392).map(|n| n.get() as u64).and_then(std::num::NonZeroU64::new),
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ring bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ring pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ring pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        RingRenderer {
            surface,
            device,
            queue,
            config,
            pipeline,
            uniform_buffer,
            bind_group,
            width: 64,
            height: 64,
            configured: false,
            ring_cfg: cfg.clone(),
            render_count: 0,
            fail_count: 0,
            id,
            auto_rotate: 0.0,
        }
    }

    /// Snapshot of the loaded config (used for spawn/particle animation on the CPU side).
    pub fn config_ref(&self) -> &crate::config::Config {
        &self.ring_cfg
    }

    /// Current auto-rotation angle in radians (config rotate + autoRotate*time).
    pub fn set_auto_rotate(&mut self, rad: f32) {
        self.auto_rotate = rad;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.configured && self.width == width && self.height == height {
            return;
        }
        self.width = width.max(1);
        self.height = height.max(1);
        self.config.width = self.width;
        self.config.height = self.height;
        self.configured = true;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn render(
        &mut self,
        bands: &[f32; NBANDS],
        spawn_scale: f32,
        particles: &[f32; 1152],
        now: f32,
    ) {
        if !self.configured {
            log::info!("render id={} SKIPPED: not configured", self.id);
            return;
        }
        self.render_count += 1;
        if self.id == 0 && self.render_count % 30 == 1 {
            log::info!("render id=0 entering get_current_texture (#{})", self.render_count);
        }
        // Timeout/Occluded: transient in Mailbox mode when the previous frame is still being
        // composited — retry briefly instead of skipping the frame, which causes visible
        // stutter on secondary monitors.
        let mut frame = loop {
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => break f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => break f,
                wgpu::CurrentSurfaceTexture::Timeout
                | wgpu::CurrentSurfaceTexture::Occluded => {
                    self.fail_count += 1;
                    if self.fail_count % 300 == 1 {
                        log::warn!(
                            "render id={} acquire stalled ({} fails, {} ok)",
                            self.id,
                            self.fail_count,
                            self.render_count,
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    self.fail_count += 1;
                    log::warn!("render id={} surface outdated; reconfiguring", self.id);
                    self.surface.configure(&self.device, &self.config);
                    continue;
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    self.fail_count += 1;
                    log::warn!("surface lost; reconfiguring");
                    self.configured = false;
                    self.surface.configure(&self.device, &self.config);
                    continue;
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    self.fail_count += 1;
                    log::warn!("render id={} surface validation error", self.id);
                    return;
                }
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let c = &self.ring_cfg;
        let min_d = self.width.min(self.height) as f32;

        // Idle breathing is binary: active only when there is no audio at all (energy below a
        // tiny threshold), off as soon as any real signal arrives.
        let energy = bands.iter().copied().fold(0.0f32, f32::max);
        let idle_factor = if energy > 0.001 { 0.0 } else { 1.0 };
        let mut colors = [0.0f32; 16];
        // Fill up to 4 RGBA colours; pad with the last one (or a default cyan).
        for (i, col) in c.colors.iter().take(4).enumerate() {
            colors[i * 4..i * 4 + 4].copy_from_slice(col);
        }
        let last = c.colors.last().copied().unwrap_or([0.0, 0.89, 1.0, 1.0]);
        for i in c.colors.len().min(4)..4 {
            colors[i * 4..i * 4 + 4].copy_from_slice(&last);
        }
        // Bass energy: strongest of the low quarter of bands, drives the inner ring.
        let bass = bands[..NBANDS / 4].iter().copied().fold(0.0f32, f32::max);
        let uniforms = Uniforms {
            bands: *bands,
            resolution: [self.width as f32, self.height as f32],
            base_r: min_d * c.base_radius,
            half_thick: (min_d * 0.006).max(1.6) * (c.ring_width / 6.0).max(0.1),
            growth: min_d * c.growth,
            halo: min_d * c.halo_size,
            aa: 1.4,
            halo_strength: c.halo_strength,
            alpha: c.alpha,
            x_off: c.x_offset,
            y_off: c.y_offset,
            smoothness: c.smoothness.clamp(0.0, 1.0),
            color_mode: match c.color_mode {
                crate::config::ColorMode::Hue => 0,
                crate::config::ColorMode::Solid => 1,
                crate::config::ColorMode::Gradient => 2,
            },
            colors,
            bass,
            inner_enabled: c.inner_ring as u32,
            inner_base_r: min_d * c.base_radius * c.inner_radius,
            inner_growth: min_d * c.inner_growth,
            inner_half_thick: (min_d * 0.006).max(1.6) * (c.inner_width / 6.0).max(0.1),
            inner_color: c.inner_color,
            mid_enabled: c.mid_ring as u32,
            mid_base_r: min_d * c.base_radius * c.mid_radius,
            mid_growth: min_d * c.mid_growth,
            mid_half_thick: (min_d * 0.006).max(1.6) * (c.mid_width / 6.0).max(0.1),
            mid_color: c.mid_color,
            shape: match c.shape {
                crate::config::Shape::Ring => 0,
                crate::config::Shape::Square => 1,
                crate::config::Shape::Diamond => 2,
                crate::config::Shape::Hexagon => 3,
                crate::config::Shape::Triangle => 4,
                crate::config::Shape::Star => 5,
                crate::config::Shape::Flower => 6,
            },
            corners: c.corners.max(2.0),
            spikiness: c.spikiness.clamp(0.0, 1.0),
            rotate: self.auto_rotate,
            spawn_scale: spawn_scale,
            particle_mode: match c.particle_mode {
                crate::config::ParticleMode::Burst => 1,
                crate::config::ParticleMode::Orbit => 2,
                crate::config::ParticleMode::Ring => 3,
                crate::config::ParticleMode::None => 0,
            },
            particle_loop: c.particle_loop as u32,
            dash_count: c.dash_count.max(0.0),
            dash_ratio: c.dash_ratio.clamp(0.0, 1.0),
            // Idle breathing only when audio is quiet: fade out smoothly as energy rises.
            idle_breathe: c.idle_breathe.clamp(0.0, 1.0) * idle_factor,
            inner_alpha: c.inner_alpha.clamp(0.0, 1.0),
            particle_shape: match c.particle_shape {
                crate::config::ParticleShape::Circle => 0,
                crate::config::ParticleShape::Square => 1,
                crate::config::ParticleShape::Diamond => 2,
                crate::config::ParticleShape::Star => 3,
            },
            time: now,
            saturn_band: c.saturn_band.max(0.0),
            saturn_alpha: c.saturn_alpha.clamp(0.0, 1.0),
            saturn_stripes: c.saturn_stripes.clamp(0.0, 1.0),
            particles: *particles,
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ring") });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ring pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Fully transparent clear — wallpaper shows through where the ring has alpha 0.
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);

        self.queue.submit(Some(encoder.finish()));
        if self.id == 0 && self.render_count % 30 == 1 {
            log::info!("render id=0 presenting (#{})", self.render_count);
        }
        self.queue.present(frame);
        if self.render_count % 60 == 1 {
            log::info!(
                "render stats id={}: {}/{} frames, {} failures, {}x{}",
                self.id,
                self.render_count,
                self.render_count + self.fail_count,
                self.fail_count,
                self.width,
                self.height,
            );
        }
    }
}

/// Smoothstep helper (CPU side): 0 below edge0, 1 above edge1, smooth between.
fn smoothstep01(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Full-screen triangle vertex shader + SDF ring fragment shader. CPU uploads band magnitudes
/// (NBANDS floats) each frame; the shader does all per-pixel math on the GPU.
const SHADER_SRC: &str = stringify!(
    const NBANDS: u32 = 128u;

    struct Uniforms {
        bands: array<f32, NBANDS>,
        resolution: vec2<f32>,
        base_r: f32,
        half_thick: f32,
        growth: f32,
        halo: f32,
        aa: f32,
        halo_strength: f32,
        alpha: f32,
        x_off: f32,
        y_off: f32,
        smoothness: f32,
        color_mode: u32,
        colors: array<f32, 16>,
        bass: f32,
        inner_enabled: u32,
        inner_base_r: f32,
        inner_growth: f32,
        inner_half_thick: f32,
        inner_color: array<f32, 4>,
        mid_enabled: u32,
        mid_base_r: f32,
        mid_growth: f32,
        mid_half_thick: f32,
        mid_color: array<f32, 4>,
        shape: u32,
        corners: f32,
        spikiness: f32,
        rotate: f32,
        spawn_scale: f32,
        particle_mode: u32,
        particle_loop: u32,
        dash_count: f32,
        dash_ratio: f32,
        idle_breathe: f32,
        inner_alpha: f32,
        particle_shape: u32,
        time: f32,
        saturn_band: f32,
        saturn_alpha: f32,
        saturn_stripes: f32,
        particles: array<f32, 1152>,
    };

    @group(0) @binding(0) var<storage, read> u: Uniforms;

    struct VsOut {
        @builtin(position) pos: vec4<f32>,
    };

    struct Band {
        idx: u32,
        frac: f32,
    }

    @vertex
    fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
        let p = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
        return VsOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0));
    }

    fn hash_band(ang: f32) -> Band {
        let t = ang / 6.28318530718 * f32(NBANDS);
        return Band(u32(t) % NBANDS, t - floor(t));
    }

    fn hsl_to_rgb(h: f32, s: f32, l: f32) -> vec3<f32> {
        let c = (1.0 - abs(2.0 * l - 1.0)) * s;
        let hp = h / 60.0;
        let x = c * (1.0 - abs(hp % 2.0 - 1.0));
        let m = l - c * 0.5;
        var rgb = vec3<f32>(0.0);
        if (hp < 1.0) { rgb = vec3<f32>(c, x, 0.0); }
        else if (hp < 2.0) { rgb = vec3<f32>(x, c, 0.0); }
        else if (hp < 3.0) { rgb = vec3<f32>(0.0, c, x); }
        else if (hp < 4.0) { rgb = vec3<f32>(0.0, x, c); }
        else if (hp < 5.0) { rgb = vec3<f32>(x, 0.0, c); }
        else { rgb = vec3<f32>(c, 0.0, x); }
        return rgb + vec3<f32>(m, m, m);
    }

    // Look up a colour from the 4-slot palette: colors[4i..4i+4] = RGBA.
    fn pal_col(i: u32) -> vec4<f32> {
        let o = i * 4u;
        return vec4<f32>(u.colors[o], u.colors[o + 1u], u.colors[o + 2u], u.colors[o + 3u]);
    }

    // Smooth the band magnitude around angle `ang` (mix of nearest band and neighbours).
    fn band_amp(ang: f32) -> f32 {
        let bip = hash_band(ang);
        let i0 = bip.idx;
        let i1 = (i0 + 1u) % NBANDS;
        let a = mix(u.bands[i0], u.bands[i1], bip.frac);
        if (u.smoothness <= 0.0) {
            return a;
        }
        // Wide triangular smoothing window: radius scales with smoothness (0..1 -> 0..14 bands
        // on each side). This turns the per-band "jagged" edge into a smooth elastic wave.
        let w = u32(u.smoothness * 14.0);
        if (w == 0u) {
            return a;
        }
        var acc = u.bands[i0];
        var wt = 1.0;
        for (var d = 1u; d <= w; d = d + 1u) {
            let j1 = (i0 + d) % NBANDS;
            let j2 = (i0 + NBANDS - d) % NBANDS;
            let weight = 1.0 - f32(d) / f32(w + 1u);
            acc = acc + (u.bands[j1] + u.bands[j2]) * weight;
            wt = wt + weight * 2.0;
        }
        let sm = acc / wt;
        return mix(a, sm, u.smoothness);
    }

    // Idle breathing: gentle sinusoidal pulse layered under real audio.
    fn idle_amp() -> f32 {
        if (u.idle_breathe <= 0.0) {
            return 0.0;
        }
        let w = 0.5 + 0.5 * sin(u.time * 1.8);
        return u.idle_breathe * w;
    }

    // Normalised (radius 1) polar boundary of the configured shape at angle `ang`.
    // Super-ellipse: 1 / (|cos|^n + |sin|^n)^(1/n); petals: multiply by (1 + spike*cos(k*ang)).
    fn shape_radius(ang: f32) -> f32 {
        let a = ang + u.rotate;
        let sa = sin(a);
        let ca = cos(a);
        var n = 2.0; // super-ellipse exponent: 2=circle, 8=square, 1=diamond, 6=hexagon, 3=triangle
        var petal = 0.0;
        if (u.shape == 1u) { n = 8.0; }
        else if (u.shape == 2u) { n = 1.0; }
        else if (u.shape == 3u) { n = 6.0; }
        else if (u.shape == 4u) { n = 3.0; }
        else if (u.shape == 5u) { n = 2.0; petal = u.spikiness * 0.9; }
        else if (u.shape == 6u) { n = 2.0; petal = u.spikiness; }
        // |cos|^n + |sin|^n via pow; WGSL pow(x, y) = x^y.
        let p = pow(abs(sa), n) + pow(abs(ca), n);
        let super_e = 1.0 / pow(p, 1.0 / n);
        var r = super_e;
        if (petal > 0.0) {
            r = r * (1.0 + petal * cos(u.corners * a));
        }
        return r;
    }

    // Boundary radius in pixels for a given amplitude (music scales the shape).
    fn ring_edge(dist: f32, ang: f32, amp: f32, base: f32, growth: f32) -> f32 {
        let base_r = base * shape_radius(ang);
        return base_r + amp * growth;
    }

    // Annulus alpha around the polar shape: |dist - edge| < thickness.
    fn shape_ring_a(dist: f32, ang: f32, amp: f32, base: f32, growth: f32, thick: f32) -> f32 {
        let edge = ring_edge(dist, ang, amp, base, growth);
        let inside = thick - abs(dist - edge);
        var a = smoothstep(-u.aa, u.aa, inside);
        // Dashed outline: keep a fraction of each angular segment lit.
        if (u.dash_count > 0.0) {
            let seg = fract(ang / 6.28318530718 * u.dash_count);
            if (seg > u.dash_ratio) {
                a = a * (1.0 - smoothstep(u.dash_ratio, u.dash_ratio + 0.02, seg));
            }
        }
        return a;
    }

    // Overall energy: mean of the mid-frequency bands, drives the middle ring.
    fn overall_energy() -> f32 {
        var acc = 0.0;
        for (var i = 16u; i < 96u; i = i + 1u) {
            acc = acc + u.bands[i];
        }
        return acc / 80.0;
    }

    // Middle ring: constant-radius annulus scaling with overall energy.
    fn mid_ring_a(dist: f32, ang: f32) -> f32 {
        if (u.mid_enabled == 0u) {
            return 0.0;
        }
        return shape_ring_a(dist, ang, overall_energy(), u.mid_base_r, u.mid_growth, u.mid_half_thick);
    }

    // Inner shape "breathes" with bass.
    fn inner_ring_a(dist: f32, ang: f32) -> f32 {
        return inner_ring_a_scaled(dist, ang, u.inner_base_r);
    }

    fn inner_ring_a_scaled(dist: f32, ang: f32, base: f32) -> f32 {
        if (u.inner_enabled == 0u) {
            return 0.0;
        }
        return shape_ring_a(dist, ang, u.bass, base, u.inner_growth, u.inner_half_thick) * u.inner_alpha;
    }

    @fragment
    fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
        let min_d = min(u.resolution.x, u.resolution.y);
        let centre = u.resolution * 0.5 + vec2<f32>(u.x_off, u.y_off) * min_d;
        let d = in.pos.xy - centre;
        let dist = length(d);

        var ang = atan2(d.y, d.x);
        if (ang < 0.0) { ang = ang + 6.28318530718; }
        let amp = max(band_amp(ang), idle_amp());

        // ---- outer shape (music-reactive, angle-mapped), scaled by spawn anim ----
        let base_scaled = u.base_r * u.spawn_scale;
        let inner_base_scaled = u.inner_base_r * u.spawn_scale;
        let edge_out = ring_edge(dist, ang, amp, base_scaled, u.growth);
        let ring_a = shape_ring_a(dist, ang, amp, base_scaled, u.growth, u.half_thick);

        var halo_a = 0.0;
        if (dist > edge_out) {
            let h_t = max(0.0, edge_out + u.halo - dist) / u.halo;
            halo_a = min(1.0, h_t * amp) * u.halo_strength;
        }

        let mid_a = mid_ring_a(dist, ang);
        let a = max(max(max(ring_a, halo_a), mid_a), inner_ring_a_scaled(dist, ang, inner_base_scaled)) * u.alpha;

        // Middle ring colour.
        let mid_present = mid_ring_a(dist, ang);
        // Inner ring gets its own fixed colour (inner_color) when visible.
        let inner_present = inner_ring_a_scaled(dist, ang, inner_base_scaled);
        var rgb: vec3<f32>;
        if (mid_present > 0.0 && u.mid_color[3] > 0.0) {
            rgb = vec3<f32>(u.mid_color[0], u.mid_color[1], u.mid_color[2]) * u.mid_color[3];
        } else if (inner_present > 0.0 && u.inner_color[3] > 0.0) {
            rgb = vec3<f32>(u.inner_color[0], u.inner_color[1], u.inner_color[2]) * u.inner_color[3];
        } else if (u.color_mode == 1u) {
            // Solid colour.
            let c = pal_col(0u);
            rgb = c.rgb * c.a;
        } else if (u.color_mode == 2u) {
            // Gradient across the ring: 4-colour linear interpolation.
            let t = ang / 6.28318530718;
            let seg = u32(t * 3.0);
            let ft = fract(t * 3.0);
            let c0 = pal_col(seg);
            let c1 = pal_col(min(seg + 1u, 3u));
            let col = mix(c0, c1, ft);
            rgb = col.rgb * col.a;
        } else {
            // Hue-rotating HSL.
            let hue = fract(ang / 6.28318530718 + 200.0 / 360.0) * 360.0;
            let light = 0.55 + 0.25 * amp;
            rgb = hsl_to_rgb(hue, 0.65, light);
        }

        // ---- saturn ring band: continuous translucent band hugging the outer ring ----
        var sat_a = 0.0;
        if (u.saturn_band > 0.0 && dist > edge_out) {
            let band_w = u.saturn_band * min_d;
            let t_in = (dist - edge_out) / band_w;
            if (t_in < 1.0) {
                // Soft inner edge, feathered outer edge.
                let fe = smoothstep(0.0, 0.08, t_in) * (1.0 - smoothstep(0.7, 1.0, t_in));
                // Concentric striations like Saturn's ring bands.
                let stripe = 1.0 - u.saturn_stripes * 0.5 * (1.0 + sin(t_in * 40.0));
                sat_a = fe * u.saturn_alpha * stripe * (0.6 + 0.4 * amp);
            }
        }

        // ---- particles (shaped sprites, spin + trail) ----
        // Colour uses "brightest particle wins" compositing so overlapping particles never
        // blow out to white; the ring mode has no trail ghosts to avoid self-overlap.
        var p_col = vec3<f32>(0.0);
        var p_a = 0.0;
        if (u.particle_mode != 0u) {
            let trail_max = select(1.0, 0.0, u.particle_mode == 3u);
            for (var i = 0u; i < 96u; i = i + 1u) {
                let o = i * 12u;
                let px = u.particles[o];
                let py = u.particles[o + 1u];
                let psize = u.particles[o + 2u];
                let palpha = u.particles[o + 3u];
                if (palpha <= 0.004) {
                    continue;
                }
                let spin = u.particles[o + 8u];
                let vx = u.particles[o + 9u];
                let vy = u.particles[o + 10u];
                var t = 0.0;
                while (t <= trail_max) {
                    let ghost = vec2<f32>(px - vx * t * 0.05, py - vy * t * 0.05);
                    let dd = in.pos.xy - ghost;
                    // Rotate into the sprite's local frame for shaped sprites.
                    let cs = cos(-spin);
                    let sn = sin(-spin);
                    let lx = dd.x * cs - dd.y * sn;
                    let ly = dd.x * sn + dd.y * cs;
                    let r = psize * (1.0 - t * 0.35);
                    var sd = length(vec2<f32>(lx, ly));
                    if (u.particle_shape == 1u) {
                        sd = max(abs(lx), abs(ly));
                    } else if (u.particle_shape == 2u) {
                        sd = abs(lx) + abs(ly);
                    } else if (u.particle_shape == 3u) {
                        // 5-point star via polar radius.
                        let a = atan2(ly, lx);
                        let sp = 0.75 + 0.25 * cos(5.0 * a);
                        sd = length(vec2<f32>(lx, ly)) / sp;
                    }
                    let da = smoothstep(r + 1.0, max(r - 1.0, 0.0), sd) * palpha * (1.0 - t * 0.6);
                    if (da > p_a) {
                        p_a = da;
                        p_col = vec3<f32>(u.particles[o + 4u], u.particles[o + 5u], u.particles[o + 6u]) * da;
                    }
                    t = t + 1.0;
                }
            }
        }

        // Composite: rings + saturn band + particles over transparent background (premultiplied).
        let pa = min(p_a, 1.0);
        let sat_col = vec3<f32>(0.75, 0.85, 1.0);
        let col = mix(rgb * a, sat_col, sat_a / max(a + sat_a, 0.0001)) * (a + sat_a) + p_col * (1.0 - min(a + sat_a, 1.0));
        let alpha = a + sat_a + pa * (1.0 - min(a + sat_a, 1.0));
        if (alpha <= 0.004) {
            return vec4<f32>(0.0, 0.0, 0.0, 0.0);
        }
        return vec4<f32>(col * alpha, alpha);
    }
);