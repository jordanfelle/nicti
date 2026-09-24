// Minimal SHADER_F16 capability probe: stores an f16 value computed from an f32 input into a
// packed-f16 storage buffer. Not part of the main throughput kernels (see gpu.rs's scoping
// note) -- exercised only by tests/features_and_limits.rs.

enable f16;

@group(0) @binding(0) var<storage, read> input_values: array<f32>;
@group(0) @binding(1) var<storage, read_write> output_values: array<f16>;

@compute @workgroup_size(64)
fn f16_probe(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= arrayLength(&input_values)) {
        return;
    }
    output_values[i] = f16(input_values[i]) * f16(2.0);
}
