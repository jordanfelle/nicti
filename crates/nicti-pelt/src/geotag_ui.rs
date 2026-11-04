//! #668: the UI half of geotagging. A "Geotag from GPX" form in the Library controls (GPX path,
//! camera clock offset and drift, max gap, overwrite) applies a log to the targeted photos (the
//! grid's selection, else the cursor photo) as a Pounce [`GeotagJob`]; an "Info" section shows the
//! cursor photo's details including its stored position, with a way to clear it
//! ([`ClearPositionJob`]). Neither touches the catalog or a sidecar on the UI thread: the GPX is
//! even read and parsed by the job's first step.
//!
//! There is no file picker in the tree and none was added (no new dependency): the GPX file is a
//! path text box, like the Folder field.
//!
//! Known limitation, shown in the form: a log with overlapping tracks from two devices is
//! interpolated in global time order, so a photo may be placed on the wrong device's track.

use std::path::PathBuf;
use std::sync::Arc;

use nicti_lair::pounce_jobs::{ClearPositionJob, ClearReport, GeotagJob, GpxSource, ReportSlot};
use nicti_lair::trail::GeotagReport;
use nicti_lair::{CatalogStore, GeotagOptions, Gps};
use nicti_pounce::Pounce;

/// How often the open Info section re-reads its photo.
const INFO_REFRESH: std::time::Duration = std::time::Duration::from_secs(3);
/// The camera-clock offset range the form accepts (UTC-14:00 .. UTC+14:00), in minutes.
const MAX_OFFSET_MINUTES: i32 = 14 * 60;
/// Drift bound: a day either way is already far beyond a real camera clock.
const MAX_DRIFT_SECONDS: f64 = 86_400.0;
/// Max-gap bound: a day.
const MAX_GAP_SECONDS: f64 = 86_400.0;

/// `46.000500° N, 7.001000° E, 1605 m` -- decimal degrees to six places (~0.1 m), hemisphere
/// letters instead of signs, altitude in whole metres when there is one.
pub fn format_position(g: &Gps) -> String {
    let ns = if g.latitude >= 0.0 { 'N' } else { 'S' };
    let ew = if g.longitude >= 0.0 { 'E' } else { 'W' };
    let mut s = format!(
        "{:.6}\u{b0} {ns}, {:.6}\u{b0} {ew}",
        g.latitude.abs(),
        g.longitude.abs()
    );
    if let Some(alt) = g.altitude {
        s.push_str(&format!(", {alt:.0} m"));
    }
    s
}

/// `UTC+02:00` / `UTC-05:30` / `UTC` for a camera clock offset in minutes.
pub fn format_offset(minutes: i32) -> String {
    if minutes == 0 {
        return "UTC".into();
    }
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}

/// One line for a finished geotag run.
pub fn summarize_geotag(r: &GeotagReport) -> String {
    if let Some(e) = &r.gpx_error {
        return format!("Geotag failed: {e}");
    }
    let mut parts = vec![format!("{} tagged", r.tagged)];
    let mut add = |n: usize, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    add(r.kept_existing, "kept their existing position");
    add(
        r.no_position,
        "outside the track (no position within the max gap)",
    );
    add(r.no_capture_time, "without a capture time");
    add(
        r.held_for_review,
        "held for review (sidecar changed; see the conflict panel)",
    );
    add(r.failed.len(), "failed");
    let mut s = format!(
        "Geotag{}: {}",
        if r.stopped { " (stopped)" } else { "" },
        parts.join(", ")
    );
    if let Some((id, why)) = r.failed.first() {
        s.push_str(&format!("; first failure (photo {id}): {why}"));
    }
    s
}

/// One line for a finished clear run.
pub fn summarize_clear(r: &ClearReport) -> String {
    let mut parts = vec![format!("{} cleared", r.cleared)];
    let mut add = |n: usize, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    add(r.nothing_to_clear, "had no position");
    add(
        r.needs_auto_write,
        "not cleared: the file carries a position and XMP auto-write is off (turn it on)",
    );
    add(
        r.held_for_review,
        "held for review (sidecar changed; see the conflict panel)",
    );
    add(r.failed.len(), "failed");
    let mut s = format!(
        "Clear position{}: {}",
        if r.stopped { " (stopped)" } else { "" },
        parts.join(", ")
    );
    if let Some((id, why)) = r.failed.first() {
        s.push_str(&format!("; first failure (photo {id}): {why}"));
    }
    s
}

