//! The six PV2012 Basic-panel tone sliders LRC's "Auto Settings" sets (see `spikes/shed`'s
//! `develop::Owner::Global` classification for the full key list this is a subset of). Ranges are
//! Adobe's own documented Develop-panel slider ranges.

use serde::{Deserialize, Serialize};

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
}

impl Sliders {
    /// The six values in a fixed order, shared by the fit's feature/label vectors and by
    /// `eval`'s per-slider reporting.
    pub const NAMES: [&'static str; 6] = [
        "Exposure2012",
        "Contrast2012",
        "Highlights2012",
        "Shadows2012",
        "Whites2012",
        "Blacks2012",
    ];

    pub fn as_array(&self) -> [f64; 6] {
        [
            self.exposure2012,
            self.contrast2012,
            self.highlights2012,
            self.shadows2012,
            self.whites2012,
            self.blacks2012,
        ]
    }

    pub fn from_array(v: [f64; 6]) -> Self {
        Self {
            exposure2012: v[0],
            contrast2012: v[1],
            highlights2012: v[2],
            shadows2012: v[3],
            whites2012: v[4],
            blacks2012: v[5],
        }
    }

    /// Clamps every slider to its documented Develop-panel range.
    pub fn clamped(&self) -> Self {
        Self {
            exposure2012: self.exposure2012.clamp(-5.0, 5.0),
            contrast2012: self.contrast2012.clamp(-100.0, 100.0),
            highlights2012: self.highlights2012.clamp(-100.0, 100.0),
            shadows2012: self.shadows2012.clamp(-100.0, 100.0),
            whites2012: self.whites2012.clamp(-100.0, 100.0),
            blacks2012: self.blacks2012.clamp(-100.0, 100.0),
        }
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
        };
        assert_eq!(Sliders::from_array(s.as_array()), s);
    }

    #[test]
    fn clamped_bounds_every_slider() {
        let s = Sliders {
            exposure2012: 10.0,
            contrast2012: -200.0,
            highlights2012: 200.0,
            shadows2012: -200.0,
            whites2012: 200.0,
            blacks2012: -200.0,
        };
        let c = s.clamped();
        assert_eq!(c.exposure2012, 5.0);
        assert_eq!(c.contrast2012, -100.0);
        assert_eq!(c.highlights2012, 100.0);
        assert_eq!(c.shadows2012, -100.0);
        assert_eq!(c.whites2012, 100.0);
        assert_eq!(c.blacks2012, -100.0);
    }
}
