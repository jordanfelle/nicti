// The geometry/present pass (ADR-0044): a bilinear affine sample over the live suffix's own
// output only -- crop/rotate/zoom/pan never touches a baked or live node directly. Matches
// geometry.rs::sample_bilinear's texel-center convention and edge-clamp behavior exactly (proven
// by a GPU/CPU parity test, not just asserted).

struct Uniforms {
    // Affine2D: (x,y) -> (a*x + b*y + tx, c*x + d*y + ty), mapping an output pixel to the input
    // coordinate to sample.
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    tx: f32,
    ty: f32,
    // The output extent, supplied by the caller (who already knows it from `FrameTexture::extent`)
    // rather than queried via `textureDimensions(output_tex)` -- naga's HLSL backend (FXC) hits a
    // "redefinition of NagaRWDimensions2D" codegen bug when a shader queries dimensions on both a
    // `read` and a `write` storage texture (confirmed: `live_suffix.wgsl`, which only queries its
    // one `read` texture, compiles fine on the same Dx12/FXC path).
    out_width: u32,
    out_height: u32,
    // #380 effects. `n*` is the affine from a source coordinate to crop-normalized (u, v) -- the
    // inverse of the crop transform divided by the crop size (effects.rs::crop_norm) -- so a
    // vignette/grain pattern depends on position in the crop, never on the output pixel or tile.
    n_a: f32,
    n_b: f32,
    n_c: f32,
    n_d: f32,
    n_tx: f32,
    n_ty: f32,
    crop_w: f32,
    crop_h: f32,
    v_amount: f32,
    v_mid: f32,
    v_feather: f32,
    v_round: f32,
    v_highlights: f32,
    v_style: u32,   // 0 highlight priority, 1 color priority, 2 paint overlay
    g_amount: f32,
    g_size: f32,
    g_rough: f32,
    g_seed: u32,
    flags: u32,     // bit 0 = vignette, bit 1 = grain; 0 runs the exact pre-#380 path
    pad: u32,
}

@group(0) @binding(0) var input_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

// ---- #380 effects: every function mirrors effects.rs, which the parity test pins. ----

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);
const GRAIN_CELLS_FINE: f32 = 1500.0;
const GRAIN_CELLS_COARSE: f32 = 300.0;
const VIGNETTE_STOPS: f32 = 2.0;
const GRAIN_GAIN: f32 = 0.3;

fn luma_of(rgb: vec3<f32>) -> f32 {
    return dot(rgb, LUMA);
}

fn vignette_t(uv: vec2<f32>, dims: vec2<f32>) -> f32 {
    let p = (uv - vec2<f32>(0.5)) * 2.0;
    let ar = dims.x / dims.y;
    let m = max(ar, 1.0);
    let circle = vec2<f32>(p.x * ar / m, p.y / m);
    let r = max(u.v_round, 0.0);
    let q = p + (circle - p) * r;
    let n = 2.0 + 4.0 * max(-u.v_round, 0.0);
    let dist = pow(pow(abs(q.x), n) + pow(abs(q.y), n), 1.0 / n);
    let rn = dist / pow(2.0, 1.0 / n);
    let inner = u.v_mid * 0.85;
    let outer = inner + (0.05 + 0.95 * u.v_feather) * (1.1 - inner);
    return smoothstep(inner, outer, rn);
}

fn apply_vignette(rgb: vec3<f32>, uv: vec2<f32>, dims: vec2<f32>) -> vec3<f32> {
    let t = vignette_t(uv, dims);
    if (t <= 0.0) {
        return rgb;
    }
    let a = u.v_amount;
    if (u.v_style == 0u) {
        let protection = u.v_highlights * smoothstep(0.25, 1.5, luma_of(rgb));
        return rgb * exp2(a * VIGNETTE_STOPS * t * (1.0 - protection));
    }
    if (u.v_style == 1u) {
        let out = rgb * exp2(a * VIGNETTE_STOPS * t);
        let l = luma_of(out);
        let k = 1.0 + 0.5 * abs(a) * t;
        return vec3<f32>(l) + (out - vec3<f32>(l)) * k;
    }
    var target_v = 1.0;
    if (a < 0.0) {
        target_v = 0.0;
    }
    return rgb + (vec3<f32>(target_v) - rgb) * (t * abs(a));
}

