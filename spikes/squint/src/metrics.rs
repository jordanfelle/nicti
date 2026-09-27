//! Scoring for #34's candidates: binary flag precision/recall/F1 (standard formulas, not
//! B-cubed/ARI -- unlike `spikes/litter`'s clustering problem, this is a plain per-frame
//! classification: does the candidate flag this frame as blurry/misfocused/eyes-closed, y/n), plus
//! the keeper false-flag rate ADR-0034's decision rule leads with.

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Confusion {
    pub true_positive: u32,
    pub false_positive: u32,
    pub false_negative: u32,
    pub true_negative: u32,
}

impl Confusion {
    pub fn record(&mut self, predicted_flag: bool, actual_flag: bool) {
        match (predicted_flag, actual_flag) {
            (true, true) => self.true_positive += 1,
            (true, false) => self.false_positive += 1,
            (false, true) => self.false_negative += 1,
            (false, false) => self.true_negative += 1,
        }
    }

    pub fn precision(&self) -> f64 {
        let denom = self.true_positive + self.false_positive;
        if denom == 0 {
            1.0
        } else {
            self.true_positive as f64 / denom as f64
        }
    }

    pub fn recall(&self) -> f64 {
        let denom = self.true_positive + self.false_negative;
        if denom == 0 {
            1.0
        } else {
            self.true_positive as f64 / denom as f64
        }
    }

    pub fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }

    /// Fraction of true-negative frames (real keepers, in this project's actual use) that were
    /// wrongly flagged -- ADR-0034's headline "keeper false-flag rate," the costly error class
    /// (a real keeper hidden behind a false reject) this project cares about most.
    pub fn false_flag_rate(&self) -> f64 {
        let denom = self.false_positive + self.true_negative;
        if denom == 0 {
            0.0
        } else {
            self.false_positive as f64 / denom as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_recall_f1_on_known_counts() {
        let mut c = Confusion::default();
        for _ in 0..8 {
            c.record(true, true); // TP
        }
        for _ in 0..2 {
            c.record(true, false); // FP
        }
        for _ in 0..1 {
            c.record(false, true); // FN
        }
        for _ in 0..89 {
            c.record(false, false); // TN
        }
        assert!((c.precision() - 0.8).abs() < 1e-9);
        assert!((c.recall() - 8.0 / 9.0).abs() < 1e-9);
        assert!((c.false_flag_rate() - 2.0 / 91.0).abs() < 1e-9);
    }

    #[test]
    fn empty_confusion_defaults_to_perfect_precision_recall_zero_false_flag() {
        let c = Confusion::default();
        assert_eq!(c.precision(), 1.0);
        assert_eq!(c.recall(), 1.0);
        assert_eq!(c.false_flag_rate(), 0.0);
    }
}
