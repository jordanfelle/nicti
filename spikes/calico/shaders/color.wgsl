// GPU port of huesatmap.rs's single-map sample + apply (dual-map illuminant blending and the
// hue-wrap-aware angle interpolation at wraparound-discontinuous hue-shift values stay CPU-only
// -- see gpu.rs's module doc for why). Input/output are vec4<f32> (rgb + unused pad), already in
// gamma-encoded ("1/1.8") linear-ProPhoto-RGB space per pipeline.rs's stage order.

@group(0) @binding(0) var<storage, read> input_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> output_pixels: array<vec4<f32>>;
@group(0) @binding(2) var hue_sat_map: texture_3d<f32>;
@group(0) @binding(3) var hue_sat_sampler: sampler;

fn rgb_to_hsv(rgb: vec3<f32>) -> vec3<f32> {
    let max_c = max(rgb.r, max(rgb.g, rgb.b));
    let min_c = min(rgb.r, min(rgb.g, rgb.b));
    let delta = max_c - min_c;
    var h: f32 = 0.0;
    if (delta > 1e-6) {
        if (max_c == rgb.r) {
            h = 60.0 * (((rgb.g - rgb.b) / delta) % 6.0);
        } else if (max_c == rgb.g) {
            h = 60.0 * ((rgb.b - rgb.r) / delta + 2.0);
        } else {
            h = 60.0 * ((rgb.r - rgb.g) / delta + 4.0);
        }
    }
    if (h < 0.0) {
        h = h + 360.0;
    }
    let s = select(0.0, delta / max_c, max_c > 0.0);
    return vec3<f32>(h, s, max_c);
}

fn hsv_to_rgb(hsv: vec3<f32>) -> vec3<f32> {
    // Wrap into [0, 360) -- new_hsv.x (hue + a HueSatMap shift) can land outside that range when
    // the shift crosses the 0/360 seam, and WGSL's `%` keeps the dividend's sign (unlike the CPU
    // `rem_euclid` in huesatmap.rs), so an unwrapped negative h picks the wrong sector below.
    let h = hsv.x - 360.0 * floor(hsv.x / 360.0);
    let s = clamp(hsv.y, 0.0, 1.0);
    let v = hsv.z;
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - abs((hp % 2.0) - 1.0));
    var rgb1: vec3<f32>;
    let sector = i32(hp);
    if (sector == 0) {
        rgb1 = vec3<f32>(c, x, 0.0);
    } else if (sector == 1) {
        rgb1 = vec3<f32>(x, c, 0.0);
    } else if (sector == 2) {
        rgb1 = vec3<f32>(0.0, c, x);
    } else if (sector == 3) {
        rgb1 = vec3<f32>(0.0, x, c);
    } else if (sector == 4) {
        rgb1 = vec3<f32>(x, 0.0, c);
    } else {
        rgb1 = vec3<f32>(c, 0.0, x);
    }
    let m = v - c;
    return rgb1 + vec3<f32>(m, m, m);
}

@compute @workgroup_size(64, 1, 1)
fn apply_hue_sat_map(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) grid: vec3<u32>) {
    let flat_index = gid.y * grid.x * 64u + gid.x;
    if (flat_index >= arrayLength(&input_pixels)) {
        return;
    }
    let rgb = input_pixels[flat_index].rgb;
    let hsv = rgb_to_hsv(rgb);

    // Texture coordinates: gpu.rs's upload maps texture width -> saturation, height -> hue,
    // depth -> value (matching map.data's natural value-outer/hue-mid/saturation-inner memory
    // order with no transpose needed). Hue wraps via the sampler's Repeat address mode on v (u in
    // [0,1) maps to a full 360deg turn), sat/val clamp via ClampToEdge. Hardware trilinear
    // filtering treats texel i's *center* as sitting at (i+0.5)/N, not at i/N -- huesatmap.rs's
    // CPU sampler places table entry i's data exactly at grid position i/N (hue, a tiling axis)
    // or i/(N-1) (sat/val, an edge-to-edge axis), so both need a coordinate remap or the GPU
    // silently samples a half-texel off (confirmed: this was exactly the size of `gpu_parity`'s
    // first real mismatch, a bug an earlier draft here didn't catch until that test actually ran).
    let dims = vec3<f32>(textureDimensions(hue_sat_map));
    let u = select((hsv.y * (dims.x - 1.0) + 0.5) / dims.x, 0.5, dims.x <= 1.0);
    let v = hsv.x / 360.0 + 0.5 / dims.y;
    let w = select((hsv.z * (dims.z - 1.0) + 0.5) / dims.z, 0.5, dims.z <= 1.0);
    let adj = textureSampleLevel(hue_sat_map, hue_sat_sampler, vec3<f32>(u, v, w), 0.0).xyz;

    let new_hsv = vec3<f32>(hsv.x + adj.x, clamp(hsv.y * adj.y, 0.0, 1.0), hsv.z * adj.z);
    output_pixels[flat_index] = vec4<f32>(hsv_to_rgb(new_hsv), 0.0);
}
