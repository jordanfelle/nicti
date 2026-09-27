//! The eight PV2012 sliders #53 predicts: the six `spikes/pupil::sliders` already defines plus
//! Saturation/Vibrance -- #53's issue body lists all eight as targets, unlike #99's auto-tone
//! (LRC's own "Auto Settings" only ever touches the first six). A small independent copy, not a
//! path dependency on `pupil` -- this repo's spikes stay self-contained (see `pupil::input`'s doc
//! comment for why).

use serde::{Deserialize, Serialize};

pub const SLIDER_COUNT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Sliders {
    /// `Exposure2012`, EV, range -5.0..=5.0.
    pub exposure2012: f64,
    /// `Contrast2012`, range -100.0..=100.0.
    pub contrast2012: f64,
    /// `Highlights2012`, range -100.0..=100.0.
    pub highlights2012: f64,
    /// `Shadows2012`, range -100.0..=100.0.
    pub shadows2012: f64,
    /// `Whites2012`, range -100.0..=100.0.
    pub whites2012: f64,
    /// `Blacks2012`, range -100.0..=100.0.
    pub blacks2012: f64,
    /// `Saturation`, range -100.0..=100.0.
    pub saturation: f64,
    /// `Vibrance`, range -100.0..=100.0.
    pub vibrance: f64,
}

impl Sliders {
    pub const NAMES: [&'static str; SLIDER_COUNT] = [
        "Exposure2012",
        "Contrast2012",
        "Highlights2012",
        "Shadows2012",
        "Whites2012",
        "Blacks2012",
        "Saturation",
        "Vibrance",
    ];

    /// The documented Develop-panel range for each slider, in `NAMES` order.
    pub const RANGES: [(f64, f64); SLIDER_COUNT] = [
        (-5.0, 5.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
        (-100.0, 100.0),
    ];

    pub fn as_array(&self) -> [f64; SLIDER_COUNT] {
        [
            self.exposure2012,
            self.contrast2012,
            self.highlights2012,
            self.shadows2012,
            self.whites2012,
            self.blacks2012,
            self.saturation,
            self.vibrance,
        ]
    }

    pub fn from_array(v: [f64; SLIDER_COUNT]) -> Self {
        Self {
            exposure2012: v[0],
            contrast2012: v[1],
            highlights2012: v[2],
            shadows2012: v[3],
            whites2012: v[4],
            blacks2012: v[5],
            saturation: v[6],
            vibrance: v[7],
        }
    }

    /// Clamps every slider to its documented Develop-panel range.
    pub fn clamped(&self) -> Self {
        let v = self.as_array();
        let clamped = std::array::from_fn(|i| v[i].clamp(Self::RANGES[i].0, Self::RANGES[i].1));
        Self::from_array(clamped)
    }

    /// True if any slider differs from its default (0.0) by at least `eps` -- the "this is a real
    /// edit, not an untouched default" filter `catalog::keepers` applies before treating a row as
    /// a training label.
    pub fn any_non_default(&self, eps: f64) -> bool {
        self.as_array().iter().any(|v| v.abs() > eps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_round_trip() {
        let s = Sliders {
            exposure2012: 0.5,
            contrast2012: 10.0,
            highlights2012: -20.0,
            shadows2012: 30.0,
            whites2012: -5.0,
            blacks2012: 15.0,
            saturation: 8.0,
            vibrance: -3.0,
        };
        assert_eq!(Sliders::from_array(s.as_array()), s);
    }

    #[test]
    fn clamped_bounds_every_slider() {
        let s = Sliders::from_array([10.0, -200.0, 200.0, -200.0, 200.0, -200.0, 200.0, -200.0]);
        let c = s.clamped();
        assert_eq!(c.exposure2012, 5.0);
        assert_eq!(c.contrast2012, -100.0);
        assert_eq!(c.highlights2012, 100.0);
        assert_eq!(c.shadows2012, -100.0);
        assert_eq!(c.whites2012, 100.0);
        assert_eq!(c.blacks2012, -100.0);
        assert_eq!(c.saturation, 100.0);
        assert_eq!(c.vibrance, -100.0);
    }

    #[test]
    fn any_non_default_is_false_for_all_zero() {
        assert!(!Sliders::default().any_non_default(1e-9));
    }

    #[test]
    fn any_non_default_detects_a_single_nonzero_slider() {
        let s = Sliders {
            vibrance: 1.0,
            ..Default::default()
        };
        assert!(s.any_non_default(1e-9));
    }
}
