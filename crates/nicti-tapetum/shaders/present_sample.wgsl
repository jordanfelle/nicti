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
}

@group(0) @binding(0) var input_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

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
    let result = mix(top, bottom, fy);
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), result);
}
