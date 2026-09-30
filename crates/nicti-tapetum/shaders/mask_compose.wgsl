// Folds one component's weight field into the running composite (#49): optional invert, opacity,
// then add (union/max) / subtract / intersect. Ping-pong: reads `acc_in`, writes `acc_out`.
// CPU twin: mask::compose::fold_step.

struct Uniforms {
    p: vec4<f32>,    // invert (0/1), opacity, op (0 add, 1 subtract, 2 intersect), 0
    dims: vec4<f32>, // width, height, 0, 0
}

@group(0) @binding(0) var acc_in: texture_2d<f32>;
@group(0) @binding(1) var weight_tex: texture_2d<f32>;
@group(0) @binding(2) var acc_out: texture_storage_2d<r32float, write>;
@group(0) @binding(3) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn mask_compose(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.dims.x) || gid.y >= u32(u.dims.y)) {
        return;
    }
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    let acc = textureLoad(acc_in, px, 0).r;
    var w = clamp(textureLoad(weight_tex, px, 0).r, 0.0, 1.0);
    if (u.p.x > 0.5) {
        w = 1.0 - w;
    }
    w = w * u.p.y;
    var out = max(acc, w);
    if (u.p.z > 0.5 && u.p.z < 1.5) {
        out = acc * (1.0 - w);
    } else if (u.p.z >= 1.5) {
        out = acc * w;
    }
    textureStore(acc_out, px, vec4<f32>(out, 0.0, 0.0, 1.0));
}