fn pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    let h = pcg(pcg(bitcast<u32>(ix) ^ (seed * 0x9E3779B9u)) + bitcast<u32>(iy));
    return f32(h >> 8u) / 8388608.0 - 1.0;
}

fn value_noise(p: vec2<f32>, seed: u32) -> f32 {
    let f = floor(p);
    let ix = i32(f.x);
    let iy = i32(f.y);
    let t = p - f;
    let w = t * t * (3.0 - 2.0 * t);
    let v00 = lattice(ix, iy, seed);
    let v10 = lattice(ix + 1, iy, seed);
    let v01 = lattice(ix, iy + 1, seed);
    let v11 = lattice(ix + 1, iy + 1, seed);
    let top = v00 + (v10 - v00) * w.x;
    let bottom = v01 + (v11 - v01) * w.x;
    return top + (bottom - top) * w.y;
}

fn apply_grain(rgb: vec3<f32>, uv: vec2<f32>, dims: vec2<f32>) -> vec3<f32> {
    let long_edge = max(dims.x, dims.y);
    let cells = GRAIN_CELLS_FINE + (GRAIN_CELLS_COARSE - GRAIN_CELLS_FINE) * u.g_size;
    let p = vec2<f32>(uv.x * dims.x / long_edge * cells, uv.y * dims.y / long_edge * cells);
    let coarse = value_noise(p, u.g_seed);
    let fine = value_noise(p * 2.1 + vec2<f32>(17.3), u.g_seed ^ 0x68E31DA4u);
    let noise = coarse + (fine - coarse) * u.g_rough;
    let g_now = clamp(pow(max(luma_of(rgb), 0.0), 1.0 / 3.0), 0.0, 1.0);
    let weight = 4.0 * g_now * (1.0 - g_now) + 0.15;
    let amp = u.g_amount * GRAIN_GAIN * weight;
    let g_base = max(g_now, 1e-3);
    let g_new = max(g_base + amp * noise, 0.0);
    let ratio = clamp(pow(g_new / g_base, 3.0), 0.25, 4.0);
    return rgb * ratio;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u.out_width || gid.y >= u.out_height) {
        return;
    }
    let in_dims = vec2<f32>(textureDimensions(input_tex));
    let ox = f32(gid.x) + 0.5;
    let oy = f32(gid.y) + 0.5;
    var sx = u.a * ox + u.b * oy + u.tx;
    var sy = u.c * ox + u.d * oy + u.ty;
    // The unclamped source position, for the crop-normalized effects below.
    let raw = vec2<f32>(sx, sy);
    sx = clamp(sx - 0.5, 0.0, in_dims.x - 1.0);
    sy = clamp(sy - 0.5, 0.0, in_dims.y - 1.0);

    let x0 = u32(floor(sx));
    let y0 = u32(floor(sy));
    let x1 = min(x0 + 1u, u32(in_dims.x) - 1u);
    let y1 = min(y0 + 1u, u32(in_dims.y) - 1u);
    let fx = sx - floor(sx);
    let fy = sy - floor(sy);

    let c00 = textureLoad(input_tex, vec2<i32>(i32(x0), i32(y0)));
    let c10 = textureLoad(input_tex, vec2<i32>(i32(x1), i32(y0)));
    let c01 = textureLoad(input_tex, vec2<i32>(i32(x0), i32(y1)));
    let c11 = textureLoad(input_tex, vec2<i32>(i32(x1), i32(y1)));

    let top = mix(c00, c10, fx);
    let bottom = mix(c01, c11, fx);
    var result = mix(top, bottom, fy);
    if (u.flags != 0u) {
        let uv = vec2<f32>(
            u.n_a * raw.x + u.n_b * raw.y + u.n_tx,
            u.n_c * raw.x + u.n_d * raw.y + u.n_ty,
        );
        let dims = vec2<f32>(u.crop_w, u.crop_h);
        var rgb = result.rgb;
        if ((u.flags & 1u) != 0u) {
            rgb = apply_vignette(rgb, uv, dims);
        }
        if ((u.flags & 2u) != 0u) {
            rgb = apply_grain(rgb, uv, dims);
        }
        result = vec4<f32>(rgb, result.a);
    }
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), result);
}
