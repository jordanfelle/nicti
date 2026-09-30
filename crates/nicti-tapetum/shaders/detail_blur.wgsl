// One separable-blur pass (horizontal or vertical, picked by `direction`) shared by both #46
// Noise Reduction and Sharpening's own reference blur -- see `stages.rs::LiveSuffixKernel`'s own
// doc comment for why this needs its own compute pass at all (unlike every other live stage, a
// blur needs neighboring pixels, not just this pixel's own value).
//
// The weight array is a fixed-size, zero-padded-beyond-its-own-radius kernel built on the CPU by
// `detail.rs::gaussian_kernel` -- this shader and that function must always agree on both the tap
// count and the exact weight values, which is what `stages.rs`'s GPU-vs-CPU parity test proves.

struct Uniforms {
    // x: 0 = horizontal (blur along x), 1 = vertical (blur along y). yzw unused.
    direction: vec4<u32>,
    // detail::MAX_BLUR_RADIUS taps each side of center, packed 4-per-vec4 (WGSL's uniform-address
    // -space array rules force this -- see `live_suffix.wgsl`'s own `curve_lut` for the same
    // packing convention).
    weights: array<vec4<f32>, 9>,
}

// Read through an ordinary sampled texture (`texture_2d` + `textureLoad`), never a read-mode storage
// texture: that binding produced garbage on the RTX 5080 under Dx12 (ADR-0051, found again in #49 --
// `stages::tests::full_pipeline_end_to_end...` failed on real Dx12 hardware on `main` itself).
@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

const RADIUS: i32 = 17;

fn weight_at(index: i32) -> f32 {
    let group = u.weights[index / 4];
    let comp = index % 4;
    if (comp == 0) { return group.x; }
    if (comp == 1) { return group.y; }
    if (comp == 2) { return group.z; }
    return group.w;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let horizontal = u.direction.x == 0u;
    var acc = vec4<f32>(0.0);
    for (var i: i32 = -RADIUS; i <= RADIUS; i = i + 1) {
        let w = weight_at(i + RADIUS);
        var coord = vec2<i32>(i32(gid.x), i32(gid.y));
        if (horizontal) {
            coord.x = clamp(coord.x + i, 0, i32(dims.x) - 1);
        } else {
            coord.y = clamp(coord.y + i, 0, i32(dims.y) - 1);
        }
        acc = acc + w * textureLoad(input_tex, coord, 0);
    }
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), acc);
}
