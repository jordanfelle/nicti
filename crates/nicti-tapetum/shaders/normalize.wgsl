// Decode's own bake: reads a LinearFrame's interleaved u16 R/G/B pixel data (uploaded packed two
// u16 samples per u32 -- WGSL has no native u16 storage-buffer element type -- little-endian:
// sample 2n in the low 16 bits, sample 2n+1 in the high 16 bits), subtracts the black level,
// scales to roughly [0, 1], and writes an Rgba16Float texture -- the baked prefix's first node.
//
// Runs once per row-strip (DecodeExec splits a full frame into strips that each fit under the
// adapter's max_storage_buffer_binding_size -- a real full-res 45MP frame's packed pixel buffer
// can exceed a software adapter's limit, e.g. lavapipe's 128MB, well before real hardware's).
// `packed_pixels` holds only this strip's rows (row 0 of the buffer is row `row_offset` of the
// full frame); `row_offset` is what lets each strip's dispatch write to the correct absolute Y
// in `output_tex`, which is always the full frame's texture regardless of how many strips there
// are.

struct Params {
    dims: vec4<u32>,   // width, strip_rows, row_offset, black
    limits: vec4<u32>, // maximum, cblack.r, cblack.g, cblack.b
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
    let width = p.dims.x;
    let strip_rows = p.dims.y;
    let row_offset = p.dims.z;
    let black = p.dims.w;
    let maximum = p.limits.x;
    let cblack = p.limits.yzw;

    if (gid.x >= width || gid.y >= strip_rows) {
        return;
    }
    let idx = (gid.y * width + gid.x) * 3u;
    let range = f32(maximum) - f32(black);
    let r = (f32(unpack_sample(idx)) - f32(black) - f32(cblack.x)) / range;
    let g = (f32(unpack_sample(idx + 1u)) - f32(black) - f32(cblack.y)) / range;
    let b = (f32(unpack_sample(idx + 2u)) - f32(black) - f32(cblack.z)) / range;
    textureStore(
        output_tex,
        vec2<i32>(i32(gid.x), i32(gid.y + row_offset)),
        vec4<f32>(r, g, b, 1.0)
    );
}
