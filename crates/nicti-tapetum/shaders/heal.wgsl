// Clone/heal spot passes (#51, promoted from spikes/groom's `poisson_jacobi.wgsl`, now on
// `rgba16float` storage textures instead of storage buffers). One spot is applied as:
//
//   extract   frame -> dst_patch (frame around `center`) and guid_patch (frame around
//             `src_center`), both `side` x `side`, edge-clamped at the frame border
//   jacobi    N ping-pong sweeps of the Poisson solve over the patch (Heal only)
//   composite frame + solved patch -> result patch (feathered radial blend); the host then copies
//             the in-bounds part of `result` back into the frame
//
// Reading and writing the frame in different passes (never one read_write storage texture) is what
// keeps this off the optional `read_write` storage-texture format feature. Every entry point
// mirrors `heal.rs`'s CPU reference exactly -- keep the two in sync.
//
// Dimensions come from `Params`, never `textureDimensions`: a shader that binds both a read and a
// write storage texture can't use `textureDimensions` under FXC on Dx12 (see present_sample.wgsl).
// Binding numbers are shared across entry points (0/1/2 textures, 3 params); each entry point's
// auto-derived layout only contains the bindings it actually references.

struct Params {
    center: vec2<i32>,
    src_center: vec2<i32>,
    side: i32,
    half: i32,
    radius: f32,
    feather: f32,
    opacity: f32,
    frame_w: i32,
    frame_h: i32,
    _pad: u32,
}

@group(0) @binding(0) var tex0: texture_storage_2d<rgba16float, read>;
@group(0) @binding(1) var tex1_w: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var tex2: texture_storage_2d<rgba16float, read>;
@group(0) @binding(3) var<uniform> p: Params;

fn feather_weight(dist: f32, radius: f32, feather_in: f32) -> f32 {
    if (radius <= 0.0) { return 0.0; }
    if (dist >= radius) { return 0.0; }
    let feather = min(max(feather_in, 0.0), radius);
    let inner = radius - feather;
    if (dist <= inner) { return 1.0; }
    return clamp((radius - dist) / (radius - inner), 0.0, 1.0);
}

fn in_patch(gid: vec3<u32>) -> bool {
    return i32(gid.x) < p.side && i32(gid.y) < p.side;
}

fn frame_px(base: vec2<i32>, gid: vec3<u32>) -> vec4<f32> {
    let c = base + vec2<i32>(i32(gid.x) - p.half, i32(gid.y) - p.half);
    let cc = vec2<i32>(clamp(c.x, 0, p.frame_w - 1), clamp(c.y, 0, p.frame_h - 1));
    return textureLoad(tex0, cc);
}

// tex0 = frame (read); tex1_w = dst patch (write); the guidance patch is written by
// `extract_guidance` below (a second entry point, since one entry point can only bind one write
// texture per binding number).
@compute @workgroup_size(8, 8, 1)
fn extract_dst(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), frame_px(p.center, gid));
}

@compute @workgroup_size(8, 8, 1)
fn extract_guidance(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), frame_px(p.src_center, gid));
}

// One Jacobi sweep (Perez et al. 2003 "seamless cloning"). tex0 = current patch (read),
// tex2 = guidance patch (read), tex1_w = next patch (write). Pixels outside the radius are fixed
// boundary conditions; alpha always passes through.
@compute @workgroup_size(8, 8, 1)
fn jacobi(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let here = textureLoad(tex0, vec2<i32>(x, y));
    let fx = f32(x - p.half);
    let fy = f32(y - p.half);
    if (sqrt(fx * fx + fy * fy) >= p.radius) {
        textureStore(tex1_w, vec2<i32>(x, y), here);
        return;
    }
    let g_here = textureLoad(tex2, vec2<i32>(x, y)).xyz;
    var sum_f = vec3<f32>(0.0);
    var sum_g = vec3<f32>(0.0);
    var n = 0.0;
    if (x > 0) {
        sum_f += textureLoad(tex0, vec2<i32>(x - 1, y)).xyz;
        sum_g += g_here - textureLoad(tex2, vec2<i32>(x - 1, y)).xyz;
        n += 1.0;
    }
    if (x + 1 < p.side) {
        sum_f += textureLoad(tex0, vec2<i32>(x + 1, y)).xyz;
        sum_g += g_here - textureLoad(tex2, vec2<i32>(x + 1, y)).xyz;
        n += 1.0;
    }
    if (y > 0) {
        sum_f += textureLoad(tex0, vec2<i32>(x, y - 1)).xyz;
        sum_g += g_here - textureLoad(tex2, vec2<i32>(x, y - 1)).xyz;
        n += 1.0;
    }
    if (y + 1 < p.side) {
        sum_f += textureLoad(tex0, vec2<i32>(x, y + 1)).xyz;
        sum_g += g_here - textureLoad(tex2, vec2<i32>(x, y + 1)).xyz;
        n += 1.0;
    }
    if (n > 0.0) {
        textureStore(tex1_w, vec2<i32>(x, y), vec4<f32>((sum_f + sum_g) / n, here.w));
    } else {
        textureStore(tex1_w, vec2<i32>(x, y), here);
    }
}

// AI removal (#51): tex0 = frame (read), tex2 = a pre-inpainted patch (read; rgb = the fill, in the
// frame's own space, a = per-pixel fill weight already carrying the mask's feather), tex1_w =
// result patch (write). Same patch <-> frame mapping as `composite`; only the weight differs --
// it comes from the patch's alpha (times the spot's opacity) instead of a radial feather.
@compute @workgroup_size(8, 8, 1)
fn composite_patch(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    let fp = p.center + vec2<i32>(i32(gid.x) - p.half, i32(gid.y) - p.half);
    if (fp.x < 0 || fp.y < 0 || fp.x >= p.frame_w || fp.y >= p.frame_h) {
        textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(0.0));
        return;
    }
    let dst = textureLoad(tex0, fp);
    let fill = textureLoad(tex2, vec2<i32>(i32(gid.x), i32(gid.y)));
    let w = clamp(fill.w, 0.0, 1.0) * p.opacity;
    textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(dst.xyz + (fill.xyz - dst.xyz) * w, dst.w));
}

// tex0 = frame (read), tex2 = solved patch (read), tex1_w = result patch (write). Out-of-frame
// patch pixels are written as zeros and never copied back.
@compute @workgroup_size(8, 8, 1)
fn composite(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    let dx = i32(gid.x) - p.half;
    let dy = i32(gid.y) - p.half;
    let fp = p.center + vec2<i32>(dx, dy);
    if (fp.x < 0 || fp.y < 0 || fp.x >= p.frame_w || fp.y >= p.frame_h) {
        textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(0.0));
        return;
    }
    let dst = textureLoad(tex0, fp);
    let solved = textureLoad(tex2, vec2<i32>(i32(gid.x), i32(gid.y)));
    let dist = sqrt(f32(dx * dx + dy * dy));
    let w = feather_weight(dist, p.radius, p.feather) * p.opacity;
    textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), dst + (solved - dst) * w);
}
