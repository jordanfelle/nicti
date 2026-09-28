//! The hardware bottleneck classifier (#70/ADR-0070): turns a `telemetry::Sample`'s CPU/GPU/disk
//! readings into "which resource, if any, is currently the limit". Named for a cat's hackles --
//! raised when stressed -- since this module's whole job is noticing when a resource is under
//! real load. Pure and UI-agnostic: no `egui`, no I/O, so it's fully unit-testable without a
//! reference machine.
//!
//! VRAM deliberately doesn't feed this classifier (see the ADR) -- being close to the VRAM
//! budget isn't the same as being limited by it, and admission already has its own
//! skip-vs-queue signal in `admission.rs` if that's needed later.

/// A resource this classifier can name as the current limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Gpu,
    Disk,
}

/// The verdict's headline: a specific resource, or `Idle` when nothing is busy enough to blame.
/// There's no `Unknown` variant here -- CPU telemetry never fails (`sysinfo` always answers), so
/// every call to [`classify`] can name a real verdict. "No sample yet at all" (before
/// `TelemetrySampler`'s first background sample lands) is a `None` at the `Sample`/UI layer, one
/// level up from this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Resource(Resource),
    Idle,
}

/// How busy one resource is. Boundaries are inclusive on the lower end of each band ("below 60%"
/// is `Calm`; exactly 60% is already `Busy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Calm,
    Busy,
    Saturated,
}

/// Below this, a resource is `Calm` and never a candidate for the limit.
pub const BUSY_THRESHOLD: f32 = 60.0;
/// At or above this, a resource is `Saturated` rather than merely `Busy`. Purely a display
/// distinction -- both count equally as "busy enough to be a candidate" for [`classify`].
pub const SATURATED_THRESHOLD: f32 = 85.0;
/// A challenger must beat the current limit's own reading by more than this many percentage
/// points before it takes over. Without this, two resources sitting within a point of each other
/// would flip the headline every sample. Only applies while switching between two already-busy
/// resources -- entering or leaving `Idle` is never debounced, since that transition is real and
/// worth showing immediately.
pub const HYSTERESIS_MARGIN: f32 = 5.0;

fn level(percent: f32) -> Level {
    if percent >= SATURATED_THRESHOLD {
        Level::Saturated
    } else if percent >= BUSY_THRESHOLD {
        Level::Busy
    } else {
        Level::Calm
    }
}

/// One classified reading: the headline `limit`, plus each resource's own level for display
/// (`gpu`/`disk` are `None` when that reading isn't available).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub limit: Limit,
    pub cpu: Level,
    pub gpu: Option<Level>,
    pub disk: Option<Level>,
}

impl Verdict {
    /// The verdict before any real sample exists -- distinct from a classified `Idle` (which
    /// means "sampled and genuinely calm"), used only as `classify`'s `previous` argument on the
    /// very first call.
    pub fn idle_no_history() -> Self {
        Verdict {
            limit: Limit::Idle,
            cpu: Level::Calm,
            gpu: None,
            disk: None,
        }
    }
}

