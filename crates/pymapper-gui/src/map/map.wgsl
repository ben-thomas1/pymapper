// Code-map canvas shaders. Every pipeline reads the same camera uniform, so panning and
// zooming change 48 bytes per frame and never the instance buffers.
//
// Coordinates: world units -> target pixels via (p - center) * zoom + origin, where origin
// is the canvas centre in target pixels. Colours arrive as gamma-space RGBA8 (straight
// alpha) and leave premultiplied; for sRGB targets they are decoded to linear first.

struct Camera {
    center: vec2<f32>,
    zoom: f32,
    srgb: f32,          // 1.0 if the target is *Srgb (decode colours to linear)
    origin: vec2<f32>,  // canvas centre, target pixels
    viewport: vec2<f32>, // render target size, pixels
    text: vec4<f32>,    // label line height, padding, ascent (pixels), unused
};

@group(0) @binding(0) var<uniform> cam: Camera;
@group(0) @binding(1) var atlas: texture_2d<f32>;

// Must match hit::CURVE_SEGMENTS.
const CURVE_SEGMENTS: u32 = 16u;

fn to_px(p: vec2<f32>) -> vec2<f32> {
    return (p - cam.center) * cam.zoom + cam.origin;
}

fn px_to_clip(px: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(px.x / cam.viewport.x * 2.0 - 1.0, 1.0 - px.y / cam.viewport.y * 2.0, 0.0, 1.0);
}

fn decode(c: vec3<f32>) -> vec3<f32> {
    if cam.srgb < 0.5 {
        return c;
    }
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn premul(c: vec4<f32>, coverage: f32) -> vec4<f32> {
    let a = c.a * coverage;
    return vec4<f32>(decode(c.rgb) * a, a);
}

// Off-screen position for culled vertices (whole primitive is clipped).
const CULLED: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

fn corner(i: u32) -> vec2<f32> {
    return vec2<f32>(f32(i & 1u), f32(i >> 1u));
}

// ---------------------------------------------------------------- rects (rounded, SDF)

struct RectIn {
    @location(0) min: vec2<f32>,
    @location(1) max: vec2<f32>,
    @location(2) fill: vec4<f32>,
    @location(3) stroke: vec4<f32>,
    @location(4) radius: f32,
    @location(5) stroke_px: f32,
};

struct RectOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,  // pixels from the rect centre
    @location(1) @interpolate(flat) half: vec2<f32>,
    @location(2) @interpolate(flat) radius: f32,
    @location(3) @interpolate(flat) stroke_px: f32,
    @location(4) @interpolate(flat) fill: vec4<f32>,
    @location(5) @interpolate(flat) stroke: vec4<f32>,
};

@vertex
fn vs_rect(@builtin(vertex_index) vi: u32, r: RectIn) -> RectOut {
    let a = to_px(r.min);
    let b = to_px(r.max);
    let center = (a + b) * 0.5;
    let half = max((b - a) * 0.5, vec2<f32>(0.0));
    // One pixel of margin for anti-aliasing.
    let ext = half + vec2<f32>(1.0);
    let local = (corner(vi) * 2.0 - 1.0) * ext;
    var o: RectOut;
    o.pos = px_to_clip(center + local);
    o.local = local;
    o.half = half;
    o.radius = clamp(r.radius, 0.0, min(half.x, half.y));
    o.stroke_px = max(r.stroke_px, 0.0);
    o.fill = r.fill;
    o.stroke = r.stroke;
    return o;
}

fn sd_round_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_rect(i: RectOut) -> @location(0) vec4<f32> {
    let d = sd_round_box(i.local, i.half, i.radius);
    let outer = clamp(0.5 - d, 0.0, 1.0);
    // Inner stroke: coverage of the shape minus coverage of the shape shrunk by stroke_px.
    let inner = clamp(0.5 - (d + i.stroke_px), 0.0, 1.0);
    let s = (outer - inner) * i.stroke.a;
    let f = inner * i.fill.a;
    let rgb = decode(i.stroke.rgb) * s + decode(i.fill.rgb) * f * (1.0 - s);
    let a = s + f * (1.0 - s);
    if a <= 0.0 {
        discard;
    }
    return vec4<f32>(rgb, a);
}

// ---------------------------------------------------------------- edges (Bezier ribbons)

