//! Reproduces #179's actual failure mode directly, rather than a same-crate repeat call (which
//! can't reach it -- each crate's own wrapper delegates to `nicti-haw`'s one process-global
//! `OnceLock`, which caches the *first* call's result, so a second call *within one crate* never
//! re-runs `commit()` and would trivially pass under the original buggy code too, given the first
//! call already succeeded).
//!
//! The real bug is cross-crate: six throwaway spikes (`groom`, `siamese`, `crouch`, `rods`,
//! `litter`, `rosette`) each used to carry their own copy of `ensure_ort_environment`, but `ort`'s
//! environment is a single process-global (`G_ENV_OPTIONS`). Before #179's fix, whichever crate's
//! copy lost the race -- i.e. called `EnvironmentBuilder::commit()` after any other crate already
//! had -- treated `commit() == false` as a permanent, cached error, so every subsequent model load
//! in that crate failed forever even though a perfectly usable environment was already active
//! process-wide. `all_six_spikes_can_commit_the_shared_ort_environment_in_one_process` drives all
//! six crates' `ensure_ort_environment` back-to-back in one process and asserts every one of them
//! succeeds when they all request the *same* dylib path.
//!
//! #229 closed a second gap #179 left open: none of those six copies could tell a *different*
//! dylib path apart from the one that actually won the race -- a losing caller just silently ran
//! against whichever environment the winner loaded. All six now delegate to `nicti-haw`, which
//! records the first path and rejects a later, different one with a typed error.
//! `a_different_dylib_path_is_rejected_before_reaching_session_builder` proves that end-to-end,
//! across two different crates, using two different string spellings of the same real dylib
//! (an absolute path and a `./`-relative one) so the test only needs one real ONNX Runtime shared
//! library on disk, not two distinct builds.
//!
//! Needs a real ONNX Runtime shared library (`NICTI_TEST_ORT_DYLIB`) to get past
//! `ort::init_from`'s own dlopen -- neither exists in CI, so both tests are `#[ignore]`d, same
//! posture as each crate's own real-model tests (e.g. `groom::ai::tests::runs_a_real_model_if_present`).
//!
//! **Run these directly, not through `cargo test`'s harness process**: `spikes/litter/src/embed.rs`
//! documents a known `ort`/`load-dynamic` static-destructor-ordering segfault on process exit
//! after a real dylib load succeeds -- the tests' own assertions still pass and print before that,
//! but `cargo test`'s harness reports the child's segfault as a failure regardless, since it
//! judges pass/fail by the test process's exit status, not by what it printed before exiting.
//! Build without running via `cargo test -p groom --test ort_cross_module --no-run`, then run the
//! resulting binary directly (path printed by that command, under `target/debug/deps/`) with
//! `NICTI_TEST_ORT_DYLIB=<path-to-libonnxruntime.so> <binary-path> --ignored --nocapture` -- the
//! env var must be set on that direct invocation too, since running the binary standalone skips
//! whatever `cargo test`'s own harness would otherwise have inherited it from. The printed
//! assertion output is the real pass/fail signal, not the process's exit status.

#[test]
#[ignore = "needs a real ONNX Runtime shared library on disk"]
fn all_six_spikes_can_commit_the_shared_ort_environment_in_one_process() {
    let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
    let dylib_path = std::path::Path::new(&dylib_path);

    groom::ai::ensure_ort_environment(dylib_path).expect("groom (first caller) must succeed");
    siamese::segment::ensure_ort_environment(dylib_path)
        .expect("siamese (second caller, environment already committed by groom) must succeed");
    crouch::ort_contend::ensure_ort_environment(dylib_path)
        .expect("crouch (third caller) must succeed");
    rods::ai::ensure_ort_environment(dylib_path).expect("rods (fourth caller) must succeed");
    litter::embed::ensure_ort_environment(dylib_path).expect("litter (fifth caller) must succeed");
    rosette::embed::ensure_ort_environment(dylib_path)
        .expect("rosette (sixth caller) must succeed");
}

#[test]
#[ignore = "needs a real ONNX Runtime shared library on disk"]
fn a_different_dylib_path_is_rejected_before_reaching_session_builder() {
    let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
    let absolute_path = std::path::Path::new(&dylib_path);
    // A different string spelling of the same file is enough to exercise "requested path !=
    // committed path" -- nicti-haw compares the requested path string, not the resolved file
    // identity, so this doesn't need two distinct real dylib builds. Not a contrived case: two
    // separate build systems/toolchains passing an absolute vs. a relative path to the same
    // installed runtime is a realistic way this could actually happen.
    //
    // Built via string concatenation, not `PathBuf::join` -- `NICTI_TEST_ORT_DYLIB` is documented
    // (see this file's own doc comment) to be set to an absolute path, and `Path::join` discards
    // `self` entirely when the argument is itself absolute (std's own documented behavior), which
    // would silently make `relative_path` byte-identical to `absolute_path` instead of a `./`
    // -prefixed spelling of it.
    let relative_path = std::path::PathBuf::from(format!("./{dylib_path}"));
    assert_ne!(
        absolute_path,
        relative_path.as_path(),
        "test setup bug: relative_path must actually be a different path string from \
         absolute_path, or the assertion below would prove nothing"
    );

    groom::ai::ensure_ort_environment(absolute_path)
        .expect("groom (first caller) commits the absolute path");

    let err = siamese::segment::ensure_ort_environment(&relative_path)
        .expect_err("a genuinely different path string must be rejected, not silently reused");
    let message = err.to_string();
    assert!(
        message.contains("already initialized"),
        "expected a path-mismatch error, got: {message}"
    );
}
