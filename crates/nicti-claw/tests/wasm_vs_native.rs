//! Measures a WASM (wasmtime) guest pixel kernel against the identical kernel in native
//! Rust, including the host<->guest buffer-copy cost — the concrete number behind ADR-0004's
//! claim that WASM is viable for non-hot-path extension points but not for a third-party
//! render-stage's per-pixel loop under the 16.7ms/frame hero-scenario budget (#43/#44).
//!
//! The kernel is `buf[i] *= k` (a linear scale — representative of a cheap per-pixel stage
//! like white balance), over a buffer smaller than a real 45MP RGBA16F frame (~360MB) to
//! keep this test's CI time/memory bounded. Treat the numbers here as an order-of-magnitude
//! proxy, not a literal measurement at hero-scenario scale — the ADR extrapolates.
//!
//! This test only asserts correctness (the WASM and native paths must agree bit-for-bit —
//! IEEE-754 single-precision multiply is deterministic). Timing is printed, not asserted on,
//! since it's environment-dependent; run with `cargo test -p nicti-claw --test wasm_vs_native --
//! --nocapture` to see the numbers, which get transcribed into the ADR by hand.

use std::time::Instant;
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

/// f32 elements. ~30.5MB. A real hero-scenario frame is ~360MB (45MP RGBA16F) — see the
/// module doc comment on why this is scaled down.
const LEN: usize = 8_000_000;
const SCALE: f32 = 1.5;

const KERNEL_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "process") (param $ptr i32) (param $len_bytes i32) (param $k f32)
    (local $i i32)
    (local.set $i (i32.const 0))
    (block $break
      (loop $continue
        (br_if $break (i32.ge_u (local.get $i) (local.get $len_bytes)))
        (f32.store
          (i32.add (local.get $ptr) (local.get $i))
          (f32.mul
            (f32.load (i32.add (local.get $ptr) (local.get $i)))
            (local.get $k)))
        (local.set $i (i32.add (local.get $i) (i32.const 4)))
        (br $continue)
      )
    )
  )
)
"#;

fn native_kernel(buf: &mut [f32], k: f32) {
    for x in buf.iter_mut() {
        *x *= k;
    }
}

/// Runs the WAT kernel once against a fresh copy of `input`, returning the result and the
/// (copy_in, call, copy_out) timings.
fn run_wasm_kernel(
    instance: &Instance,
    store: &mut Store<()>,
    process: &TypedFunc<(i32, i32, f32), ()>,
    input: &[f32],
) -> (
    Vec<f32>,
    std::time::Duration,
    std::time::Duration,
    std::time::Duration,
) {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .expect("module exports \"memory\"");

    let len_bytes = std::mem::size_of_val(input);
    let needed_pages = len_bytes.div_ceil(65536) as u64;
    let current_pages = memory.size(&mut *store);
    if needed_pages > current_pages {
        memory
            .grow(&mut *store, needed_pages - current_pages)
            .expect("grow guest memory to fit the buffer");
    }

    let copy_in_start = Instant::now();
    let input_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(input.as_ptr().cast::<u8>(), len_bytes) };
    memory
        .write(&mut *store, 0, input_bytes)
        .expect("write input buffer into guest linear memory");
    let copy_in = copy_in_start.elapsed();

    let call_start = Instant::now();
    process
        .call(&mut *store, (0, len_bytes as i32, SCALE))
        .expect("guest kernel call");
    let call = call_start.elapsed();

    // Allocated *before* the timed window, mirroring how `native_buf` is cloned before
    // `native_start` in the caller: an earlier version of this test started the clock before
    // this allocation, which counted a ~30MB zero-init allocation as "copy_out" time and
    // inflated the WASM side's numbers against a native path that pays no such cost inside
    // its own timed region.
    let mut out = vec![0.0f32; input.len()];
    let copy_out_start = Instant::now();
    let out_bytes: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u8>(), len_bytes) };
    memory
        .read(&store, 0, out_bytes)
        .expect("read result buffer back out of guest linear memory");
    let copy_out = copy_out_start.elapsed();

    (out, copy_in, call, copy_out)
}

#[test]
fn wasm_and_native_agree_and_are_timed() {
    let input: Vec<f32> = (0..LEN).map(|i| (i % 4096) as f32 * 0.01).collect();

    let mut native_buf = input.clone();
    let native_start = Instant::now();
    native_kernel(&mut native_buf, SCALE);
    let native_elapsed = native_start.elapsed();

    let engine = Engine::default();
    let module = Module::new(&engine, KERNEL_WAT).expect("parse+compile the WAT kernel");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate the module");
    let process: TypedFunc<(i32, i32, f32), ()> = instance
        .get_typed_func(&mut store, "process")
        .expect("module exports \"process\"");

    let (wasm_out, copy_in, call, copy_out) =
        run_wasm_kernel(&instance, &mut store, &process, &input);

    assert_eq!(
        wasm_out, native_buf,
        "WASM and native kernels must agree bit-for-bit (deterministic f32 multiply)"
    );

    let wasm_total = copy_in + call + copy_out;
    eprintln!(
        "wasm_vs_native ({LEN} f32 elements, ~{:.1}MB): \
         native={native_elapsed:?} | wasm copy_in={copy_in:?} call={call:?} \
         copy_out={copy_out:?} total={wasm_total:?} \
         (wasm total / native = {:.1}x)",
        (LEN * 4) as f64 / 1_000_000.0,
        wasm_total.as_secs_f64() / native_elapsed.as_secs_f64().max(f64::EPSILON),
    );
}
