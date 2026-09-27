//! Reproduces #179's actual failure mode directly, rather than a same-crate repeat call (which
//! can't reach it -- each crate's `OnceLock` caches its own first call's result, so a second call
//! *within one crate* never re-runs `commit()` and would trivially pass under the original buggy
//! code too, given the first call already succeeded).
//!
//! The real bug is cross-crate: five throwaway spikes (`groom`, `siamese`, `crouch`, `rods`,
//! `litter`) each carry their own copy of `ensure_ort_environment`, but `ort`'s environment is a
//! single process-global (`G_ENV_OPTIONS`). Before #179's fix, whichever crate's copy lost the
//! race -- i.e. called `EnvironmentBuilder::commit()` after any other crate already had -- treated
//! `commit() == false` as a permanent, cached error, so every subsequent model load in that crate
//! failed forever even though a perfectly usable environment was already active process-wide.
//! This test drives all five crates' `ensure_ort_environment` back-to-back in one process and
//! asserts every one of them succeeds.
//!
//! Needs a real ONNX Runtime shared library (`NICTI_TEST_ORT_DYLIB`) to get past
//! `ort::init_from`'s own dlopen -- neither exists in CI, so this is `#[ignore]`d, same posture as
//! each crate's own real-model tests (e.g. `groom::ai::tests::runs_a_real_model_if_present`).
//!
//! **Run this directly, not through `cargo test`'s harness process**: `spikes/litter/src/embed.rs`
//! documents a known `ort`/`load-dynamic` static-destructor-ordering segfault on process exit
//! after a real dylib load succeeds -- the test's own assertions still pass and print before that,
//! but `cargo test`'s harness reports the child's segfault as a failure regardless. Build with
//! `cargo test -p groom --test ort_cross_module -- --ignored --nocapture` and run the resulting
//! binary under `target/debug/deps/` directly to see the real pass/fail signal.

#[test]
#[ignore = "needs a real ONNX Runtime shared library on disk"]
fn all_five_spikes_can_commit_the_shared_ort_environment_in_one_process() {
    let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
    let dylib_path = std::path::Path::new(&dylib_path);

    groom::ai::ensure_ort_environment(dylib_path).expect("groom (first caller) must succeed");
    siamese::segment::ensure_ort_environment(dylib_path)
        .expect("siamese (second caller, environment already committed by groom) must succeed");
    crouch::ort_contend::ensure_ort_environment(dylib_path)
        .expect("crouch (third caller) must succeed");
    rods::ai::ensure_ort_environment(dylib_path).expect("rods (fourth caller) must succeed");
    litter::embed::ensure_ort_environment(dylib_path).expect("litter (fifth caller) must succeed");
}
