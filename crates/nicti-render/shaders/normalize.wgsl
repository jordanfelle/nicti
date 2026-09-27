// Decode's own bake: reads a LinearFrame's interleaved u16 R/G/B pixel data (uploaded packed two
// u16 samples per u32 -- WGSL has no native u16 storage-buffer element type -- little-endian:
// sample 2n in the low 16 bits, sample 2n+1 in the high 16 bits), subtracts the black level,
// scales to roughly [0, 1], and writes an Rgba16Float texture -- the baked prefix's first node.

struct Params {
    width: u32,
    height: u32,
    black: u32,
    maximum: u32,
    cblack: vec4<u32>, // R, G, B, (unused G2)
}

@group(0) @binding(0) var<storage, read> packed_pixels: array<u32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> p: Params;

fn unpack_sample(i: u32) -> u32 {
    let packed = packed_pixels[i >> 1u];
    return (packed >> ((i & 1u) * 16u)) & 0xffffu;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.width || gid.y >= p.height) {
        return;
    }
    let idx = (gid.y * p.width + gid.x) * 3u;
    let range = f32(p.maximum) - f32(p.black);
    let r = (f32(unpack_sample(idx)) - f32(p.black) - f32(p.cblack.x)) / range;
    let g = (f32(unpack_sample(idx + 1u)) - f32(p.black) - f32(p.cblack.y)) / range;
    let b = (f32(unpack_sample(idx + 2u)) - f32(p.black) - f32(p.cblack.z)) / range;
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(r, g, b, 1.0));
}
