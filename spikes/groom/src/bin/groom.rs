//! Minimal demo/bench binary: runs clone stamp, spot heal, and auto-source-pick once each on a
//! synthetic image and prints a small JSON summary (timing in microseconds), mirroring `sniff`'s
//! JSON-bench-output convention at a much smaller scale -- this spike's real perf numbers live in
//! `tests/throughput.rs` (`#[ignore]`d, run explicitly), not this binary.

use std::time::Instant;

use groom::cpu_reference::{auto_source_pick, clone_stamp, spot_heal, Image};

fn checkerboard(width: usize, height: usize) -> Image {
    let mut img = Image::new(width, height, [0.0, 0.0, 0.0, 1.0]);
    for y in 0..height {
        for x in 0..width {
            let v = if (x / 8 + y / 8) % 2 == 0 { 0.85 } else { 0.15 };
            img.set(x as i32, y as i32, [v, v, v, 1.0]);
        }
    }
    img
}

fn main() {
    let src = checkerboard(256, 256);

    let mut clone_dst = src.clone();
    let t0 = Instant::now();
    clone_stamp(&mut clone_dst, &src, (128, 128), (20, 20), 15.0, 3.0);
    let clone_us = t0.elapsed().as_micros();

    let mut heal_dst = src.clone();
    let t0 = Instant::now();
    spot_heal(&mut heal_dst, &src, (128, 128), (20, 20), 15.0, 3.0, 200);
    let heal_us = t0.elapsed().as_micros();

    let t0 = Instant::now();
    let picked = auto_source_pick(&src, (128, 128), 15.0, 60.0, 24);
    let pick_us = t0.elapsed().as_micros();

    println!(
        "{{\"clone_stamp_us\":{clone_us},\"spot_heal_us\":{heal_us},\"auto_source_pick_us\":{pick_us},\"auto_source_pick_offset\":{picked:?}}}"
    );
}
