// Pass 1: Malvar-He-Cutler 5x5 gradient-corrected debayer + levels + white balance + colour matrix
// (camera RGB -> linear Rec.709). Writes linear Rgba16Float at sensor resolution.
// (Pass 2 lives in display.wgsl.)

struct Params {
    size: vec2<u32>,
    cfa_shift: vec2<u32>,   // sensor-pixel parity offset of the CFA (unused = 0)
    cfa: vec4<u32>,         // colours of (0,0),(1,0),(0,1),(1,1): 0 = R, 1 = G, 2 = B
    black: f32,
    inv_range: f32,         // 1 / (white - black)
    exposure: f32,          // linear gain (2^stops)
    _pad: f32,
    wb: vec4<f32>,          // R,G,B gains (G = 1)
    m0: vec4<f32>,          // colour matrix rows (xyz used)
    m1: vec4<f32>,
    m2: vec4<f32>,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var bayer: texture_2d<u32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Fullscreen triangle.
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return o;
}

fn mirror(i: i32, n: i32) -> i32 {
    var r = i;
    if (r < 0) { r = -r; }
    if (r >= n) { r = 2 * (n - 1) - r; }
    return clamp(r, 0, n - 1);
}

fn tap(x: i32, y: i32) -> f32 {
    let w = i32(p.size.x);
    let h = i32(p.size.y);
    let v = textureLoad(bayer, vec2<i32>(mirror(x, w), mirror(y, h)), 0).r;
    return f32(v);
}

fn cfa_at(x: i32, y: i32) -> u32 {
    let px = u32(x + i32(p.cfa_shift.x)) & 1u;
    let py = u32(y + i32(p.cfa_shift.y)) & 1u;
    return p.cfa[py * 2u + px];
}

@fragment
fn fs_debayer(in: VsOut) -> @location(0) vec4<f32> {
    let x = i32(in.pos.x);
    let y = i32(in.pos.y);
    let c0 = cfa_at(x, y);

    let c  = tap(x, y);
    let l1 = tap(x - 1, y);
    let r1 = tap(x + 1, y);
    let u1 = tap(x, y - 1);
    let d1 = tap(x, y + 1);
    let l2 = tap(x - 2, y);
    let r2 = tap(x + 2, y);
    let u2 = tap(x, y - 2);
    let d2 = tap(x, y + 2);

    // Build `rgb` with select() instead of dynamically indexed writes (`rgb[i] = ...`):
    // D3D12's FXC compiler rejects those ("array reference cannot be used as an l-value").
    var rgb: vec3<f32>;
    if (c0 != 1u) {
        // R or B site: green by the cross kernel, the opposite chroma by the diagonal kernel.
        let g = (4.0 * c + 2.0 * (l1 + r1 + u1 + d1) - (l2 + r2 + u2 + d2)) / 8.0;
        let diag = tap(x - 1, y - 1) + tap(x + 1, y - 1) + tap(x - 1, y + 1) + tap(x + 1, y + 1);
        let o = (6.0 * c + 2.0 * diag - 1.5 * (l2 + r2 + u2 + d2)) / 8.0;
        // Red site -> (c, g, o); blue site -> (o, g, c).
        rgb = select(vec3<f32>(o, g, c), vec3<f32>(c, g, o), c0 == 0u);
    } else {
        // Green site: chroma in the same row (horizontal) and in the same column (vertical).
        let dg = tap(x - 1, y - 1) + tap(x + 1, y - 1) + tap(x - 1, y + 1) + tap(x + 1, y + 1);
        let hch = (5.0 * c - dg - l2 - r2 + 0.5 * (u2 + d2) + 4.0 * (l1 + r1)) / 8.0;
        let vch = (5.0 * c - dg - u2 - d2 + 0.5 * (l2 + r2) + 4.0 * (u1 + d1)) / 8.0;
        // Red neighbour in this row -> (hch, c, vch); blue neighbour in this row -> (vch, c, hch).
        rgb = select(vec3<f32>(vch, c, hch), vec3<f32>(hch, c, vch), cfa_at(x + 1, y) == 0u);
    }

    var cam = max((rgb - vec3<f32>(p.black)) * p.inv_range, vec3<f32>(0.0)) * p.wb.xyz;
    let out = vec3<f32>(dot(p.m0.xyz, cam), dot(p.m1.xyz, cam), dot(p.m2.xyz, cam)) * p.exposure;
    return vec4<f32>(max(out, vec3<f32>(0.0)), 1.0);
}

