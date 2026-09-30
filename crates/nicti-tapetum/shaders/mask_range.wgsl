// Luminance / colour range mask (#49): selects pixels of the baked frame by how bright they are
// (mode 0) or how close they are to sampled colours (mode 1). The frame is camera-linear, so each
// tap goes camera -> working (ProPhoto) with the caller's matrix, then to a perceptual luma
// (Y^(1/2.2)) or to Lab (D50). The weight is computed per source tap and then bilinearly combined
// at the mask extent, so a mask smaller than the frame is smooth, not aliased. CPU twin:
// mask::raster::range_weight_at / range_field.

struct Uniforms {
    m0: vec4<f32>,   // mode, lo, hi, smooth
    m1: vec4<f32>,   // tolerance, sample count, mask w, mask h
    d: vec4<f32>,    // frame w, frame h, 0, 0
    col0: vec4<f32>, // camera -> working matrix columns
    col1: vec4<f32>,
    col2: vec4<f32>,
    samples: array<vec4<f32>, 16>, // Lab L, a, b, _
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

fn lab_f(t: f32) -> f32 {
    if (t > 0.008856) {
        return pow(t, 1.0 / 3.0);
    }
    return 7.787 * t + 16.0 / 116.0;
}

fn weight_at(px: vec2<i32>) -> f32 {
    let cam = textureLoad(frame_tex, px, 0).rgb;
    let working = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz) * cam;
    if (u.m0.x < 0.5) {
        let y = dot(working, vec3<f32>(0.2880402, 0.7118741, 0.0000857));
        let v = pow(clamp(y, 0.0, 1.0), 1.0 / 2.2);
        if (v >= u.m0.y && v <= u.m0.z) {
            return 1.0;
        }
        var outside = v - u.m0.z;
        if (v < u.m0.y) {
            outside = u.m0.y - v;
        }
        if (u.m0.w <= 0.0) {
            return 0.0;
        }
        return clamp(1.0 - outside / u.m0.w, 0.0, 1.0);
    }
    let xyz = vec3<f32>(
        dot(working, vec3<f32>(0.7976749, 0.1351917, 0.0313534)),
        dot(working, vec3<f32>(0.2880402, 0.7118741, 0.0000857)),
        dot(working, vec3<f32>(0.0, 0.0, 0.82521)),
    );
    let fx = lab_f(xyz.x / 0.9642);
    let fy = lab_f(xyz.y);
    let fz = lab_f(xyz.z / 0.8251);
    let lab = vec3<f32>(116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz));
    let n = i32(u.m1.y);
    if (n == 0 || u.m1.x <= 0.0) {
        return 0.0;
    }
    var nearest = 1.0e30;
    for (var i: i32 = 0; i < n; i = i + 1) {
        nearest = min(nearest, distance(lab, u.samples[i].xyz));
    }
    return clamp(1.0 - nearest / u.m1.x, 0.0, 1.0);
}

@compute @workgroup_size(8, 8)
fn mask_range(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.m1.z) || gid.y >= u32(u.m1.w)) {
        return;
    }
    let fx = (f32(gid.x) + 0.5) / u.m1.z * u.d.x - 0.5;
    let fy = (f32(gid.y) + 0.5) / u.m1.w * u.d.y - 0.5;
    let x0 = floor(fx);
    let y0 = floor(fy);
    let tx = fx - x0;
    let ty = fy - y0;
    let maxx = i32(u.d.x) - 1;
    let maxy = i32(u.d.y) - 1;
    let ix0 = clamp(i32(x0), 0, maxx);
    let ix1 = clamp(i32(x0) + 1, 0, maxx);
    let iy0 = clamp(i32(y0), 0, maxy);
    let iy1 = clamp(i32(y0) + 1, 0, maxy);
    let top = weight_at(vec2<i32>(ix0, iy0)) * (1.0 - tx) + weight_at(vec2<i32>(ix1, iy0)) * tx;
    let bottom = weight_at(vec2<i32>(ix0, iy1)) * (1.0 - tx) + weight_at(vec2<i32>(ix1, iy1)) * tx;
    let w = top * (1.0 - ty) + bottom * ty;
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(w, 0.0, 0.0, 1.0));
}
