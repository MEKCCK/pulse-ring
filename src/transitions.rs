//! GLSL transition compilation for the wallpaper switcher.
//!
//! Borrowed from Kaleidux (shaders.rs): the 50+ built-in transitions live in
//! `assets/shaders/transitions/*.glsl` (gl-transitions style). Each file defines
//! `vec4 transition(vec2 uv)` using `progress`, `getFromColor`/`getToColor`. We
//! wrap it with a standard prelude, strip the `uniform` declarations (converting
//! their `// = default` comments into `#define`s — naga needs concrete values),
//! and compile GLSL -> WGSL via naga at startup.

use std::collections::HashMap;
use std::sync::Mutex;

/// Standard wrapper: uniforms + texture accessors + the fragment main.
const GLSL_PRELUDE: &str = r#"
#version 450
precision highp float;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o_color;
layout(set = 0, binding = 0) uniform TransitionUniforms {
    float progress;
    float screen_aspect;
    vec4 params[7];
};
layout(set = 0, binding = 1) uniform texture2D t_from;
layout(set = 0, binding = 2) uniform texture2D t_to;
layout(set = 0, binding = 3) uniform sampler s_linear;
#define ratio screen_aspect
vec4 getFromColor(vec2 uv) { return texture(sampler2D(t_from, s_linear), uv); }
vec4 getToColor(vec2 uv) { return texture(sampler2D(t_to, s_linear), uv); }
"#;

static CACHE: Mutex<Option<HashMap<String, (String, String)>>> = Mutex::new(None);

/// Compile a transition GLSL source to WGSL (cached by name). Returns (wgsl, fragment
/// entry point name) — the fragment entry is appended with a matching vertex stage by
/// the caller.
pub fn compile(name: &str, glsl: &str) -> Result<(String, String), String> {
    let mut guard = CACHE.lock().unwrap();
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(w) = cache.get(name) {
        return Ok(w.clone());
    }

    // Convert `uniform <type> <name>; // = <default>` lines into `#define name (default)`
    // and drop the bare uniform lines (naga requires concrete values).
    let mut defines = String::new();
    let mut stripped = String::new();
    for line in glsl.lines() {
        let t = line.trim_start();
        if t.starts_with("uniform ") {
            // Extract `type name = default` from `uniform type name; // = default`
            let decl = t["uniform ".len()..].trim().trim_end_matches(';').to_string();
            // Everything after "//" is a comment; the default lives there.
            let (decl, comment) = match decl.find("//") {
                Some(i) => (decl[..i].trim().to_string(), decl[i + 2..].trim().to_string()),
                None => (decl.clone(), String::new()),
            };
            let parts: Vec<&str> = decl.split_whitespace().collect();
            if parts.len() >= 2 {
                let name_part = parts[1].trim_end_matches([';', '/']).to_string();
                // Default: from "= value" inside the comment, or "name = value" in the decl.
                let default = comment
                    .strip_prefix('=')
                    .or_else(|| {
                        let rest: Vec<&str> = decl.splitn(3, '=').collect();
                        if rest.len() >= 2 { Some(rest[1]) } else { None }
                    })
                    .map(|d| d.trim().trim_end_matches(';').to_string());
                if let Some(val) = default {
                    defines.push_str(&format!("#define {} ({})\n", name_part.trim(), val.trim()));
                }
            }
            // The uniform line itself is dropped.
            continue;
        }
        stripped.push_str(line);
        stripped.push('\n');
    }

    // Two-parameter transitions (parallax style: `transition(vec2 p, vec2 prev_p)`)
    // are wrapped: rename the original and expose a 1-param `transition(vec2 uv)`.
    let mut stripped = stripped;
    if stripped.contains("transition (vec2 p, vec2") || stripped.contains("transition(vec2 p, vec2") {
        stripped = stripped.replace("vec4 transition (vec2 p, vec2", "vec4 transition_full (vec2 p, vec2");
        stripped = stripped.replace("vec4 transition(vec2 p, vec2", "vec4 transition_full (vec2 p, vec2");
        stripped.push_str("\nvec4 transition(vec2 uv) { return transition_full(uv, uv); }\n");
    }

    let full_glsl = format!("{}\n{}\n{}\nvoid main() {{ o_color = transition(v_uv); }}", GLSL_PRELUDE, defines, stripped);

    let mut parser = naga::front::glsl::Frontend::default();
    let module = parser
        .parse(
            &naga::front::glsl::Options {
                stage: naga::ShaderStage::Fragment,
                defines: Default::default(),
            },
            &full_glsl,
        )
        .map_err(|e| format!("GLSL parse '{}': {e:?}\n--- source (char 600-680) ---\n{}", name, &full_glsl[600.min(full_glsl.len())..680.min(full_glsl.len())]))?;

    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| format!("GLSL validate '{}': {e:?}", name))?;

    let mut out = String::new();
    let mut writer = naga::back::wgsl::Writer::new(&mut out, naga::back::wgsl::WriterFlags::empty());
    writer
        .write(&module, &info)
        .map_err(|e| format!("WGSL gen '{}': {e:?}", name))?;

    let entry = module
        .entry_points
        .iter()
        .find(|e| e.stage == naga::ShaderStage::Fragment)
        .map(|e| e.name.clone())
        .unwrap_or_else(|| "fs_main".to_string());
    cache.insert(name.to_string(), (out.clone(), entry.clone()));
    Ok((out, entry))
}

/// Path of a built-in transition by name (case-insensitive), e.g. "circleopen".
pub fn transition_path(name: &str) -> Option<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/shaders/transitions");
    let lower = name.to_ascii_lowercase();
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if stem.to_ascii_lowercase() == lower {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }
    None
}
