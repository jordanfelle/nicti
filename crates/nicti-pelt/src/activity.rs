//! The activity/progress panel (#55): a persistent bottom status bar over Pounce's own job queue
//! -- collapsed, a bottleneck headline (#70/ADR-0070, via `nicti_pounce::hackles`) plus a
//! running/queued count and a CPU/RAM/VRAM/GPU/Disk readout; expanded (via the
//! `CollapsingHeader` below), one row per job with a progress indicator and a cancel button --
//! plus a live CPU-lane concurrency control. Read-only over [`Pounce::snapshot`] and
//! [`TelemetrySampler::sample`]; this module's only state is the caller-owned `bottleneck`
//! verdict (kept across frames for `hackles::classify`'s hysteresis), beyond what egui's own
//! `CollapsingHeader` already persists per-id.

use nicti_pounce::telemetry::TelemetrySampler;
use nicti_pounce::{hackles, JobState, JobStatus, Pounce};

pub fn show(
    ui: &mut egui::Ui,
    pounce: &Pounce,
    telemetry: &TelemetrySampler,
    bottleneck: &mut Option<hackles::Verdict>,
) {
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

        // Classified once per frame, before building any widgets, so the headline label and the
        // CPU/GPU/Disk readouts below always agree within one frame -- classifying inside the
        // `ui.horizontal` closure instead would leave the headline showing last frame's verdict
        // while the numbers next to it were already this frame's.
        let sample = telemetry.sample();
        if let Some(sample) = sample {
            let previous_limit = bottleneck.map(|v| v.limit).unwrap_or(hackles::Limit::Idle);
            *bottleneck = Some(hackles::classify(
                sample.host.cpu_usage_percent,
                sample.load.gpu_busy_percent,
                sample.load.disk_busy_percent,
                previous_limit,
            ));
        }
        // `bottleneck` stays whatever it was on a `None` sample (no telemetry yet, or -- should
        // never happen at this poll rate -- a rare in-window read) rather than resetting: the
        // last real verdict is still the best guess for "what's going on".

        ui.horizontal(|ui| {
            if let Some(verdict) = bottleneck.as_ref() {
                let (label, color) = match verdict.limit {
                    hackles::Limit::Idle => ("Idle", egui::Color32::GRAY),
                    hackles::Limit::Resource(hackles::Resource::Cpu) => {
                        ("Limit: CPU", color_for_level(Some(verdict.cpu), ui))
                    }
                    hackles::Limit::Resource(hackles::Resource::Gpu) => {
                        ("Limit: GPU", color_for_level(verdict.gpu, ui))
                    }
                    hackles::Limit::Resource(hackles::Resource::Disk) => {
                        ("Limit: Disk", color_for_level(verdict.disk, ui))
                    }
                };
                ui.colored_label(color, label);
                ui.separator();
            }

            ui.label(format!("{running} running \u{b7} {queued} queued"));
            ui.separator();

            // Telemetry itself may not have a sample yet (right after startup, before the
            // background sampler's first tick) -- shown as its own status, never a fabricated
            // reading, same honesty convention as the individual VRAM/GPU/Disk "n/a" cases below.
            match sample {
                Some(sample) => {
                    ui.label(format!("CPU {:.0}%", sample.host.cpu_usage_percent));
                    ui.label(format!(
                        "RAM {:.1}/{:.1} GB",
                        sample.host.used_memory_bytes as f64 / 1e9,
                        sample.host.total_memory_bytes as f64 / 1e9
                    ));
                    // VRAM is genuinely unavailable in this sandbox (no GPU adapter under WSL,
                    // and the DXGI source is Windows-only anyway) -- shown as "n/a", never a
                    // fabricated number, per this repo's own measured-vs-hypothesis convention
                    // (see `telemetry.rs`).
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

                    let gpu_level = bottleneck.as_ref().and_then(|v| v.gpu);
                    match sample.load.gpu_busy_percent {
                        Some(pct) => {
                            ui.colored_label(
                                color_for_level(gpu_level, ui),
                                format!("GPU {pct:.0}%"),
                            );
                        }
                        None => {
                            ui.label("GPU: n/a");
                        }
                    }

                    let disk_level = bottleneck.as_ref().and_then(|v| v.disk);
                    match sample.load.disk_busy_percent {
                        Some(pct) => {
                            ui.colored_label(
                                color_for_level(disk_level, ui),
                                format!("Disk {pct:.0}%"),
                            );
                        }
                        None => {
                            ui.label("Disk: n/a");
                        }
                    }
                }
                None => {
                    ui.label("Telemetry: warming up...");
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
                // A finished/cancelled job with no total (e.g. IngestJob, whose walk is always
                // lazy) stays in this branch forever -- `egui::Ui::spinner` requests a repaint
                // every frame it's drawn, so spinning it for a job that will never move again
                // would keep the app repainting continuously while Details is open, defeating
                // the whole on_change/request_repaint design (found by CodeRabbit's review).
                if matches!(status.state, JobState::Queued | JobState::Running) {
                    ui.spinner();
                }
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

/// Maps a `hackles::Level` to a display color -- `Calm` and the "no reading" case both fall back
/// to the theme's own default text color, so an unavailable GPU/Disk reading's "n/a" label (a
/// plain `ui.label`, not `colored_label`) is the only place absence is visually distinct from a
/// calm-but-present reading.
fn color_for_level(level: Option<hackles::Level>, ui: &egui::Ui) -> egui::Color32 {
    match level {
        Some(hackles::Level::Calm) => ui.visuals().text_color(),
        Some(hackles::Level::Busy) => egui::Color32::from_rgb(230, 160, 40),
        Some(hackles::Level::Saturated) => egui::Color32::RED,
        None => ui.visuals().text_color(),
    }
}
