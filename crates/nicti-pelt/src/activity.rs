//! The activity/progress panel (#55): a persistent bottom status bar over Pounce's own job queue
//! -- collapsed, a running/queued count plus a CPU/RAM/VRAM readout; expanded (via the
//! `CollapsingHeader` below), one row per job with a progress indicator and a cancel button --
//! plus a live CPU-lane concurrency control. Read-only over [`Pounce::snapshot`] and
//! [`TelemetrySampler::sample`]; this module owns no state of its own beyond what egui's own
//! `CollapsingHeader` already persists per-id.

use nicti_pounce::telemetry::TelemetrySampler;
use nicti_pounce::{JobState, JobStatus, Pounce};

pub fn show(ui: &mut egui::Ui, pounce: &Pounce, telemetry: &mut TelemetrySampler) {
    egui::Panel::bottom("activity").show(ui, |ui| {
        let statuses = pounce.snapshot();
        let running = statuses
            .iter()
            .filter(|s| s.state == JobState::Running)
            .count();
        let queued = statuses
            .iter()
            .filter(|s| s.state == JobState::Queued)
            .count();

        ui.horizontal(|ui| {
            ui.label(format!("{running} running \u{b7} {queued} queued"));
            ui.separator();

            let sample = telemetry.sample();
            ui.label(format!("CPU {:.0}%", sample.host.cpu_usage_percent));
            ui.label(format!(
                "RAM {:.1}/{:.1} GB",
                sample.host.used_memory_bytes as f64 / 1e9,
                sample.host.total_memory_bytes as f64 / 1e9
            ));
            // VRAM is genuinely unavailable in this sandbox (no GPU adapter under WSL, and the
            // DXGI source is Windows-only anyway) -- shown as "n/a", never a fabricated number,
            // per this repo's own measured-vs-hypothesis convention (see `telemetry.rs`).
            match sample.vram {
                Some(v) => {
                    ui.label(format!(
                        "VRAM {:.2}/{:.2} GB",
                        v.used_bytes as f64 / 1e9,
                        v.budget_bytes as f64 / 1e9
                    ));
                }
                None => {
                    ui.label("VRAM: n/a");
                }
            }

            ui.separator();
            ui.label("CPU jobs:");
            let mut limit = pounce.cpu_limit() as i32;
            if ui
                .add(egui::DragValue::new(&mut limit).range(1..=64))
                .changed()
            {
                pounce.set_cpu_limit(limit.max(1) as usize);
            }
        });

        egui::CollapsingHeader::new("Details")
            .id_salt("nicti_pelt_activity_details")
            .default_open(false)
            .show(ui, |ui| {
                if statuses.is_empty() {
                    ui.label("No jobs yet.");
                    return;
                }
                for status in &statuses {
                    show_job_row(ui, pounce, status);
                }
            });
    });
}

fn show_job_row(ui: &mut egui::Ui, pounce: &Pounce, status: &JobStatus) {
    ui.horizontal(|ui| {
        ui.label(format!("[{:?}]", status.lane));
        ui.label(&status.label);

        match status.progress.total {
            Some(total) => {
                let frac = if total == 0 {
                    1.0
                } else {
                    status.progress.done as f32 / total as f32
                };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
            }
            None => {
                ui.spinner();
                ui.label(format!("{} done", status.progress.done));
            }
        }

        match &status.state {
            JobState::Queued => {
                ui.label("queued");
            }
            JobState::Running => {
                ui.label("running");
            }
            JobState::Done => {
                ui.colored_label(egui::Color32::GREEN, "done");
            }
            JobState::Cancelled => {
                ui.colored_label(egui::Color32::YELLOW, "cancelled");
            }
            JobState::Failed(msg) => {
                ui.colored_label(egui::Color32::RED, format!("failed: {msg}"));
            }
        }

        if matches!(status.state, JobState::Queued | JobState::Running)
            && ui.button("Cancel").clicked()
        {
            pounce.cancel(status.id);
        }
    });
}
