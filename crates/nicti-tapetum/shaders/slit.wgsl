// The baked lens-correction pass (#428): one bilinear resample of the (demosaiced, normalised,
// still camera-RGB) frame in which every colour channel is sampled at its own corrected source
// position, then scaled by the radial vignette gain. See `slit.rs` for the model and the CPU twin
// (`slit::reference`) this shader is proven against.
//
// Adapted from storytold/lightcraft@265248c crates/gpu/src/wgsl/geom.wgsl and
// crates/pipeline/src/optics.rs, Copyright (c) 2026 ArtCraft Team and the LightCraft
// contributors, MIT OR Apache-2.0 (see docs/licensing.md).
//
// For an output pixel centre `p` (continuous coordinates, pixel centres at +0.5):
//   1. inverse DNG WarpRectilinear, per colour plane: normalise `p - centre` by `m` (centre to the
//      farthest corner), radial polynomial + tangential terms -> the source position;
//   2. automatic lateral CA: red and blue are additionally magnified about the optical centre by
//      `1 + alpha` (green is the reference);
//   3. sample R, G, B bilinearly at their own positions (clamped at the frame edge);
//   4. multiply by the FixVignetteRadial gain evaluated at the *green* source position.
//
// Input is read through a sampled `texture_2d` + `textureLoad` (never a read-mode storage texture:
// ADR-0051 / #49, garbage on the RTX 5080 under Dx12) and written to a write-only storage texture.

const MAX_VIGNETTE_GAIN: f32 = 16.0;

struct Uniforms {
    // x: width, y: height (pixels), z: 1 when the warp is active, w: 1 when the vignette is.
    dims: vec4<f32>,
    // Warp optical centre in pixels (xy), normalisation radius in pixels (z).
    warp_c: vec4<f32>,
    // Vignette optical centre in pixels (xy), normalisation radius in pixels (z).
    vig_c: vec4<f32>,
    // Red and blue lateral-CA scales (xy), CA centre in pixels (zw).
    ca: vec4<f32>,
    // Per colour plane p (0 = R, 1 = G, 2 = B): [2p] = kr0..kr3, [2p + 1] = (kt0, kt1, 0, 0).
    warp: array<vec4<f32>, 6>,
    // Vignette k0..k3, then (k4, 0, 0, 0).
    vig_k: array<vec4<f32>, 2>,
}

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

fn load_clamped(x: i32, y: i32) -> vec3<f32> {
    let dims = vec2<i32>(textureDimensions(input_tex));
    let c = clamp(vec2<i32>(x, y), vec2<i32>(0, 0), dims - vec2<i32>(1, 1));
    return textureLoad(input_tex, c, 0).rgb;
}

// Bilinear sample at a continuous position (pixel centres at +0.5), edge-clamped.
fn bilinear(p: vec2<f32>) -> vec3<f32> {
    let q = p - vec2<f32>(0.5, 0.5);
    let f = floor(q);
    let t = q - f;
    let i = vec2<i32>(f);
    let a = load_clamped(i.x, i.y);
    let b = load_clamped(i.x + 1, i.y);
    let c = load_clamped(i.x, i.y + 1);
    let d = load_clamped(i.x + 1, i.y + 1);
    return mix(mix(a, b, t.x), mix(c, d, t.x), t.y);
}

fn warp_source(ch: u32, p: vec2<f32>) -> vec2<f32> {
    if (u.dims.z < 0.5) {
        return p;
    }
    let c = u.warp_c.xy;
    let m = u.warp_c.z;
    let d = (p - c) / m;
    let k = u.warp[2u * ch];
    let t = u.warp[2u * ch + 1u];
    let r2 = dot(d, d);
    let f = k.x + r2 * (k.y + r2 * (k.z + r2 * k.w));
    let sx = f * d.x + 2.0 * t.x * d.x * d.y + t.y * (r2 + 2.0 * d.x * d.x);
    let sy = f * d.y + t.x * (r2 + 2.0 * d.y * d.y) + 2.0 * t.y * d.x * d.y;
    return c + vec2<f32>(sx, sy) * m;
}

fn ca_source(alpha: f32, s: vec2<f32>) -> vec2<f32> {
    let c = u.ca.zw;
    return c + (s - c) * (1.0 + alpha);
}

fn vignette_gain(s: vec2<f32>) -> f32 {
    if (u.dims.w < 0.5) {
        return 1.0;
    }
    let d = (s - u.vig_c.xy) / u.vig_c.z;
    let r2 = dot(d, d);
    let a = u.vig_k[0];
    let b = u.vig_k[1];
    // Bounded: a hostile or corrupt profile must not turn the frame negative (which the live shader's
    // cube roots turn into NaN) or blow it out. Real vignette gains stay under ~4 stops.
    return clamp(1.0 + r2 * (a.x + r2 * (a.y + r2 * (a.z + r2 * (a.w + r2 * b.x)))), 0.0, MAX_VIGNETTE_GAIN);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let p = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let sr = ca_source(u.ca.x, warp_source(0u, p));
    let sg = warp_source(1u, p);
    let sb = ca_source(u.ca.y, warp_source(2u, p));
    let rgb = vec3<f32>(bilinear(sr).r, bilinear(sg).g, bilinear(sb).b) * vignette_gain(sg);
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, 1.0));
}
