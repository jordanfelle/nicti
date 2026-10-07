//! Develop-settings translation (ADR-0061 Q4): LRC's `Adobe_imageDevelopSettings.text` Lua literal
//! -> a nicti [`EditDocument`]. The Lua is parsed with the `agprefs` crate (0 failures on 380,307
//! real rows). A key is *translated* only when nicti has a stage with the same meaning and the
//! conversion is known; everything else stays in the provenance blob and is listed (when it has a
//! real, non-zero value) in [`Translation::untranslated`] so a follow-up ticket can see what real
//! catalogs actually use. Absent keys keep nicti's own defaults -- never a guessed value.
//!
//! Slider units: LRC's global sliders are -100..100 and nicti's are -1..1 (`coat.rs`), so those are
//! divided by 100; exposure is in stops in both; the `Local*` sliders inside a mask correction are
//! already -1..1 in LRC's own data.

use std::borrow::Cow;
use std::collections::BTreeSet;

use agprefs::{Agpref, Value};
use nicti_pawprint::{EditDocument, StageEntry};
use serde::Serialize;

mod basic;
mod crop;
mod detail;
mod effects;
mod filters;
mod heal;
mod hsl;
mod lens;
mod masks;

pub use filters::FilterCounts;

/// What the translator needs to know about the image beyond the Lua text.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Source frame size in pixels, for the keys LRC stores normalized (crop, heal spots).
    pub width: Option<f32>,
    pub height: Option<f32>,
    /// `Adobe_images.orientation` (`AB` = upright). Crop coordinates are in *oriented* space, so a
    /// rotated original cannot be mapped without knowing nicti's own convention for it.
    pub orientation: Option<String>,
    /// `Adobe_imageDevelopSettings.processVersion` (`15.4` current, `10.0` legacy PV2003).
    pub process_version: Option<String>,
}

/// Counters for the report: what was dropped and why, summed across images by the job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// `RetouchAreas` entries seen (not translated, see `heal.rs`).
    pub heal_spots_skipped: u64,
    pub mask_corrections: u64,
    pub mask_corrections_skipped: u64,
    pub mask_components_skipped: u64,
    pub ai_masks: u64,
    pub crops: u64,
    /// A crop present in the data but not translated (rotated, upright-unknown or no frame size).
    pub crops_skipped: u64,
    pub legacy_process_version: u64,
    /// `Parametric*Split` points other than nicti's fixed 25/50/75.
    pub tone_curve_splits_ignored: u64,
}

/// The result of translating one image's develop text.
#[derive(Debug, Clone, Default)]
pub struct Translation {
    pub document: EditDocument,
    /// Top-level keys seen with a real (non-zero/empty) value that no translator consumed, sorted.
    pub untranslated: Vec<String>,
    pub filters: FilterCounts,
    pub stats: Stats,
}

impl Stats {
    pub fn add(&mut self, o: &Stats) {
        self.heal_spots_skipped += o.heal_spots_skipped;
        self.mask_corrections += o.mask_corrections;
        self.mask_corrections_skipped += o.mask_corrections_skipped;
        self.mask_components_skipped += o.mask_components_skipped;
        self.ai_masks += o.ai_masks;
        self.crops += o.crops;
        self.crops_skipped += o.crops_skipped;
        self.legacy_process_version += o.legacy_process_version;
        self.tone_curve_splits_ignored += o.tone_curve_splits_ignored;
    }
}

#[derive(Debug, thiserror::Error)]
#[error("develop settings did not parse: {0}")]
pub struct DevelopError(String);

pub(crate) struct Tx<'a, 'v> {
    pub root: &'a Value<'v>,
    pub ctx: &'a Context,
    pub consumed: BTreeSet<String>,
    pub doc: EditDocument,
    pub filters: FilterCounts,
    pub stats: Stats,
}

impl<'v> Tx<'_, 'v> {
    /// A top-level value, marking the key consumed whether or not it ends up used.
    pub fn take(&mut self, key: &str) -> Option<&Value<'v>> {
        let v = field(self.root, key)?;
        self.consumed.insert(key.to_string());
        Some(v)
    }

    /// A top-level number (`Int`/`Float`, or a numeric string), consuming the key.
    pub fn num(&mut self, key: &str) -> Option<f64> {
        self.take(key).and_then(number)
    }

    /// A top-level string, consuming the key.
    pub fn text(&mut self, key: &str) -> Option<String> {
        self.take(key)
            .and_then(|v| v.get_string())
            .map(str::to_string)
    }

    /// `Enable<Panel>`: absent means enabled; `false`/`0` disables. Consumes the key.
    pub fn enabled(&mut self, key: &str) -> bool {
        match self.take(key) {
            Some(Value::Bool(b)) => *b,
            Some(v) => number(v).is_none_or(|n| n != 0.0),
            None => true,
        }
    }

    /// Stores `params` as stage `id` unless it equals the stage's default (a no-op entry is
    /// noise: it would make every untouched image look edited).
    pub fn put<T: Serialize + Default + PartialEq>(&mut self, id: &str, params: T) {
        if params == T::default() {
            return;
        }
        // Serializing these plain structs cannot fail; if it ever did, no half value is persisted.
        if let Ok(value) = serde_json::to_value(&params) {
            self.doc.stages.insert(
                id.to_string(),
                StageEntry {
                    schema_version: 1,
                    params: value,
                },
            );
        }
    }
}

