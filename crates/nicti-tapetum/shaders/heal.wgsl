// Clone/heal spot passes (#51, promoted from spikes/groom's `poisson_jacobi.wgsl`, now on
// `rgba16float` storage textures instead of storage buffers). One spot is applied as:
//
//   extract   frame -> dst_patch (frame around `center`) and guid_patch (frame around
//             `src_center`), both `side` x `side`, edge-clamped at the frame border
//   boundary_mean / init_heal  (Heal only) mean of (dst - src) on the ring just outside the spot,
//             then interior := src + that mean. Jacobi converges in ~side^2 sweeps, far more than we
//             run, so it must not start from the destination: whatever blemish it started from would
//             survive the few sweeps we can afford. Starting from src + the boundary offset removes
//             the blemish up front and leaves Jacobi only the (smooth) boundary variation to refine.
//   jacobi    N ping-pong sweeps of the Poisson solve over the patch (Heal only)
//   composite frame + solved patch -> result patch (feathered radial blend); the host then copies
//             the in-bounds part of `result` back into the frame
//
// Dx12 note: wgpu's Dx12 backend does not reliably barrier a texture that goes write -> read ->
// write across compute passes touching it only as a storage texture (a Jacobi chain came back as
// garbage on real hardware while passing on Vulkan). The host therefore clears scratch textures
// per spot and copies each Jacobi sweep back rather than swapping roles -- see heal.rs.
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

// Inputs are ordinary sampled textures read with `textureLoad` (an SRV); only the outputs are
// storage textures, and write-only. Declaring the inputs as `texture_storage_2d<.., read>` (a UAV)
// made wgpu's Dx12 backend's resource-state tracking disagree with the shader's binding and
// produced wrong results on real hardware; keeping reads and writes in different binding kinds
// makes every hand-off between passes an unambiguous state transition.
@group(0) @binding(0) var tex0: texture_2d<f32>;
@group(0) @binding(1) var tex1_w: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var tex2: texture_2d<f32>;
@group(0) @binding(3) var<uniform> p: Params;
@group(0) @binding(4) var tex4: texture_2d<f32>;

fn ld0(c: vec2<i32>) -> vec4<f32> { return textureLoad(tex0, c, 0); }
fn ld2(c: vec2<i32>) -> vec4<f32> { return textureLoad(tex2, c, 0); }
fn ld4(c: vec2<i32>) -> vec4<f32> { return textureLoad(tex4, c, 0); }

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
    return ld0(cc);
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
    let here = ld0(vec2<i32>(x, y));
    let fx = f32(x - p.half);
    let fy = f32(y - p.half);
    if (sqrt(fx * fx + fy * fy) >= p.radius) {
        textureStore(tex1_w, vec2<i32>(x, y), here);
        return;
    }
    let g_here = ld2(vec2<i32>(x, y)).xyz;
    var sum_f = vec3<f32>(0.0);
    var sum_g = vec3<f32>(0.0);
    var n = 0.0;
    if (x > 0) {
        sum_f += ld0(vec2<i32>(x - 1, y)).xyz;
        sum_g += g_here - ld2(vec2<i32>(x - 1, y)).xyz;
        n += 1.0;
    }
    if (x + 1 < p.side) {
        sum_f += ld0(vec2<i32>(x + 1, y)).xyz;
        sum_g += g_here - ld2(vec2<i32>(x + 1, y)).xyz;
        n += 1.0;
    }
    if (y > 0) {
        sum_f += ld0(vec2<i32>(x, y - 1)).xyz;
        sum_g += g_here - ld2(vec2<i32>(x, y - 1)).xyz;
        n += 1.0;
    }
    if (y + 1 < p.side) {
        sum_f += ld0(vec2<i32>(x, y + 1)).xyz;
        sum_g += g_here - ld2(vec2<i32>(x, y + 1)).xyz;
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
    let dst = ld0(fp);
    let fill = ld2(vec2<i32>(i32(gid.x), i32(gid.y)));
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
    let dst = ld0(fp);
    let solved = ld2(vec2<i32>(i32(gid.x), i32(gid.y)));
    let dist = sqrt(f32(dx * dx + dy * dy));
    let w = feather_weight(dist, p.radius, p.feather) * p.opacity;
    textureStore(tex1_w, vec2<i32>(i32(gid.x), i32(gid.y)), dst + (solved - dst) * w);
}

// Width of the ring (outside the spot's radius) averaged for the boundary offset.
const RING_WIDTH: f32 = 1.5;

var<workgroup> partial_sum: array<vec4<f32>, 256>;
var<workgroup> partial_count: array<f32, 256>;

// One workgroup reduces the mean of (dst - src) over the ring [radius, radius + RING_WIDTH) and
// writes it to texel (0,0) of tex1_w. tex0 = dst patch, tex2 = guidance patch.
@compute @workgroup_size(256)
fn boundary_mean(@builtin(local_invocation_index) lid: u32) {
    var sum = vec4<f32>(0.0);
    var count = 0.0;
    let total = u32(p.side * p.side);
    for (var i = lid; i < total; i = i + 256u) {
        let x = i32(i) % p.side;
        let y = i32(i) / p.side;
        let fx = f32(x - p.half);
        let fy = f32(y - p.half);
        let d = sqrt(fx * fx + fy * fy);
        if (d >= p.radius && d < p.radius + RING_WIDTH) {
            sum += ld0(vec2<i32>(x, y)) - ld2(vec2<i32>(x, y));
            count += 1.0;
        }
    }
    partial_sum[lid] = sum;
    partial_count[lid] = count;
    workgroupBarrier();
    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (lid < stride) {
            partial_sum[lid] += partial_sum[lid + stride];
            partial_count[lid] += partial_count[lid + stride];
        }
        workgroupBarrier();
    }
    if (lid == 0u) {
        let n = max(partial_count[0], 1.0);
        textureStore(tex1_w, vec2<i32>(0, 0), vec4<f32>((partial_sum[0] / n).xyz, 0.0));
    }
}

// tex0 = dst patch, tex2 = guidance patch, tex4 = the 1x1 mean, tex1_w = initial patch.
// Interior := guidance + mean offset; everything else stays as the destination (the Dirichlet
// boundary the Jacobi sweeps hold fixed).
@compute @workgroup_size(8, 8, 1)
fn init_heal(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!in_patch(gid)) { return; }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let dst = ld0(vec2<i32>(x, y));
    let fx = f32(x - p.half);
    let fy = f32(y - p.half);
    if (sqrt(fx * fx + fy * fy) >= p.radius) {
        textureStore(tex1_w, vec2<i32>(x, y), dst);
        return;
    }
    let g = ld2(vec2<i32>(x, y));
    let mean = ld4(vec2<i32>(0, 0)).xyz;
    textureStore(tex1_w, vec2<i32>(x, y), vec4<f32>(g.xyz + mean, dst.w));
}
