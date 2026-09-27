//! VRAM budget admission control -- ADR-0054 decision rule #5: a background bake job is refused
//! (not queued to wait) when its declared `vram_bytes` exceeds the budget remaining after already
//! -admitted jobs, so the scheduler never oversubscribes VRAM and lets the driver OOM. Foreground
//! (live-render) jobs are never refused here -- ADR-0044 already sized the live suffix + present
//! -sample kernels to a fixed, small per-frame footprint (well under budget at both screen and
//! full res), and a UI that can't render at all because a background bake ate the budget is a
//! worse failure mode than temporarily starving a background job.

use crate::job::{JobId, Priority};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// `requested` would exceed `remaining` (the budget minus everything currently admitted).
    OverBudget { requested: u64, remaining: u64 },
}

/// Tracks how many bytes are currently reserved against a fixed budget. Reservations are held
/// only while a job's chunk is actually running (`admit`/`release` bracket each chunk, not the
/// whole job's lifetime) -- ADR-0044's own tiers already do LRU eviction for *resident* cache
/// data; this is a separate, narrower guard against transient in-flight overcommit.
#[derive(Debug)]
pub struct Admission {
    budget_bytes: u64,
    reserved_bytes: u64,
    admitted: Vec<(JobId, u64)>,
}

impl Admission {
    pub fn new(budget_bytes: u64) -> Self {
        Admission {
            budget_bytes,
            reserved_bytes: 0,
            admitted: Vec::new(),
        }
    }

    pub fn remaining(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.reserved_bytes)
    }

    /// Attempts to reserve `vram_bytes` for `job`. A [`Priority::Foreground`] job is always
    /// admitted (see this module's own doc comment); a [`Priority::Background`] job over the
    /// remaining budget is refused, not queued.
    pub fn admit(
        &mut self,
        job: JobId,
        priority: Priority,
        vram_bytes: u64,
    ) -> Result<(), AdmissionError> {
        if priority == Priority::Background && vram_bytes > self.remaining() {
            return Err(AdmissionError::OverBudget {
                requested: vram_bytes,
                remaining: self.remaining(),
            });
        }
        self.reserved_bytes += vram_bytes;
        self.admitted.push((job, vram_bytes));
        Ok(())
    }

    pub fn release(&mut self, job: JobId) {
        if let Some(pos) = self.admitted.iter().position(|(id, _)| *id == job) {
            let (_, bytes) = self.admitted.remove(pos);
            self.reserved_bytes = self.reserved_bytes.saturating_sub(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_background_job_within_budget() {
        let mut admission = Admission::new(1000);
        assert!(admission.admit(JobId(1), Priority::Background, 500).is_ok());
        assert_eq!(admission.remaining(), 500);
    }

    #[test]
    fn refuses_background_job_over_budget() {
        let mut admission = Admission::new(1000);
        admission
            .admit(JobId(1), Priority::Background, 800)
            .unwrap();
        let result = admission.admit(JobId(2), Priority::Background, 500);
        assert_eq!(
            result,
            Err(AdmissionError::OverBudget {
                requested: 500,
                remaining: 200
            })
        );
    }

    #[test]
    fn foreground_job_always_admitted_even_over_budget() {
        let mut admission = Admission::new(100);
        assert!(admission
            .admit(JobId(1), Priority::Foreground, 10_000)
            .is_ok());
    }

    #[test]
    fn release_frees_reserved_bytes() {
        let mut admission = Admission::new(1000);
        admission
            .admit(JobId(1), Priority::Background, 500)
            .unwrap();
        admission.release(JobId(1));
        assert_eq!(admission.remaining(), 1000);
    }

    #[test]
    fn releasing_unknown_job_is_a_noop() {
        let mut admission = Admission::new(1000);
        admission.release(JobId(99));
        assert_eq!(admission.remaining(), 1000);
    }
}
