// Pass 2: sample the linear image (bilinear, aspect-fitted viewport) and apply the Rec.709 OETF.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return o;
}

@group(0) @binding(0) var lin_tex: texture_2d<f32>;
@group(0) @binding(1) var lin_samp: sampler;

fn oetf709(v: f32) -> f32 {
    let c = clamp(v, 0.0, 1.0);
    if (c < 0.018) { return 4.5 * c; }
    return 1.099 * pow(c, 0.45) - 0.099;
}

@fragment
fn fs_display(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(lin_tex, lin_samp, in.uv).rgb;
    return vec4<f32>(oetf709(c.r), oetf709(c.g), oetf709(c.b), 1.0);
}