/// What the Info section shows for one photo.
#[derive(Debug, Clone, PartialEq)]
pub struct InfoData {
    pub id: i64,
    pub file_name: String,
    pub captured_at: Option<String>,
    pub camera: Option<String>,
    pub size: Option<(u32, u32)>,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    pub position: Option<Gps>,
}

impl InfoData {
    /// Reads one photo's details; `None` when it is gone from the catalog.
    pub fn read(store: &dyn CatalogStore, id: i64) -> Result<Option<InfoData>, String> {
        let Some(a) = store.get_asset(id).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let position = store.get_gps(&[id]).map_err(|e| e.to_string())?.remove(&id);
        let camera = match (a.make.as_deref(), a.model.as_deref()) {
            // EXIF makes are often corporate names ("NIKON CORPORATION") whose first word the model
            // repeats ("NIKON Z 8"): show the model alone then.
            (Some(make), Some(model))
                if make.split_whitespace().next().is_some_and(|brand| {
                    model.to_lowercase().starts_with(&brand.to_lowercase())
                }) =>
            {
                Some(model.to_string())
            }
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (None, Some(model)) => Some(model.to_string()),
            (Some(make), None) => Some(make.to_string()),
            (None, None) => None,
        };
        Ok(Some(InfoData {
            id,
            file_name: a
                .rel_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&a.rel_path)
                .to_string(),
            captured_at: a.captured_at,
            camera,
            size: a.width.zip(a.height),
            lens: a.lens,
            iso: a.iso,
            position,
        }))
    }
}

/// Parsed form values, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct FormValues {
    pub path: PathBuf,
    pub opts: GeotagOptions,
}

/// Validates the form: a non-blank path and in-range clock values. (Whether the file exists or
/// parses is the job's business -- it reads it off the UI thread.)
pub fn validate_form(
    path: &str,
    offset_minutes: i32,
    drift_seconds: f64,
    max_gap_seconds: f64,
) -> Result<FormValues, String> {
    let path = path.trim().trim_matches('"');
    if path.is_empty() {
        return Err("Enter the path of a .gpx file.".into());
    }
    if !(-MAX_OFFSET_MINUTES..=MAX_OFFSET_MINUTES).contains(&offset_minutes) {
        return Err("The camera clock offset must be within UTC\u{b1}14:00.".into());
    }
    if !drift_seconds.is_finite() || drift_seconds.abs() > MAX_DRIFT_SECONDS {
        return Err("The clock drift must be within a day.".into());
    }
    if !max_gap_seconds.is_finite() || max_gap_seconds <= 0.0 || max_gap_seconds > MAX_GAP_SECONDS {
        return Err("The max gap must be between 1 second and a day.".into());
    }
    Ok(FormValues {
        path: PathBuf::from(path),
        opts: GeotagOptions {
            camera_utc_offset_minutes: offset_minutes,
            drift_seconds,
            max_gap_seconds,
        },
    })
}

pub struct GeotagUi {
    gpx_path: String,
    offset_minutes: i32,
    drift_seconds: f64,
    max_gap_seconds: f64,
    overwrite: bool,
    pending_geotag: Option<ReportSlot<GeotagReport>>,
    pending_clear: Option<ReportSlot<ClearReport>>,
    /// Last result / why a request was refused.
    status: Option<String>,
    /// The Info section's data for one photo, kept until the cursor moves or a job finishes.
    info: Option<Result<Option<InfoData>, String>>,
    info_for: Option<i64>,
    /// When `info` was read: it is re-read every few seconds so a position changed elsewhere (a
    /// rescan, an XMP import) shows up without moving the cursor.
    info_at: Option<std::time::Instant>,
    /// Tests draw the collapsed sections open.
    #[cfg(test)]
    force_open: bool,
}

impl Default for GeotagUi {
    fn default() -> Self {
        let d = GeotagOptions::default();
        GeotagUi {
            gpx_path: String::new(),
            offset_minutes: d.camera_utc_offset_minutes,
            drift_seconds: d.drift_seconds,
            max_gap_seconds: d.max_gap_seconds,
            overwrite: false,
            pending_geotag: None,
            pending_clear: None,
            status: None,
            info: None,
            info_for: None,
            info_at: None,
            #[cfg(test)]
            force_open: false,
        }
    }
}

