// Fullscreen triangle sampling the live_chain output texture via textureLoad (no Sampler --
// Rgba32Float isn't filterable without the FLOAT32_FILTERABLE feature, and this spike only needs
// a 1:1 blit, not filtered scaling).

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = positions[vertex_index];
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    return out;
}

@group(0) @binding(0) var viewport_tex: texture_2d<f32>;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(viewport_tex));
    let coord = vec2<i32>(in.uv * dims);
    return textureLoad(viewport_tex, coord, 0);
}