/// Classifies one reading, given the previous verdict's `limit` (for hysteresis -- see
/// [`HYSTERESIS_MARGIN`]). `cpu_percent` is always available; `gpu_percent`/`disk_percent` are
/// `None` when that source is unavailable (non-Windows, PDH setup failed, or the first sample
/// before a PDH rate counter has a valid delta yet).
///
/// The limit is the busiest (`Busy` or `Saturated`) resource. If none reach `Busy`, the verdict
/// is `Idle`. Among two or more busy candidates, the previous limit keeps the title unless a
/// challenger beats its reading by more than [`HYSTERESIS_MARGIN`] points.
pub fn classify(
    cpu_percent: f32,
    gpu_percent: Option<f32>,
    disk_percent: Option<f32>,
    previous: Limit,
) -> Verdict {
    let cpu_level = level(cpu_percent);
    let gpu_level = gpu_percent.map(level);
    let disk_level = disk_percent.map(level);

    let mut candidates: Vec<(Resource, f32)> = Vec::with_capacity(3);
    if cpu_level != Level::Calm {
        candidates.push((Resource::Cpu, cpu_percent));
    }
    if let (Some(percent), Some(lvl)) = (gpu_percent, gpu_level) {
        if lvl != Level::Calm {
            candidates.push((Resource::Gpu, percent));
        }
    }
    if let (Some(percent), Some(lvl)) = (disk_percent, disk_level) {
        if lvl != Level::Calm {
            candidates.push((Resource::Disk, percent));
        }
    }

    let limit = if candidates.is_empty() {
        Limit::Idle
    } else {
        let (mut best_resource, mut best_percent) = candidates[0];
        for &(resource, percent) in &candidates[1..] {
            if percent > best_percent {
                best_resource = resource;
                best_percent = percent;
            }
        }

        let previous_resource = match previous {
            Limit::Resource(r) => Some(r),
            Limit::Idle => None,
        };
        let previous_still_busy = previous_resource
            .and_then(|prev| candidates.iter().find(|(r, _)| *r == prev))
            .copied();

        match previous_still_busy {
            // `<=`, not `<`: a challenger beating the previous limit by *exactly*
            // `HYSTERESIS_MARGIN` still doesn't switch -- only strictly *more* than the margin
            // does (per this function's own doc comment). A caught-by-testing off-by-one: an
            // earlier version used `<`, which switched right at the boundary instead of holding.
            Some((prev_resource, prev_percent))
                if best_percent - prev_percent <= HYSTERESIS_MARGIN =>
            {
                Limit::Resource(prev_resource)
            }
            _ => Limit::Resource(best_resource),
        }
    };

    Verdict {
        limit,
        cpu: cpu_level,
        gpu: gpu_level,
        disk: disk_level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_below_busy_threshold() {
        let verdict = classify(10.0, Some(5.0), Some(20.0), Limit::Idle);
        assert_eq!(verdict.limit, Limit::Idle);
        assert_eq!(verdict.cpu, Level::Calm);
        assert_eq!(verdict.gpu, Some(Level::Calm));
        assert_eq!(verdict.disk, Some(Level::Calm));
    }

    #[test]
    fn exactly_at_busy_threshold() {
        let verdict = classify(60.0, None, None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn just_below_busy_threshold() {
        let verdict = classify(59.9, None, None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Idle);
        assert_eq!(verdict.cpu, Level::Calm);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn exactly_at_saturated_threshold() {
        let verdict = classify(85.0, None, None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Saturated);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn just_below_saturated_threshold() {
        let verdict = classify(84.9, None, None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn gpu_and_disk_none() {
        let verdict = classify(10.0, None, None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Idle);
        assert_eq!(verdict.cpu, Level::Calm);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn two_busy_candidates_no_history() {
        let verdict = classify(70.0, Some(90.0), None, Limit::Idle);
        assert_eq!(verdict.limit, Limit::Resource(Resource::Gpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Saturated));
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn hysteresis_holds() {
        let verdict = classify(70.0, Some(74.0), None, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Busy));
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn hysteresis_switches() {
        // gpu=76.0 beats cpu=70.0 by 6.0, more than HYSTERESIS_MARGIN (5.0) -- switches.
        // 76.0 is Busy, not Saturated (SATURATED_THRESHOLD is 85.0) -- a fix from the
        // local-model draft, which had asserted Saturated here.
        let verdict = classify(70.0, Some(76.0), None, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.limit, Limit::Resource(Resource::Gpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Busy));
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn hysteresis_boundary() {
        let verdict = classify(70.0, Some(75.0), None, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Busy));
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn previous_limit_drops_out_of_contention() {
        let verdict = classify(70.0, Some(20.0), None, Limit::Resource(Resource::Gpu));
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Calm));
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn previous_limit_reading_is_none() {
        let verdict = classify(70.0, None, None, Limit::Resource(Resource::Disk));
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, None);
        assert_eq!(verdict.disk, None);
    }

    #[test]
    fn all_three_tied_and_busy_prefers_cpu() {
        // Not just "one of the three" (the local-model draft's weaker assertion): candidates
        // are built CPU-then-GPU-then-Disk, and the fold that picks `best` only overtakes on a
        // strictly-greater percent, so an exact tie deterministically keeps the first-pushed
        // candidate, CPU.
        let verdict = classify(70.0, Some(70.0), Some(70.0), Limit::Idle);
        assert_eq!(verdict.limit, Limit::Resource(Resource::Cpu));
        assert_eq!(verdict.cpu, Level::Busy);
        assert_eq!(verdict.gpu, Some(Level::Busy));
        assert_eq!(verdict.disk, Some(Level::Busy));
    }
}
