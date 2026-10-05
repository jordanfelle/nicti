//! The shared outcome signal for automatic develop operations (ADR-0101, #311).
//!
//! A discrete three-state signal rather than a floating-point score: the thresholds behind each
//! operation's mapping are untuned (#273, #202) and a score would not calibrate across different
//! kinds of degradation. Each operation defines its own mapping onto these states.

/// Why an automatic operation produced no result or an uncertain one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReason {
    /// Nothing usable to lock onto (including only-diagonal lines outside the axis-deviation limit).
    NoFeatures,
    /// Features exist but are too few or mutually inconsistent.
    WeakEvidence,
    /// The input is degenerate (e.g. a near-empty, heavily clipped or single-spike histogram).
    AtypicalInput,
    /// No complete decoded frame exists. Produced by the orchestration layer, never by a pure estimator.
    DecodeIncomplete,
}

/// What an automatic operation concluded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AutoOutcome<T> {
    Confident(T),
    LowConfidence(T, AutoReason),
    NoResult(AutoReason),
}

impl<T> AutoOutcome<T> {
    /// The computed value, if any, regardless of confidence.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Confident(v) | Self::LowConfidence(v, _) => Some(v),
            Self::NoResult(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_is_present_unless_there_is_no_result() {
        assert_eq!(AutoOutcome::Confident(1).value(), Some(&1));
        assert_eq!(
            AutoOutcome::LowConfidence(2, AutoReason::WeakEvidence).value(),
            Some(&2)
        );
        assert_eq!(
            AutoOutcome::<i32>::NoResult(AutoReason::NoFeatures).value(),
            None
        );
    }
}
