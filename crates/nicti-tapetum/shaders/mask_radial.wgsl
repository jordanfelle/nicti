// Radial-gradient mask weight (#49): 1 inside a rotated ellipse, linear to 0 over `feather`
// pixels beyond it (converted to ellipse units through the mean radius so one width works along
// both axes). CPU twin: mask::raster::radial_weight.

struct Uniforms {
    a: vec4<f32>,    // center.xy, radii.xy (pixels)
    b: vec4<f32>,    // cos(angle), sin(angle), feather_norm, 0
    dims: vec4<f32>, // width, height, 0, 0
}

@group(0) @binding(0) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(1) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn mask_radial(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.dims.x) || gid.y >= u32(u.dims.y)) {
        return;
    }
    var w = 0.0;
    if (u.a.z > 0.0 && u.a.w > 0.0) {
        let d = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5) - u.a.xy;
        let rot_x = d.x * u.b.x + d.y * u.b.y;
        let rot_y = -d.x * u.b.y + d.y * u.b.x;
        let n = sqrt((rot_x / u.a.z) * (rot_x / u.a.z) + (rot_y / u.a.w) * (rot_y / u.a.w));
        w = clamp(1.0 - (n - 1.0) / u.b.z, 0.0, 1.0);
    }
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(w, 0.0, 0.0, 1.0));
}
