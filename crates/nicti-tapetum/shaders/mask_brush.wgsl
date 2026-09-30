// One brush stroke folded into the running brush field (#49). Dispatched over the stroke's
// bounding box only (the caller copies `prev` into the output first, so everything outside the box
// is already right). Dabs are pre-binned on the CPU into 64x64 tiles, so a pixel only visits the
// dabs that can touch its tile instead of all of them. Dabs blend with `max`; an erase stroke
// subtracts. CPU twin: mask::raster::rasterize_brush.

struct Uniforms {
    origin: vec4<f32>, // bbox x0, y0, width, height (pixels)
    mode: vec4<f32>,   // tiles_x, erase (0/1), 0, 0
}

@group(0) @binding(0) var prev_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<storage, read> dabs: array<vec4<f32>>; // 2 per dab: (cx,cy,radius,feather), (flow,0,0,0)
@group(0) @binding(3) var<storage, read> tile_offsets: array<u32>;
@group(0) @binding(4) var<storage, read> tile_dabs: array<u32>;
@group(0) @binding(5) var<uniform> u: Uniforms;

fn dab_weight(i: u32, x: f32, y: f32) -> f32 {
    let a = dabs[i * 2u];
    let flow = dabs[i * 2u + 1u].x;
    let dist = sqrt((x - a.x) * (x - a.x) + (y - a.y) * (y - a.y));
    if (dist >= a.z) {
        return 0.0;
    }
    let inner = max(a.z - a.w, 0.0);
    var w = 1.0;
    if (dist > inner) {
        w = 1.0 - (dist - inner) / max(a.z - inner, 1e-6);
    }
    return clamp(w * flow, 0.0, 1.0);
}

@compute @workgroup_size(8, 8)
fn mask_brush(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.origin.z) || gid.y >= u32(u.origin.w)) {
        return;
    }
    let px = vec2<i32>(i32(u.origin.x) + i32(gid.x), i32(u.origin.y) + i32(gid.y));
    let fx = f32(px.x) + 0.5;
    let fy = f32(px.y) + 0.5;
    let tile = (gid.y / 64u) * u32(u.mode.x) + (gid.x / 64u);
    var s = 0.0;
    for (var k = tile_offsets[tile]; k < tile_offsets[tile + 1u]; k = k + 1u) {
        s = max(s, dab_weight(tile_dabs[k], fx, fy));
    }
    let prev = textureLoad(prev_tex, px, 0).r;
    var out = max(prev, s);
    if (u.mode.y > 0.5) {
        out = max(prev - s, 0.0);
    }
    textureStore(out_tex, px, vec4<f32>(out, 0.0, 0.0, 1.0));
}
