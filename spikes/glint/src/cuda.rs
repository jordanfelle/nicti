//! Raw CUDA (via NVRTC) implementation of the `live_chain` kernel, compiled and run through
//! `cudarc`'s driver + NVRTC bindings. This is the comparison point for the ADR-0005 decision
//! rule's "within 2x of the equivalent CUDA kernel" clause. `cudarc`'s `fallback-dynamic-loading`
//! feature means this compiles even without the CUDA toolkit installed; `CudaContext::new`
//! returns an error at runtime instead if the driver/NVRTC libraries aren't present, which every
//! caller here treats as "skip this test" rather than a hard failure.

use std::sync::Arc;

use cudarc::driver::{CudaContext, CudaFunction, CudaStream, LaunchConfig, PushKernelArg};

const LIVE_CHAIN_CU: &str = r#"
extern "C" __global__ void live_chain(
    const float4* input_pixels,
    float4* output_pixels,
    unsigned int count,
    float wb_r, float wb_g, float wb_b,
    float exposure_stops,
    float vibrance
) {
    unsigned int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;

    float4 px = input_pixels[i];
    float mul = exp2f(exposure_stops);
    float r = px.x * wb_r * mul;
    float g = px.y * wb_g * mul;
    float b = px.z * wb_b * mul;

    const float xs[5] = {0.0f, 0.25f, 0.5f, 0.75f, 1.0f};
    const float ys[5] = {0.02f, 0.22f, 0.5f, 0.80f, 0.98f};

    float rgb[3] = {r, g, b};
    #pragma unroll
    for (int c = 0; c < 3; c++) {
        float x = fminf(fmaxf(rgb[c], 0.0f), 1.0f);
        float out = ys[4];
        #pragma unroll
        for (int seg = 0; seg < 4; seg++) {
            if (x <= xs[seg + 1] || seg == 3) {
                float t = (x - xs[seg]) / (xs[seg + 1] - xs[seg]);
                out = ys[seg] + t * (ys[seg + 1] - ys[seg]);
                break;
            }
        }
        rgb[c] = out;
    }

    float mx = fmaxf(rgb[0], fmaxf(rgb[1], rgb[2]));
    float mn = fminf(rgb[0], fminf(rgb[1], rgb[2]));
    float sat = mx > 0.0f ? (mx - mn) / mx : 0.0f;
    float weight = vibrance * (1.0f - sat);
    float avg = (rgb[0] + rgb[1] + rgb[2]) / 3.0f;

    float4 result;
    result.x = fminf(fmaxf(avg + (rgb[0] - avg) * (1.0f + weight), 0.0f), 1.0f);
    result.y = fminf(fmaxf(avg + (rgb[1] - avg) * (1.0f + weight), 0.0f), 1.0f);
    result.z = fminf(fmaxf(avg + (rgb[2] - avg) * (1.0f + weight), 0.0f), 1.0f);
    result.w = px.w;
    output_pixels[i] = result;
}
"#;

pub struct CudaLiveChain {
    stream: Arc<CudaStream>,
    func: CudaFunction,
}

#[derive(Debug, Clone, Copy)]
pub struct CudaLiveChainParams {
    pub wb_gain: [f32; 3],
    pub exposure_stops: f32,
    pub vibrance: f32,
}

impl CudaLiveChain {
    /// `None` if no CUDA driver/NVRTC is available on this machine -- callers should skip the
    /// comparison, not fail, exactly like the wgpu side skips when no adapter is available.
    pub fn new() -> Option<Self> {
        let ctx = CudaContext::new(0).ok()?;
        let stream = ctx.default_stream();
        let ptx = cudarc::nvrtc::compile_ptx(LIVE_CHAIN_CU).ok()?;
        let module = ctx.load_module(ptx).ok()?;
        let func = module.load_function("live_chain").ok()?;
        Some(Self { stream, func })
    }

    pub fn device_name(&self) -> String {
        self.stream
            .context()
            .name()
            .unwrap_or_else(|_| "unknown CUDA device".to_string())
    }

    pub fn run(&self, pixels: &[[f32; 4]], params: CudaLiveChainParams) -> (Vec<[f32; 4]>, f64) {
        let flat: &[f32] = bytemuck::cast_slice(pixels);
        let input_dev = self
            .stream
            .clone_htod(flat)
            .expect("host->device upload failed");
        let mut output_dev = self
            .stream
            .alloc_zeros::<f32>(flat.len())
            .expect("device alloc failed");

        let count = pixels.len() as u32;
        let cfg = LaunchConfig::for_num_elems(count);

        let mut builder = self.stream.launch_builder(&self.func);
        builder.arg(&input_dev);
        builder.arg(&mut output_dev);
        builder.arg(&count);
        builder.arg(&params.wb_gain[0]);
        builder.arg(&params.wb_gain[1]);
        builder.arg(&params.wb_gain[2]);
        builder.arg(&params.exposure_stops);
        builder.arg(&params.vibrance);
        // GPU-side event timing (not host wall-clock), per `docs/benchmarks.md`'s methodology --
        // matches the timestamp-query approach used on the wgpu side in `gpu.rs`.
        builder.record_kernel_launch(cudarc::driver::sys::CUevent_flags::CU_EVENT_DEFAULT);
        let (start_event, end_event) = unsafe { builder.launch(cfg) }
            .expect("kernel launch failed")
            .expect("record_kernel_launch was set, events must be returned");
        let elapsed_ms = start_event
            .elapsed_ms(&end_event)
            .expect("event timing failed");
        let elapsed = elapsed_ms as f64 * 1e6;
        self.stream.synchronize().expect("stream sync failed");

        let out_flat = self
            .stream
            .clone_dtoh(&output_dev)
            .expect("device->host readback failed");
        let out: &[[f32; 4]] = bytemuck::cast_slice(&out_flat);
        (out.to_vec(), elapsed)
    }
}
