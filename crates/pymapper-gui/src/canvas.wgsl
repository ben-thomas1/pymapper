// Placeholder canvas: an anti-aliased grid in the canvas rect (scissored by the caller).
// Replaced by the instanced code-map renderer.

struct Canvas {
    origin: vec2<f32>,      // canvas rect min, physical pixels
    size: vec2<f32>,        // canvas rect size, physical pixels
    spacing: f32,           // minor grid spacing, physical pixels
    line_width: f32,        // physical pixels
    _pad: vec2<f32>,
    background: vec4<f32>,
    minor: vec4<f32>,
    major: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Canvas;

// One triangle covering the whole target; the scissor rect limits it to the canvas.
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

// Coverage of the nearest grid line of the given spacing (1 on the line, fading over 1 px).
fn line_coverage(p: vec2<f32>, spacing: f32) -> f32 {
    let d = abs(fract(p / spacing + 0.5) - 0.5) * spacing;
    let dist = min(d.x, d.y);
    return clamp(u.line_width * 0.5 + 0.5 - dist, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // Pixel centres are at .5; shift so lines land on pixel centres.
    let p = pos.xy - u.origin - vec2<f32>(0.5, 0.5);
    var c = u.background;
    c = mix(c, u.minor, line_coverage(p, u.spacing) * u.minor.a);
    c = mix(c, u.major, line_coverage(p, u.spacing * 5.0) * u.major.a);
    return vec4<f32>(c.rgb, 1.0);
}