impl GeotagUi {
    pub fn is_running(&self) -> bool {
        self.pending_geotag.is_some() || self.pending_clear.is_some()
    }

    /// Folds in a finished job. Returns `true` the frame one finishes (the Info data is stale).
    pub fn poll(&mut self) -> bool {
        let mut finished = false;
        if let Some(slot) = self.pending_geotag.clone() {
            if let Some(report) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                self.pending_geotag = None;
                self.status = Some(summarize_geotag(&report));
                finished = true;
            }
        }
        if let Some(slot) = self.pending_clear.clone() {
            if let Some(report) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                self.pending_clear = None;
                self.status = Some(summarize_clear(&report));
                finished = true;
            }
        }
        if finished {
            self.info = None;
            self.info_for = None;
            self.info_at = None;
        }
        finished
    }

    fn start_geotag(
        &mut self,
        store: &Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        targets: Vec<i64>,
        auto_write: bool,
    ) {
        match validate_form(
            &self.gpx_path,
            self.offset_minutes,
            self.drift_seconds,
            self.max_gap_seconds,
        ) {
            Ok(form) => {
                let (job, slot) = GeotagJob::new(
                    store.clone(),
                    targets,
                    GpxSource::File(form.path),
                    form.opts,
                    self.overwrite,
                    auto_write,
                );
                pounce.submit(Box::new(job));
                self.pending_geotag = Some(slot);
                self.status = Some("Geotagging\u{2026}".into());
            }
            Err(e) => self.status = Some(e),
        }
    }

    fn start_clear(
        &mut self,
        store: &Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        targets: Vec<i64>,
        auto_write: bool,
    ) {
        let (job, slot) = ClearPositionJob::new(store.clone(), targets, auto_write);
        pounce.submit(Box::new(job));
        self.pending_clear = Some(slot);
        self.status = Some("Clearing positions\u{2026}".into());
    }

    /// Draws the Geotag form and the Info section. `targets` is the selection (else the cursor
    /// photo); `busy_elsewhere` is true while another job that writes the catalog or sidecars
    /// runs.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        target_count: usize,
        targets: &dyn Fn() -> Vec<i64>,
        cursor: Option<i64>,
        auto_write: bool,
        busy_elsewhere: bool,
    ) {
        let idle = !self.is_running() && !busy_elsewhere;
        #[cfg(test)]
        let open = self.force_open.then_some(true);
        #[cfg(not(test))]
        let open: Option<bool> = None;
        egui::CollapsingHeader::new("Geotag from GPX")
            .default_open(false)
            .open(open)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("GPX file:");
                    ui.add_enabled(
                        !self.is_running(),
                        egui::TextEdit::singleline(&mut self.gpx_path)
                            .align(egui::Align2::LEFT_TOP)
                            .hint_text("Path to a .gpx track log")
                            .desired_width(280.0),
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Camera clock:");
                    ui.add_enabled(
                        !self.is_running(),
                        egui::DragValue::new(&mut self.offset_minutes)
                            .range(-MAX_OFFSET_MINUTES..=MAX_OFFSET_MINUTES)
                            .speed(15.0)
                            .suffix(" min"),
                    )
                    .on_hover_text(
                        "Minutes the camera clock is ahead of UTC (EXIF times carry no zone): \
                         120 = UTC+02:00, -300 = UTC-05:00.",
                    );
                    ui.label(format!("({})", format_offset(self.offset_minutes)));
                    ui.label("drift:");
                    ui.add_enabled(
                        !self.is_running(),
                        egui::DragValue::new(&mut self.drift_seconds)
                            .range(-MAX_DRIFT_SECONDS..=MAX_DRIFT_SECONDS)
                            .speed(0.5)
                            .suffix(" s"),
                    )
                    .on_hover_text("Seconds the camera clock runs ahead of true time.");
                    ui.label("max gap:");
                    ui.add_enabled(
                        !self.is_running(),
                        egui::DragValue::new(&mut self.max_gap_seconds)
                            .range(1.0..=MAX_GAP_SECONDS)
                            .speed(5.0)
                            .suffix(" s"),
                    )
                    .on_hover_text(
                        "The longest gap between track points that still places a photo.",
                    );
                });
                ui.checkbox(&mut self.overwrite, "Replace existing positions")
                    .on_hover_text("Off: photos that already have a position keep it.");
                ui.weak(
                    "A log with overlapping tracks from two devices is interpolated in time \
                     order, so a photo may land on the wrong device's track.",
                );
                if !auto_write {
                    ui.weak(
                        "XMP auto-write is off: positions are stored in the catalog and written \
                         to the sidecars when it is turned on.",
                    );
                }
                ui.horizontal(|ui| {
                    let n = target_count;
                    if ui
                        .add_enabled(
                            idle && n > 0,
                            egui::Button::new(format!("Apply to {n} photo(s)")),
                        )
                        .on_hover_text("Tags the selected photos (or the cursor photo).")
                        .clicked()
                    {
                        self.start_geotag(store, pounce, targets(), auto_write);
                    }
                    if ui
                        .add_enabled(idle && n > 0, egui::Button::new("Clear positions"))
                        .on_hover_text("Removes the stored position from the selected photos.")
                        .clicked()
                    {
                        self.start_clear(store, pounce, targets(), auto_write);
                    }
                });
                if let Some(msg) = &self.status {
                    ui.label(msg.clone());
                }
            });

        egui::CollapsingHeader::new("Info")
            .default_open(false)
            .open(open)
            .show(ui, |ui| {
                let Some(id) = cursor else {
                    ui.weak("No photo under the cursor.");
                    return;
                };
                let stale = self.info_at.is_none_or(|t| t.elapsed() > INFO_REFRESH);
                if self.info_for != Some(id) || stale {
                    self.info = Some(InfoData::read(store.as_ref(), id));
                    self.info_for = Some(id);
                    self.info_at = Some(std::time::Instant::now());
                }
                // Keep the section current while it is open and the app is otherwise idle.
                ui.ctx().request_repaint_after(INFO_REFRESH);
                match &self.info {
                    Some(Ok(Some(info))) => {
                        let info = info.clone();
                        egui::Grid::new("photo_info").num_columns(2).show(ui, |ui| {
                            ui.label("File");
                            ui.label(&info.file_name);
                            ui.end_row();
                            if let Some(t) = &info.captured_at {
                                ui.label("Captured");
                                ui.label(t);
                                ui.end_row();
                            }
                            if let Some(c) = &info.camera {
                                ui.label("Camera");
                                ui.label(c);
                                ui.end_row();
                            }
                            if let Some((w, h)) = info.size {
                                ui.label("Size");
                                ui.label(format!("{w} \u{d7} {h}"));
                                ui.end_row();
                            }
                            if let Some(l) = &info.lens {
                                ui.label("Lens");
                                ui.label(l);
                                ui.end_row();
                            }
                            if let Some(iso) = info.iso {
                                ui.label("ISO");
                                ui.label(iso.to_string());
                                ui.end_row();
                            }
                            ui.label("Position");
                            match &info.position {
                                Some(p) => ui.label(format_position(p)),
                                None => ui.weak("none"),
                            };
                            ui.end_row();
                        });
                        if info.position.is_some()
                            && ui
                                .add_enabled(idle, egui::Button::new("Clear position"))
                                .on_hover_text(
                                    "Removes this photo's stored position, from the catalog and \
                                     from its XMP.",
                                )
                                .clicked()
                        {
                            self.start_clear(store, pounce, vec![id], auto_write);
                        }
                    }
                    Some(Ok(None)) => {
                        ui.weak("This photo is no longer in the catalog.");
                    }
                    Some(Err(e)) => {
                        ui.colored_label(egui::Color32::RED, format!("Info unavailable: {e}"));
                    }
                    None => {}
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::{NewAsset, SqliteCatalog};

    #[test]
    fn positions_read_as_degrees_with_hemispheres_and_whole_metre_altitude() {
        let g = Gps {
            latitude: 46.0005,
            longitude: 7.001,
            altitude: Some(1605.4),
        };
        assert_eq!(
            format_position(&g),
            "46.000500\u{b0} N, 7.001000\u{b0} E, 1605 m"
        );
        let south_west = Gps {
            latitude: -33.8688,
            longitude: -151.2093,
            altitude: None,
        };
        assert_eq!(
            format_position(&south_west),
            "33.868800\u{b0} S, 151.209300\u{b0} W"
        );
    }

    #[test]
    fn clock_offsets_read_as_utc_plus_minus_hh_mm() {
        assert_eq!(format_offset(0), "UTC");
        assert_eq!(format_offset(120), "UTC+02:00");
        assert_eq!(format_offset(-330), "UTC-05:30");
        assert_eq!(format_offset(14 * 60), "UTC+14:00");
    }

    #[test]
    fn the_form_refuses_a_blank_path_and_out_of_range_clock_values() {
        let ok = validate_form("  \"/tmp/day.gpx\" ", 120, 2.5, 300.0).unwrap();
        assert_eq!(
            ok.path,
            PathBuf::from("/tmp/day.gpx"),
            "trimmed, quotes dropped"
        );
        assert_eq!(ok.opts.camera_utc_offset_minutes, 120);
        assert_eq!(ok.opts.drift_seconds, 2.5);
        for bad in [
            validate_form("", 0, 0.0, 300.0),
            validate_form("   ", 0, 0.0, 300.0),
            validate_form("a.gpx", 15 * 60, 0.0, 300.0),
            validate_form("a.gpx", 0, f64::NAN, 300.0),
            validate_form("a.gpx", 0, 1e9, 300.0),
            validate_form("a.gpx", 0, 0.0, 0.0),
            validate_form("a.gpx", 0, 0.0, f64::INFINITY),
            validate_form("a.gpx", 0, 0.0, 1e9),
        ] {
            assert!(bad.is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_geotag_summary_names_every_outcome_and_the_first_failure() {
        let r = GeotagReport {
            tagged: 5,
            kept_existing: 1,
            no_position: 2,
            no_capture_time: 3,
            held_for_review: 1,
            failed: vec![(9, "disk full".into())],
            stopped: true,
            gpx_error: None,
        };
        let s = summarize_geotag(&r);
        for want in [
            "(stopped)",
            "5 tagged",
            "1 kept their existing",
            "2 outside the track",
            "3 without a capture time",
            "1 held for review",
            "1 failed",
            "photo 9",
            "disk full",
        ] {
            assert!(s.contains(want), "{s}");
        }
        let bad = GeotagReport {
            gpx_error: Some("a.gpx: not xml".into()),
            ..Default::default()
        };
        assert_eq!(summarize_geotag(&bad), "Geotag failed: a.gpx: not xml");
        assert!(summarize_geotag(&GeotagReport::default()).contains("0 tagged"));
    }

    #[test]
    fn a_clear_summary_says_when_auto_write_is_needed() {
        let r = ClearReport {
            cleared: 2,
            needs_auto_write: 1,
            nothing_to_clear: 4,
            ..Default::default()
        };
        let s = summarize_clear(&r);
        assert!(
            s.contains("2 cleared") && s.contains("4 had no position"),
            "{s}"
        );
        assert!(s.contains("auto-write is off"), "{s}");
    }

    #[test]
    fn info_reads_the_photo_and_its_stored_position() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let vol = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(vol, "/r").unwrap();
        let id = store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: "2026/IMG_0001.NEF".into(),
                    rel_path_fold: "2026/img_0001.nef".into(),
                    size_bytes: 1,
                    mtime_unix: 0,
                    fingerprint: None,
                    natural_key: None,
                    make: Some("NIKON CORPORATION".into()),
                    model: Some("NIKON Z 8".into()),
                    captured_at: Some("2026:05:01 10:00:30".into()),
                    width: Some(8256),
                    height: Some(5504),
                    imported_at: 0,
                },
                None,
            )
            .unwrap();
        let before = InfoData::read(&store, id).unwrap().unwrap();
        assert_eq!(before.file_name, "IMG_0001.NEF");
        assert_eq!(
            before.camera.as_deref(),
            Some("NIKON Z 8"),
            "no doubled make"
        );
        assert_eq!(before.size, Some((8256, 5504)));
        assert!(before.position.is_none());

        let pos = Gps {
            latitude: 46.0,
            longitude: 7.0,
            altitude: Some(1600.0),
        };
        store.set_gps(id, Some(pos)).unwrap();
        let after = InfoData::read(&store, id).unwrap().unwrap();
        assert_eq!(after.position, Some(pos));
        assert!(
            InfoData::read(&store, id + 99).unwrap().is_none(),
            "gone photo"
        );
    }

    #[test]
    fn a_finished_job_is_folded_in_once_and_makes_the_info_stale() {
        let mut ui = GeotagUi::default();
        let slot: ReportSlot<GeotagReport> = Arc::new(std::sync::Mutex::new(None));
        ui.pending_geotag = Some(slot.clone());
        ui.info_for = Some(7);
        ui.info = Some(Ok(None));
        assert!(ui.is_running());
        assert!(!ui.poll(), "nothing finished yet");
        *slot.lock().unwrap() = Some(GeotagReport {
            tagged: 3,
            ..Default::default()
        });
        assert!(ui.poll());
        assert!(!ui.is_running());
        assert!(ui.status.as_deref().unwrap().contains("3 tagged"));
        assert!(
            ui.info.is_none() && ui.info_for.is_none(),
            "Info must re-read"
        );
        assert!(!ui.poll(), "folded in once");
    }

    /// Draws both sections open, for a photo with and without a position, idle and busy, and checks
    /// the stale-info refresh after a job: no panic, and the Info data is read for the cursor.
    #[test]
    fn the_form_and_info_section_draw_in_every_state() {
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let ids = crate::test_support::seed_assets(&store, 2);
        store
            .set_gps(
                ids[0],
                Some(Gps {
                    latitude: 46.0,
                    longitude: 7.0,
                    altitude: None,
                }),
            )
            .unwrap();
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        let pounce = Pounce::new(u64::MAX, 2, 1, || {});
        let ctx = egui::Context::default();
        let mut ui_state = GeotagUi {
            force_open: true,
            ..GeotagUi::default()
        };
        let frame = |ui_state: &mut GeotagUi, cursor: Option<i64>, auto: bool, busy: bool| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui_state.show(
                    ui,
                    &dyn_store,
                    &pounce,
                    ids.len(),
                    &|| ids.clone(),
                    cursor,
                    auto,
                    busy,
                );
            });
            out.textures_delta.clear();
        };
        frame(&mut ui_state, Some(ids[0]), true, false);
        assert_eq!(ui_state.info_for, Some(ids[0]));
        let shown = ui_state.info.clone().unwrap().unwrap().unwrap();
        assert!(
            shown.position.is_some(),
            "the cursor photo's position is read"
        );
        frame(&mut ui_state, Some(ids[1]), false, true);
        assert_eq!(ui_state.info_for, Some(ids[1]));
        assert!(ui_state
            .info
            .clone()
            .unwrap()
            .unwrap()
            .unwrap()
            .position
            .is_none());
        frame(&mut ui_state, None, true, false);
        // A position changed elsewhere shows up on the next periodic re-read, with no cursor move.
        store
            .set_gps(
                ids[1],
                Some(Gps {
                    latitude: 1.0,
                    longitude: 2.0,
                    altitude: None,
                }),
            )
            .unwrap();
        frame(&mut ui_state, Some(ids[1]), true, false);
        assert!(
            ui_state
                .info
                .clone()
                .unwrap()
                .unwrap()
                .unwrap()
                .position
                .is_none(),
            "fresh data is not re-read every frame"
        );
        ui_state.info_at = Some(std::time::Instant::now() - INFO_REFRESH * 2);
        frame(&mut ui_state, Some(ids[1]), true, false);
        assert!(
            ui_state
                .info
                .clone()
                .unwrap()
                .unwrap()
                .unwrap()
                .position
                .is_some(),
            "stale data is re-read"
        );
        // A missing photo is a message, not a panic.
        frame(&mut ui_state, Some(ids[1] + 100), true, false);
        assert!(matches!(ui_state.info, Some(Ok(None))));
        // Starting with a blank path is refused with a message and starts no job.
        ui_state.start_geotag(&dyn_store, &pounce, ids.clone(), true);
        assert!(!ui_state.is_running());
        assert!(ui_state.status.as_deref().unwrap().contains(".gpx"));
        pounce.shutdown();
    }
}
