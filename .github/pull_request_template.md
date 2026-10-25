## Summary

<!-- What does this change do, and why? -->

Closes #<!-- issue number, if any -->

## Checklist

- [ ] `cargo fmt --all -- --check` / `cargo clippy --workspace ...` / `cargo test --workspace ...`
      pass locally (see CONTRIBUTING.md for the exact exclude flags)
- [ ] `docs/licensing.md` updated, if this adds/changes a dependency, native library, or ML model
- [ ] An ADR added or updated under `docs/adr/`, if this makes a decision worth recording
- [ ] Verified on Windows (v1's required platform), if this touches anything platform-sensitive
- [ ] No `notify*` inside a `Drop` (or other cleanup) that also runs on a path that did no work
      (#407: an idle worker's permit release woke every other idle worker, forever)
