// Packs up to four finished correction composites into one RGBA atlas layer (#49), one per
// channel, so the live shader reads four masks with one sample. Write-only (a storage texture
// can't be read-modify-written safely across backends -- see ADR-0051), so all four channels are
// written at once; unused channels are bound to a zero dummy and come out 0.

struct Uniforms {
    dims: vec4<f32>, // width, height, 0, 0
}

@group(0) @binding(0) var m0: texture_2d<f32>;
@group(0) @binding(1) var m1: texture_2d<f32>;
@group(0) @binding(2) var m2: texture_2d<f32>;
@group(0) @binding(3) var m3: texture_2d<f32>;
@group(0) @binding(4) var out_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn mask_pack(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.dims.x) || gid.y >= u32(u.dims.y)) {
        return;
    }
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    // A dummy is 1x1; clamp its load to (0,0) so a wide extent doesn't read out of bounds.
    let d0 = textureDimensions(m0);
    let d1 = textureDimensions(m1);
    let d2 = textureDimensions(m2);
    let d3 = textureDimensions(m3);
    let a = textureLoad(m0, min(px, vec2<i32>(d0) - vec2<i32>(1, 1)), 0).r;
    let b = textureLoad(m1, min(px, vec2<i32>(d1) - vec2<i32>(1, 1)), 0).r;
    let c = textureLoad(m2, min(px, vec2<i32>(d2) - vec2<i32>(1, 1)), 0).r;
    let d = textureLoad(m3, min(px, vec2<i32>(d3) - vec2<i32>(1, 1)), 0).r;
    textureStore(out_tex, px, vec4<f32>(a, b, c, d));
}
