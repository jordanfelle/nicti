# ADR-0101: Graceful degradation for automatic develop operations

- **Status:** Accepted
- **Date:** 2026-09-29
- **Ticket:** #101 Requirements: graceful-degradation behavior for automatic develop operations

## Context

Issue #101 (`requirements`, `develop`, `v1`, Part of #8) addresses what automatic,
non-interactive develop operations must do when they fail or have low confidence. Three
areas require clear specification: a standardized confidence/failure signal, UI behavior
on low confidence, and whether an effectively no-op invocation creates a history step (#21).

Current state across the codebase:
- `nicti-tapetum/src/autolevel.rs` defines `detect_level_angle(..) -> Option<f32>`. It
  returns `None` when there is no detected line within 30°, and provides no confidence
  signal otherwise. Thresholds remain untuned (#273).
- `nicti-tapetum/src/perk.rs` defines `estimate(&Histogram) -> (ExposureParams, ToneParams)`.
  It cannot fail and is provisional until #202 selects candidate A or B.
- `nicti-pelt/src/render.rs` defines `apply_auto_straighten`, which does nothing on `None`,
  and `apply_auto_tone`, which always writes both stages even when nothing changes.
- `nicti-pawprint/src/history.rs` defines `push_delta`, which has no no-op check; an
  unchanged delta appends a step and truncates the redo stack. `nicti-pelt` does not yet
  integrate `History`.

## Decision

1. **Signal type.** Every automatic operation returns a shared outcome enum rather than a
   bare value:
   `AutoOutcome<T> = Confident(T) | LowConfidence(T, AutoReason) | NoResult(AutoReason)`.
   This is a discrete three-state signal rather than a floating-point score, as thresholds
   are untuned and calibration does not carry across degradation types (ADR-0034). Each
   operation defines its own mapping to these three states. `AutoReason` is a compact enum:
   `NoFeatures` (nothing usable to lock onto, including only-diagonal lines outside the
   axis-deviation limit), `WeakEvidence` (features exist but too few or inconsistent),
   `AtypicalInput` (e.g. a degenerate histogram), and `DecodeIncomplete`.
2. **Auto-straighten mapping.**
   - `NoResult`: no qualifying lines found (equivalent to today's `None`).
   - `LowConfidence`: too few supporting lines, or detected line angles disagree beyond a
     specified tolerance (exact thresholds deferred to #273).
   - `Confident`: all other qualifying detections.
3. **Auto-tone mapping.**
   - Auto-tone always yields a value, matching Lightroom Classic's baseline behavior.
   - `LowConfidence`: degenerate histograms (near-empty, heavily clipped, or single-spike).
   - `NoResult(DecodeIncomplete)`: produced by the orchestration layer (`DevelopView`) when no
     complete decoded frame exists, not by the pure estimator (`perk::estimate` only sees a
     `Histogram` and cannot observe decode state).
   - This rule is independent of which candidate is chosen by #202.
4. **UI behavior, interactive single image.**
   - `NoResult`: make no change to the image, and display a transient, non-modal status hint
     (e.g. "No straight lines found"), avoiding silent no-ops.
   - `Confident` but the result equals the current parameters: no change, with a brief hint
     (e.g. "Already level"), so a click never looks like it silently failed.
   - `LowConfidence`:
     - Straighten: *do not apply* the adjustment, and display a transient hint distinct from
       the `NoResult` one (e.g. "Uncertain angle, not applied"), since lines were found but
       were too weak or inconsistent (an incorrect rotation is worse than none).
     - Tone: apply the adjustment anyway, with a subtle "low confidence" indicator on the
       control (the adjustment remains one undo step away).
   - Never display a modal dialog for automatic operations.
5. **Batch and sync operations (#52).** Never show per-image confirmation prompts during
   batch processing. Apply the same per-operation degradation rules, then present a single
   summary notification (e.g., "12 of 400 skipped: no straight lines; 5 applied at low
   confidence"), counting both skipped and low-confidence-applied images, with a way to jump to
   them. A photo where one op applied and another was skipped counts under each op separately.
   The jump mechanism (a session-local selection, since the catalog has no transient batch-result
   filter) is left to the implementing ticket.
6. **History stack integration (#21).**
   - An invocation whose resulting parameters equal the *effective* current parameters creates
     **no history step and does not truncate redo**. "Effective" means a stage with no entry
     compares as its stage default, so `None -> Some(default)` is a no-op. `nicti-pawprint`
     is schema-agnostic, so the caller resolves absent entries to defaults before the
     comparison. The same applies to `NoResult` and to an auto-straighten skipped due to low
     confidence.
   - An applied result creates exactly one history entry, via `apply_batch` (which records
     `control: None`, and `compact` only ever merges `control`-tagged deltas, so an auto op never
     coalesces with slider drags or with another auto invocation). Auto-tone writes Exposure
     and Tone in one batch.
   - This no-op guard is a general invariant for `History`, not a special case for automatic
     operations. It also covers `compact`: a merged run whose net `before == after` (a drag
     that returns to its start) is dropped rather than kept as a dummy entry.
7. **Corrupt or partial decode.** The orchestration layer returns `NoResult(DecodeIncomplete)`
   without invoking the operation when there is no complete decoded frame, rather than running
   against incomplete or corrupted image buffer data.

## Consequences

- #311: Implements `AutoOutcome` and `AutoReason` in `nicti-tapetum` (`autolevel` and
  `perk`), updates call sites, and wires UI hints and control markers in `nicti-pelt`.
- #312: Adds the general no-op guard to `nicti-pawprint/src/history.rs` (`push_delta`),
  including unit tests verifying that an unchanged delta does not append a history step and
  does not clear the redo stack.
- #273 tunes the specific numerical thresholds for straighten line counts and angular variance;
  the three-state outcome model and mapping defined here remain unaffected.
- #202 selects between candidate A and B for tone estimation; the auto-tone degradation rules
  and `AutoOutcome` contract defined here remain unaffected.
- Deferred: the exact transient hint wording and control-marker iconography, and the batch
  summary filter UI (#52), are scoped to their respective implementation tickets.
