## Culling

Covers burst/duplicate grouping (#33). Face/subject grouping (#35) and blur/misfocus detection
(#34) are separate research tickets, not yet covered here — this file grows as they land.

- **Burst/duplicate grouping (#33)**: `docs/adr/0033-burst-duplicate-grouping.md` — con-day
  duplicates are pose sets 2-30s apart, not sub-second bursts (measured on a real con day: most
  gaps land at 2-10s, only 152/1,368 are <=1s), so a pure timestamp threshold misses most real
  duplicates. `spikes/litter` implements sequence-constrained two-level (tight/set) grouping,
  where time acts as a constraint and a visual-similarity signal (dHash/pHash/SSIM/a real DINOv2
  embedding, all four measured under the identical grouping algorithm) makes the actual link
  decision. Nesting invariant (every set group is a union of whole tight groups) is structural,
  not just asserted — a dedicated test tries to break it under adversarial similarity functions.
  A from-scratch EXIF/Nikon-MakerNote reader (capture time, shutter count, serial, PreviewIFD)
  cross-checks byte-exact against 37 real Z8 NEFs' own XMP sidecars. A real ONNX Runtime library
  and a real DINOv2 export were obtained and run end-to-end (not left as an untested `#[ignore]`)
  — found one real caveat along the way: a segfault-on-exit that's an `ort`/`load-dynamic`
  teardown-ordering quirk, not a bug in this crate. **Proposed, not Accepted** — the actual
  accuracy numbers wait on a real unculled con shoot (none exists on disk yet; every large con
  folder found is already culled), tracked in the labelling/measurement follow-up (#180).