/// A struct field by name.
pub(crate) fn field<'a, 'v>(v: &'a Value<'v>, key: &str) -> Option<&'a Value<'v>> {
    v.get_struct().and_then(|m| m.get(key))
}

/// A number from an `Int`/`Float`, or a numeric string (LRC writes some numbers as text).
pub(crate) fn number(v: &Value<'_>) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

pub(crate) fn field_num(v: &Value<'_>, key: &str) -> Option<f64> {
    field(v, key).and_then(number)
}

pub(crate) fn field_str<'a>(v: &'a Value<'_>, key: &str) -> Option<&'a str> {
    field(v, key).and_then(|x| x.get_string())
}

/// `true`/non-zero.
pub(crate) fn field_bool(v: &Value<'_>, key: &str) -> Option<bool> {
    match field(v, key)? {
        Value::Bool(b) => Some(*b),
        other => number(other).map(|n| n != 0.0),
    }
}

/// A Lua array's elements: LRC writes `{ a, b }` lists as `Values`; a table with integer-like keys
/// would parse as a struct, so also accept that in key order.
pub(crate) fn items<'a, 'v>(v: &'a Value<'v>) -> Vec<&'a Value<'v>> {
    match v {
        Value::Values(list) => list.iter().collect(),
        Value::Struct(map) => map.values().collect(),
        _ => Vec::new(),
    }
}

/// Whether a value carries nothing: zero, `false`, empty text/table, unit.
fn is_noop(v: &Value<'_>) -> bool {
    match v {
        Value::Unit => true,
        Value::Int(i) => *i == 0,
        Value::Float(f) => *f == 0.0,
        Value::Bool(b) => !*b,
        Value::String(s) => s.is_empty(),
        Value::Values(l) => l.iter().all(is_noop),
        Value::Struct(m) => m.values().all(is_noop),
    }
}

/// Values LRC writes into every untouched image (read off a real v13 catalog): not "edits", so
/// they don't belong in the untranslated-key histogram. They stay in provenance regardless.
enum Known {
    Num(f64),
    Text(&'static str),
    Flag(bool),
}

const DEFAULTS: &[(&str, Known)] = &[
    ("ColorGradeBlending", Known::Num(50.0)),
    ("CurveRefineSaturation", Known::Num(100.0)),
    // Consumed by `lens::apply`, which only runs for PV2012: a legacy-PV image's untouched sliders must
    // still count as noise (same reason `GrainSize`/`GrainFrequency` stay listed).
    ("DefringeGreenHueHi", Known::Num(60.0)),
    ("DefringeGreenHueLo", Known::Num(40.0)),
    ("DefringePurpleHueHi", Known::Num(70.0)),
    ("DefringePurpleHueLo", Known::Num(30.0)),
    ("PerspectiveScale", Known::Num(100.0)),
    ("UprightCenterNormX", Known::Num(0.5)),
    ("UprightCenterNormY", Known::Num(0.5)),
    ("UprightFocalLength35mm", Known::Num(35.0)),
    ("UprightTransformCount", Known::Num(6.0)),
    ("UprightVersion", Known::Num(151388160.0)),
    ("GrainSize", Known::Num(25.0)),
    ("GrainFrequency", Known::Num(50.0)),
    ("HDRMaxValue", Known::Num(2.3)),
    ("HDRMaxValue", Known::Num(4.0)),
    ("LensProfileSetup", Known::Text("LensDefaults")),
    ("ToneCurveName2012", Known::Text("Linear")),
    ("EnableDistractionRemoval", Known::Flag(true)),
];

/// Pure bookkeeping: identifiers, versions and digests, never a develop parameter.
fn is_bookkeeping(key: &str) -> bool {
    key.ends_with("Digest")
        || matches!(
            key,
            "ProcessVersion" | "Version" | "CompatibleVersion" | "UUID" | "ToggleStyleAmount"
                // PV2003/2010 sliders LRC keeps alongside the PV2012 ones; ignored by current
                // rendering (`Saturation` is shared with PV2012, so it is *not* listed).
                | "Contrast" | "Shadows" | "Brightness" | "FillLight" | "Exposure"
                | "HighlightRecovery" | "AutoTone"
        )
}

fn is_identity_curve(v: &Value<'_>) -> bool {
    let points: Vec<f64> = items(v).into_iter().filter_map(number).collect();
    points.is_empty() || points == [0.0, 0.0, 255.0, 255.0]
}

/// Whether an untranslated key is just LRC's untouched-image noise.
fn is_noise(key: &str, v: &Value<'_>) -> bool {
    if is_bookkeeping(key) {
        return true;
    }
    if key.starts_with("ToneCurvePV2012") && is_identity_curve(v) {
        return true;
    }
    // A key with a known non-zero default: only that exact default is noise -- a user-set 0 is a
    // real edit and must stay visible.
    if DEFAULTS.iter().any(|(k, _)| *k == key) {
        return DEFAULTS.iter().any(|(k, d)| {
            *k == key
                && match d {
                    Known::Num(n) => number(v).is_some_and(|x| (x - n).abs() < 1e-9),
                    Known::Text(t) => v.get_string() == Some(t),
                    Known::Flag(b) => matches!(v, Value::Bool(x) if x == b),
                }
        });
    }
    is_noop(v)
}

/// Translates one image's develop-settings Lua text. A parse failure is an `Err` the caller counts;
/// the image is then imported with no translated edit (provenance still keeps the text).
pub fn translate(text: &str, ctx: &Context) -> Result<Translation, DevelopError> {
    let pref = Agpref::parse(text).map_err(|e| DevelopError(e.to_string()))?;
    let root = &pref.values;
    if root.get_struct().is_none() {
        return Err(DevelopError("top level is not a table".into()));
    }
    let mut tx = Tx {
        root,
        ctx,
        consumed: BTreeSet::new(),
        doc: EditDocument::default(),
        filters: FilterCounts::default(),
        stats: Stats::default(),
    };

    // PV2003/2010 are "10.0" and below; PV2012 and every later version (11.x ... 15.x, and whatever
    // Adobe ships next) use the keys translated here. An unparsable version counts as current.
    if ctx
        .process_version
        .as_deref()
        .and_then(|p| p.trim().parse::<f64>().ok())
        .is_some_and(|v| v < 11.0)
    {
        // PV2003/2010 use different slider keys and curves (Brightness, FillLight, ...): not
        // translated; the provenance text keeps everything.
        tx.stats.legacy_process_version += 1;
    }
    let legacy = tx.stats.legacy_process_version > 0;

    if !legacy {
        basic::apply(&mut tx);
        hsl::apply(&mut tx);
        detail::apply(&mut tx);
        effects::apply(&mut tx);
        lens::apply(&mut tx);
        crop::apply(&mut tx);
        heal::apply(&mut tx);
        masks::apply(&mut tx);
    }
    filters::apply(&mut tx);

    let untranslated = match root.get_struct() {
        Some(map) => map
            .iter()
            .filter(|(k, v)| !tx.consumed.contains::<str>(k) && !is_noise(k, v))
            .map(|(k, _): (&Cow<str>, _)| k.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    Ok(Translation {
        document: tx.doc,
        untranslated,
        filters: tx.filters,
        stats: tx.stats,
    })
}

/// `value` is within a hair of `expected` -- stage params are `f32`, so their JSON is not exact.
#[cfg(test)]
pub(crate) fn close(value: &serde_json::Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|v| (v - expected).abs() <= 1e-4 * expected.abs().max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pv2003_and_2010_are_legacy_not_every_version_that_isnt_15() {
        let text = "s = { Exposure2012 = 1 }";
        for (pv, translated) in [
            ("10.0", false),
            ("11.0", true),
            ("15.4", true),
            ("16.1", true),
        ] {
            let ctx = Context {
                process_version: Some(pv.into()),
                ..Context::default()
            };
            let t = translate(text, &ctx).unwrap();
            assert_eq!(!t.document.stages.is_empty(), translated, "{pv}");
            assert_eq!(
                t.stats.legacy_process_version,
                u64::from(!translated),
                "{pv}"
            );
        }
    }

    #[test]
    fn a_user_set_zero_on_a_key_with_a_nonzero_default_stays_visible_but_the_default_is_noise() {
        let t = translate(
            // (The grain keys used to be this test's example; #380 translates them.)
            "s = { ColorGradeBlending = 0, PerspectiveScale = 100, UprightFocalLength35mm = 50 }",
            &Context::default(),
        )
        .unwrap();
        assert_eq!(
            t.untranslated,
            vec!["ColorGradeBlending", "UprightFocalLength35mm"]
        );
    }
}