struct EdgeIn {
    @location(0) p0: vec2<f32>,
    @location(1) c0: vec2<f32>,
    @location(2) c1: vec2<f32>,
    @location(3) p1: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) width: f32,
};

struct EdgeOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) across: f32,  // signed pixels from the centre line
    @location(1) @interpolate(flat) half_width: f32,
    @location(2) @interpolate(flat) color: vec4<f32>,
};

fn edge_vertex(vi: u32, e: EdgeIn, segments: u32) -> EdgeOut {
    let seg = vi >> 1u;
    let side = select(-1.0, 1.0, (vi & 1u) == 1u);
    let t = f32(seg) / f32(segments);
    let u = 1.0 - t;
    let p = to_px(e.p0) * (u * u * u) + to_px(e.c0) * (3.0 * u * u * t) + to_px(e.c1) * (3.0 * u * t * t) + to_px(e.p1) * (t * t * t);
    var d = (to_px(e.c0) - to_px(e.p0)) * (3.0 * u * u) + (to_px(e.c1) - to_px(e.c0)) * (6.0 * u * t) + (to_px(e.p1) - to_px(e.c1)) * (3.0 * t * t);
    if dot(d, d) < 1e-12 {
        d = to_px(e.p1) - to_px(e.p0);
    }
    if dot(d, d) < 1e-12 {
        d = vec2<f32>(1.0, 0.0);
    }
    let n = normalize(vec2<f32>(-d.y, d.x));
    // Ribbons narrower than a pixel stay one pixel wide and fade instead.
    let w = max(e.width, 1.0);
    let half = w * 0.5 + 1.0;
    var o: EdgeOut;
    o.pos = px_to_clip(p + n * (half * side));
    o.across = half * side;
    o.half_width = w * 0.5;
    o.color = vec4<f32>(e.color.rgb, e.color.a * min(e.width, 1.0));
    return o;
}

@vertex
fn vs_curve(@builtin(vertex_index) vi: u32, e: EdgeIn) -> EdgeOut {
    return edge_vertex(vi, e, CURVE_SEGMENTS);
}

@vertex
fn vs_line(@builtin(vertex_index) vi: u32, e: EdgeIn) -> EdgeOut {
    return edge_vertex(vi, e, 1u);
}

@fragment
fn fs_edge(i: EdgeOut) -> @location(0) vec4<f32> {
    let cov = clamp(i.half_width + 0.5 - abs(i.across), 0.0, 1.0);
    if cov <= 0.0 {
        discard;
    }
    return premul(i.color, cov);
}

// ---------------------------------------------------------------- glyphs (atlas labels)

struct GlyphIn {
    @location(0) rect_min: vec2<f32>,
    @location(1) rect_max: vec2<f32>,
    @location(2) offset: vec2<i32>,
    @location(3) size: vec2<u32>,
    @location(4) uv: vec2<u32>,
    @location(5) color: vec4<f32>,
};

struct GlyphOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) texel: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, g: GlyphIn) -> GlyphOut {
    let a = to_px(g.rect_min);
    let b = to_px(g.rect_max);
    let line_h = cam.text.x;
    let pad = cam.text.y;
    let ascent = cam.text.z;
    // Pen origin on the baseline, snapped to whole pixels so glyphs map 1:1 to texels.
    let pen = floor(a + vec2<f32>(pad, pad + ascent) + 0.5);
    let tl = pen + vec2<f32>(g.offset);
    let size = vec2<f32>(g.size);
    var o: GlyphOut;
    o.color = g.color;
    // Label LOD: the rect must be tall enough for a line and the glyph must fit inside.
    if b.y - a.y < line_h + 2.0 * pad || tl.x + size.x > b.x - pad {
        o.pos = CULLED;
        o.texel = vec2<f32>(0.0);
        return o;
    }
    let c = corner(vi);
    o.pos = px_to_clip(tl + c * size);
    o.texel = vec2<f32>(g.uv) + c * size;
    return o;
}

@fragment
fn fs_glyph(i: GlyphOut) -> @location(0) vec4<f32> {
    let cov = textureLoad(atlas, vec2<i32>(floor(i.texel)), 0).r;
    if cov <= 0.0 {
        discard;
    }
    return premul(i.color, cov);
}
