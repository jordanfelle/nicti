//! The Export dialog, progress line and result summary (#57).
//!
//! [`ExportUi`] owns everything about exporting that lives in the UI: the presets, the working
//! copy of the settings while the dialog is open, the active [`ExportRun`], and the last report.
//! `PeltApp` asks it to open ([`ExportUi::request`]), draws it each frame, and supplies an
//! [`ExportEnv`] only at the moment the user presses Export.
//!
//! The dialog edits a plain [`ExportSpec`]; validation (including both templates) is
//! [`ExportSpec::validate`], the same gate `ExportRun::start` applies, so the Export button is
//! enabled exactly when starting would succeed.

use std::path::{Path, PathBuf};

use crate::folder_dialog::FolderPicker;
use crate::fur::{self, SliderSpec};
use nicti_preen::naming::{AssetFacts, Template};
use nicti_preen::spec::{
    Anchor, BitDepth, CollisionPolicy, DestinationBase, ExportFormat, ExportSpace, ExportSpec,
    FormatSpec, MetadataPolicy, ResizeMode, Subsampling, TiffCompression, WatermarkSpec,
};

use super::jobs::{ExportEnv, ExportReport, ExportRun};
use super::presets::{presets_file_for, PresetStore};

/// Files listed individually in the result summary before "and N more".
const SUMMARY_ROWS: usize = 12;

/// What the user asked of the dialog this frame.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Start,
    Close,
}

/// Which resize control is showing (the spec's `ResizeMode` carries its values, so the combo needs
/// a value-free discriminant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeKind {
    None,
    LongEdge,
    ShortEdge,
    Fit,
    Megapixels,
}

impl ResizeKind {
    const ALL: [ResizeKind; 5] = [
        ResizeKind::None,
        ResizeKind::LongEdge,
        ResizeKind::ShortEdge,
        ResizeKind::Fit,
        ResizeKind::Megapixels,
    ];

    fn of(mode: ResizeMode) -> Self {
        match mode {
            ResizeMode::None => ResizeKind::None,
            ResizeMode::LongEdge(_) => ResizeKind::LongEdge,
            ResizeMode::ShortEdge(_) => ResizeKind::ShortEdge,
            ResizeMode::Fit { .. } => ResizeKind::Fit,
            ResizeMode::Megapixels(_) => ResizeKind::Megapixels,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ResizeKind::None => "Original size",
            ResizeKind::LongEdge => "Long edge",
            ResizeKind::ShortEdge => "Short edge",
            ResizeKind::Fit => "Fit in a box",
            ResizeKind::Megapixels => "Megapixels",
        }
    }

    /// The mode for this kind, keeping the numbers of `previous` where they carry over.
    fn to_mode(self, previous: ResizeMode) -> ResizeMode {
        let px = match previous {
            ResizeMode::LongEdge(v) | ResizeMode::ShortEdge(v) => v,
            ResizeMode::Fit { width, .. } => width,
            _ => 2048,
        };
        match self {
            ResizeKind::None => ResizeMode::None,
            ResizeKind::LongEdge => ResizeMode::LongEdge(px),
            ResizeKind::ShortEdge => ResizeMode::ShortEdge(px),
            ResizeKind::Fit => ResizeMode::Fit {
                width: px,
                height: px,
            },
            ResizeKind::Megapixels => ResizeMode::Megapixels(match previous {
                ResizeMode::Megapixels(v) => v,
                _ => 8.0,
            }),
        }
    }
}

/// The open dialog's working state.
struct Dialog {
    ids: Vec<i64>,
    /// "3 selected photos" / "the photo on screen": what the export applies to.
    scope: String,
    spec: ExportSpec,
    /// Up to 3 photos' facts, for the live filename preview.
    samples: Vec<AssetFacts>,
    preset_name: String,
    selected_preset: Option<String>,
    message: Option<String>,
    /// #342: the destination folder's native Browse dialog.
    folder_picker: FolderPicker,
}

pub struct ExportUi {
    presets: PresetStore,
    presets_path: PathBuf,
    dialog: Option<Dialog>,
    run: Option<ExportRun>,
    /// The finished run's report, shown until dismissed.
    report: Option<ExportReport>,
    /// A failed start (planning/watermark/catalog), shown in the dialog.
    start_error: Option<String>,
}

impl ExportUi {
    pub fn new(catalog_path: &Path) -> Self {
        let presets_path = presets_file_for(catalog_path);
        ExportUi {
            presets: PresetStore::load(&presets_path),
            presets_path,
            dialog: None,
            run: None,
            report: None,
            start_error: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// Whether there is a progress line or result summary to draw.
    pub fn has_status(&self) -> bool {
        self.run.is_some() || self.report.is_some()
    }

    /// Opens the dialog for `ids`. Ignored while a run is active or `ids` is empty. `samples`
    /// (the first few photos' facts) feed the filename preview.
    pub fn request(&mut self, ids: Vec<i64>, scope: String, samples: Vec<AssetFacts>) {
        if ids.is_empty() || self.is_running() {
            return;
        }
        self.report = None;
        self.start_error = None;
        self.dialog = Some(Dialog {
            ids,
            scope,
            spec: self.presets.last.clone(),
            samples,
            preset_name: String::new(),
            selected_preset: None,
            message: None,
            folder_picker: FolderPicker::default(),
        });
    }

    /// Collects a finished run's report. Call once per frame.
    pub fn poll(&mut self) {
        if let Some(run) = &self.run {
            if let Some(report) = run.poll() {
                self.report = Some(report);
                self.run = None;
            }
        }
    }

    /// Cancels the active run, if any.
    pub fn cancel(&self) {
        if let Some(run) = &self.run {
            run.cancel();
        }
    }

    /// Draws the dialog (a modal) while open. `make_env` is called only when the user starts the
    /// export, so the caller can build it from live app state.
    pub fn show_dialog(&mut self, ctx: &egui::Context, make_env: &dyn Fn() -> Option<ExportEnv>) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut action = Action::None;
        let modal = egui::Modal::new(egui::Id::new("nicti_export_dialog")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            egui::ScrollArea::vertical()
                .max_height(ctx.content_rect().height() * 0.8)
                .show(ui, |ui| {
                    action = self.contents(ui, &mut dialog);
                });
        });
        if modal.should_close() {
            action = Action::Close;
        }

        match action {
            Action::None => self.dialog = Some(dialog),
            Action::Close => {}
            Action::Start => {
                self.presets.last = dialog.spec.clone();
                if let Err(e) = self.presets.save(&self.presets_path) {
                    // Not fatal: the export itself doesn't depend on the preset file.
                    dialog.message = Some(format!("Couldn't save your settings: {e}"));
                }
                match make_env() {
                    None => {
                        self.start_error = Some("The catalog isn't open.".into());
                        self.dialog = Some(dialog);
                    }
                    Some(env) => match ExportRun::start(env, &dialog.ids, dialog.spec.clone()) {
                        Ok(run) => {
                            self.start_error = None;
                            self.run = Some(run);
                        }
                        Err(e) => {
                            self.start_error = Some(e.to_string());
                            self.dialog = Some(dialog);
                        }
                    },
                }
            }
        }
    }

    /// The progress line (with Cancel) while a run is active, then the result summary until
    /// dismissed. Draw it in a panel or window of the caller's choosing.
    pub fn show_status(&mut self, ui: &mut egui::Ui) {
        if let Some(run) = &self.run {
            let (done, total) = run.progress();
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Exporting {done} of {total}…"));
                cancel = ui.button("Cancel export").clicked();
            });
            if cancel {
                run.cancel();
            }
            return;
        }
        let Some(report) = &self.report else {
            return;
        };
        let mut dismiss = false;
        ui.horizontal(|ui| {
            let mut line = format!("Exported {} of {}", report.exported.len(), report.total);
            if !report.skipped.is_empty() {
                line += &format!(", skipped {}", report.skipped.len());
            }
            if !report.failed.is_empty() {
                line += &format!(", {} failed", report.failed.len());
            }
            if report.cancelled > 0 {
                line += &format!(", {} cancelled", report.cancelled);
            }
            let colour = if report.failed.is_empty() {
                ui.visuals().text_color()
            } else {
                egui::Color32::YELLOW
            };
            ui.colored_label(colour, line);
            dismiss = ui.button("Dismiss").clicked();
        });
        if !report.failed.is_empty() || !report.warnings.is_empty() {
            egui::CollapsingHeader::new("Details")
                .default_open(true)
                .show(ui, |ui| {
                    for w in &report.warnings {
                        ui.label(format!("• {w}"));
                    }
                    for (id, why) in report.failed.iter().take(SUMMARY_ROWS) {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("• photo {id}: {why}"));
                    }
                    if report.failed.len() > SUMMARY_ROWS {
                        ui.label(format!("…and {} more", report.failed.len() - SUMMARY_ROWS));
                    }
                });
        }
        if dismiss {
            self.report = None;
        }
    }

    // --- dialog contents ------------------------------------------------------------------------

    fn contents(&mut self, ui: &mut egui::Ui, d: &mut Dialog) -> Action {
        ui.heading("Export");
        ui.label(format!("Exporting {}.", d.scope));
        ui.add_space(4.0);

        self.preset_row(ui, d);
        ui.separator();
        format_section(ui, &mut d.spec);
        ui.separator();
        size_section(ui, &mut d.spec);
        ui.separator();
        metadata_section(ui, &mut d.spec);
        ui.separator();
        watermark_section(ui, &mut d.spec);
        ui.separator();
        naming_section(ui, d);
        ui.separator();
        destination_section(ui, &mut d.spec, &mut d.folder_picker);
        ui.add_space(6.0);

        let validity = d.spec.validate();
        if let Err(e) = &validity {
            ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
        }
        if let Some(e) = &self.start_error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        if let Some(m) = &d.message {
            ui.label(m);
        }

        let mut action = Action::None;
        ui.horizontal(|ui| {
            // Not while Browse is open: the pick would land after Export started with the old path.
            let start = ui.add_enabled(
                validity.is_ok() && !d.folder_picker.is_open(),
                egui::Button::new("Export"),
            );
            if start.clicked() {
                action = Action::Start;
            }
            if ui.button("Cancel").clicked() {
                action = Action::Close;
            }
        });
        action
    }

    fn preset_row(&mut self, ui: &mut egui::Ui, d: &mut Dialog) {
        ui.horizontal(|ui| {
            ui.label("Preset");
            let shown = d
                .selected_preset
                .clone()
                .unwrap_or_else(|| "(custom)".to_string());
            egui::ComboBox::from_id_salt("export_preset")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    for p in self.presets.all() {
                        let selected = d.selected_preset.as_deref() == Some(&p.name);
                        if ui.selectable_label(selected, &p.name).clicked() {
                            d.spec = p.spec.clone();
                            d.preset_name = if PresetStore::is_builtin(&p.name) {
                                String::new()
                            } else {
                                p.name.clone()
                            };
                            d.selected_preset = Some(p.name);
                            d.message = None;
                        }
                    }
                });
            ui.text_edit_singleline(&mut d.preset_name)
                .on_hover_text("Name to save these settings under");
            if ui.button("Save preset").clicked() {
                d.message = Some(match self.presets.upsert(&d.preset_name, d.spec.clone()) {
                    Ok(()) => {
                        d.selected_preset = Some(d.preset_name.trim().to_string());
                        match self.presets.save(&self.presets_path) {
                            Ok(()) => format!("Saved preset \"{}\".", d.preset_name.trim()),
                            Err(e) => format!("Couldn't save the preset file: {e}"),
                        }
                    }
                    Err(e) => e.to_string(),
                });
            }
            let deletable = d
                .selected_preset
                .as_deref()
                .is_some_and(|n| !PresetStore::is_builtin(n));
            if ui
                .add_enabled(deletable, egui::Button::new("Delete"))
                .clicked()
            {
                if let Some(name) = d.selected_preset.take() {
                    self.presets.delete(&name);
                    let _ = self.presets.save(&self.presets_path);
                    d.message = Some(format!("Deleted preset \"{name}\"."));
                }
            }
        });
    }
}

// --- sections -----------------------------------------------------------------------------------

fn format_section(ui: &mut egui::Ui, spec: &mut ExportSpec) {
    ui.strong("Format");
    ui.horizontal(|ui| {
        let mut kind = spec.format.format();
        egui::ComboBox::from_id_salt("export_format")
            .selected_text(match kind {
                ExportFormat::Jpeg => "JPEG",
                ExportFormat::Png => "PNG",
                ExportFormat::Tiff => "TIFF",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut kind, ExportFormat::Jpeg, "JPEG");
                ui.selectable_value(&mut kind, ExportFormat::Png, "PNG");
                ui.selectable_value(&mut kind, ExportFormat::Tiff, "TIFF");
            });
        if kind != spec.format.format() {
            spec.format = match kind {
                ExportFormat::Jpeg => FormatSpec::default(),
                ExportFormat::Png => FormatSpec::Png {
                    depth: BitDepth::Eight,
                },
                ExportFormat::Tiff => FormatSpec::Tiff {
                    depth: BitDepth::Sixteen,
                    compression: TiffCompression::Deflate,
                },
            };
        }
        match &mut spec.format {
            FormatSpec::Jpeg {
                quality,
                subsampling,
            } => {
                let mut q = f32::from(*quality);
                const QUALITY: SliderSpec =
                    SliderSpec::new("export-quality", "Quality", 1.0, 100.0, 90.0).step(1.0, 0);
                if fur::slider(ui, &QUALITY, &mut q, true).changed {
                    *quality = q.round() as u8;
                }
                egui::ComboBox::from_id_salt("export_subsampling")
                    .selected_text(match subsampling {
                        Subsampling::S420 => "4:2:0",
                        Subsampling::S444 => "4:4:4",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(subsampling, Subsampling::S420, "4:2:0 (smaller)");
                        ui.selectable_value(
                            subsampling,
                            Subsampling::S444,
                            "4:4:4 (sharper color)",
                        );
                    });
            }
            FormatSpec::Png { depth } => depth_picker(ui, depth),
            FormatSpec::Tiff { depth, compression } => {
                depth_picker(ui, depth);
                egui::ComboBox::from_id_salt("export_tiff_compression")
                    .selected_text(match compression {
                        TiffCompression::None => "No compression",
                        TiffCompression::Lzw => "LZW",
                        TiffCompression::Deflate => "Deflate (ZIP)",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(compression, TiffCompression::None, "No compression");
                        ui.selectable_value(compression, TiffCompression::Lzw, "LZW");
                        ui.selectable_value(compression, TiffCompression::Deflate, "Deflate (ZIP)");
                    });
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Color space");
        egui::ComboBox::from_id_salt("export_space")
            .selected_text(match spec.color_space {
                ExportSpace::Srgb => "sRGB",
                ExportSpace::DisplayP3 => "Display P3",
                ExportSpace::AdobeRgb => "Adobe RGB (1998)",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut spec.color_space, ExportSpace::Srgb, "sRGB");
                ui.selectable_value(&mut spec.color_space, ExportSpace::DisplayP3, "Display P3");
                ui.selectable_value(
                    &mut spec.color_space,
                    ExportSpace::AdobeRgb,
                    "Adobe RGB (1998)",
                );
            });
    });
}

fn depth_picker(ui: &mut egui::Ui, depth: &mut BitDepth) {
    ui.selectable_value(depth, BitDepth::Eight, "8-bit");
    ui.selectable_value(depth, BitDepth::Sixteen, "16-bit");
}

fn size_section(ui: &mut egui::Ui, spec: &mut ExportSpec) {
    ui.strong("Size");
    ui.horizontal(|ui| {
        let mut kind = ResizeKind::of(spec.resize.mode);
        egui::ComboBox::from_id_salt("export_resize")
            .selected_text(kind.label())
            .show_ui(ui, |ui| {
                for k in ResizeKind::ALL {
                    ui.selectable_value(&mut kind, k, k.label());
                }
            });
        if kind != ResizeKind::of(spec.resize.mode) {
            spec.resize.mode = kind.to_mode(spec.resize.mode);
        }
        match &mut spec.resize.mode {
            ResizeMode::None => {}
            ResizeMode::LongEdge(px) | ResizeMode::ShortEdge(px) => {
                ui.add(egui::DragValue::new(px).range(1..=60_000).suffix(" px"));
            }
            ResizeMode::Fit { width, height } => {
                ui.add(egui::DragValue::new(width).range(1..=60_000).suffix(" px"));
                ui.label("×");
                ui.add(egui::DragValue::new(height).range(1..=60_000).suffix(" px"));
            }
            ResizeMode::Megapixels(mp) => {
                ui.add(
                    egui::DragValue::new(mp)
                        .range(0.1..=1000.0)
                        .speed(0.1)
                        .suffix(" MP"),
                );
            }
        }
        if spec.resize.mode != ResizeMode::None {
            ui.checkbox(&mut spec.resize.dont_enlarge, "Don't enlarge");
        }
    });
    ui.horizontal(|ui| {
        ui.label("Resolution");
        ui.add(
            egui::DragValue::new(&mut spec.dpi)
                .range(1..=10_000)
                .suffix(" dpi"),
        )
        .on_hover_text("Written into the file; never resamples the image");
    });
}

fn metadata_section(ui: &mut egui::Ui, spec: &mut ExportSpec) {
    ui.strong("Metadata");
    let m = &mut spec.metadata;
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("export_metadata")
            .selected_text(match m.policy {
                MetadataPolicy::All => "All metadata",
                MetadataPolicy::CopyrightOnly => "Copyright only",
                MetadataPolicy::None => "None",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut m.policy, MetadataPolicy::All, "All metadata");
                ui.selectable_value(
                    &mut m.policy,
                    MetadataPolicy::CopyrightOnly,
                    "Copyright only",
                );
                ui.selectable_value(&mut m.policy, MetadataPolicy::None, "None");
            });
        ui.add_enabled(
            m.policy == MetadataPolicy::All,
            egui::Checkbox::new(&mut m.include_keywords, "Include keywords"),
        );
    });
    optional_text(ui, "Artist", &mut m.artist);
    optional_text(ui, "Copyright", &mut m.copyright);
}

/// A labelled text box editing an `Option<String>` (empty = `None`).
fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = value.clone().unwrap_or_default();
        if ui.text_edit_singleline(&mut text).changed() {
            *value = (!text.is_empty()).then_some(text);
        }
    });
}

fn watermark_section(ui: &mut egui::Ui, spec: &mut ExportSpec) {
    ui.strong("Watermark");
    let mut enabled = spec.watermark.is_some();
    if ui
        .checkbox(&mut enabled, "Add a logo (SVG or PNG)")
        .changed()
    {
        spec.watermark = enabled.then(WatermarkSpec::default);
    }
    let Some(w) = spec.watermark.as_mut() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label("File");
        let mut path = w.path.to_string_lossy().into_owned();
        if ui.text_edit_singleline(&mut path).changed() {
            w.path = PathBuf::from(path);
        }
    });
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("export_wm_anchor")
            .selected_text(anchor_label(w.anchor))
            .show_ui(ui, |ui| {
                for a in ANCHORS {
                    ui.selectable_value(&mut w.anchor, a, anchor_label(a));
                }
            });
    });
    const WIDTH: SliderSpec = SliderSpec::new("export-wm-width", "Width", 1.0, 100.0, 15.0)
        .step(1.0, 0)
        .unit("%");
    const OPACITY: SliderSpec =
        SliderSpec::new("export-wm-opacity", "Opacity", 0.05, 1.0, 1.0).percent();
    const INSET: SliderSpec = SliderSpec::new("export-wm-inset", "Inset", 0.0, 20.0, 2.0)
        .step(0.5, 1)
        .unit("%");
    fur::slider(ui, &WIDTH, &mut w.scale_pct, true);
    fur::slider(ui, &OPACITY, &mut w.opacity, true);
    fur::slider(ui, &INSET, &mut w.inset_pct, true);
}

const ANCHORS: [Anchor; 9] = [
    Anchor::TopLeft,
    Anchor::Top,
    Anchor::TopRight,
    Anchor::Left,
    Anchor::Center,
    Anchor::Right,
    Anchor::BottomLeft,
    Anchor::Bottom,
    Anchor::BottomRight,
];

fn anchor_label(a: Anchor) -> &'static str {
    match a {
        Anchor::TopLeft => "Top left",
        Anchor::Top => "Top",
        Anchor::TopRight => "Top right",
        Anchor::Left => "Left",
        Anchor::Center => "Center",
        Anchor::Right => "Right",
        Anchor::BottomLeft => "Bottom left",
        Anchor::Bottom => "Bottom",
        Anchor::BottomRight => "Bottom right",
    }
}

fn naming_section(ui: &mut egui::Ui, d: &mut Dialog) {
    ui.strong("File names");
    ui.horizontal(|ui| {
        ui.label("Template");
        ui.text_edit_singleline(&mut d.spec.naming.template)
            .on_hover_text(
                "{Filename} {Sequence} {Sequence:4} {Date} {Date:YYYY-MM-DD} {Rating} {Make} {Model} {Folder}",
            );
        ui.label("start at");
        ui.add(egui::DragValue::new(&mut d.spec.naming.sequence_start).range(0..=999_999_999));
    });
    match preview_names(&d.spec, &d.samples) {
        Ok(names) => {
            for n in names {
                ui.weak(n);
            }
        }
        Err(e) => {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    }
}

/// The first few output file names for the current template, or the template's parse error.
fn preview_names(spec: &ExportSpec, samples: &[AssetFacts]) -> Result<Vec<String>, String> {
    let template = Template::parse(&spec.naming.template).map_err(|e| e.to_string())?;
    let ext = match spec.format.format() {
        ExportFormat::Jpeg => "jpg",
        ExportFormat::Png => "png",
        ExportFormat::Tiff => "tif",
    };
    Ok(samples
        .iter()
        .enumerate()
        .map(|(i, facts)| {
            let seq = spec.naming.sequence_start.saturating_add(i as u32);
            format!("{}.{ext}", template.render_filename(facts, seq))
        })
        .collect())
}

fn destination_section(ui: &mut egui::Ui, spec: &mut ExportSpec, picker: &mut FolderPicker) {
    ui.strong("Destination");
    let mut same = matches!(spec.destination.base, DestinationBase::SameAsSource);
    ui.horizontal(|ui| {
        ui.radio_value(&mut same, true, "Next to each original");
        ui.radio_value(&mut same, false, "In a folder");
    });
    match (&spec.destination.base, same) {
        (DestinationBase::Folder(_), true) => spec.destination.base = DestinationBase::SameAsSource,
        (DestinationBase::SameAsSource, false) => {
            spec.destination.base = DestinationBase::Folder(PathBuf::new())
        }
        _ => {}
    }
    if let DestinationBase::Folder(path) = &mut spec.destination.base {
        ui.horizontal(|ui| {
            ui.label("Folder");
            let mut text = path.to_string_lossy().into_owned();
            if ui
                .add_enabled(
                    !picker.is_open(),
                    egui::TextEdit::singleline(&mut text).hint_text("C:\\Exports"),
                )
                .changed()
            {
                *path = PathBuf::from(text);
            }
            if ui
                .add_enabled(!picker.is_open(), egui::Button::new("Browse..."))
                .clicked()
            {
                picker.open(ui.ctx(), "Export destination", &path.to_string_lossy());
            }
        });
    }
    // Polled whether or not the Folder row is showing, so a pick made before switching to "Next to
    // each original" is dropped instead of overwriting the path when the user switches back.
    if let Some(chosen) = picker.poll() {
        if let DestinationBase::Folder(path) = &mut spec.destination.base {
            *path = chosen;
        }
    }
    let mut sub = spec.destination.subfolder.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Subfolder");
        if ui
            .add(
                egui::TextEdit::singleline(&mut sub)
                    .hint_text("optional, e.g. {Date:YYYY}/{Folder}"),
            )
            .changed()
        {
            spec.destination.subfolder = (!sub.is_empty()).then_some(sub.clone());
        }
    });
    ui.horizontal(|ui| {
        ui.label("If a file exists");
        egui::ComboBox::from_id_salt("export_collision")
            .selected_text(match spec.collision {
                CollisionPolicy::UniqueSuffix => "Keep both (add -2, -3…)",
                CollisionPolicy::Overwrite => "Overwrite",
                CollisionPolicy::Skip => "Skip",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut spec.collision,
                    CollisionPolicy::UniqueSuffix,
                    "Keep both (add -2, -3…)",
                );
                ui.selectable_value(&mut spec.collision, CollisionPolicy::Overwrite, "Overwrite");
                ui.selectable_value(&mut spec.collision, CollisionPolicy::Skip, "Skip");
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_preen::spec::NamingSpec;

    fn facts(stem: &str) -> AssetFacts {
        AssetFacts {
            asset_id: 1,
            stem: stem.into(),
            folder: "shoot".into(),
            mtime_unix: 1_700_000_000,
            ..AssetFacts::default()
        }
    }

    #[test]
    fn resize_kinds_round_trip_and_carry_their_numbers() {
        assert_eq!(ResizeKind::of(ResizeMode::None), ResizeKind::None);
        assert_eq!(
            ResizeKind::ShortEdge.to_mode(ResizeMode::LongEdge(1500)),
            ResizeMode::ShortEdge(1500)
        );
        assert_eq!(
            ResizeKind::Fit.to_mode(ResizeMode::LongEdge(1500)),
            ResizeMode::Fit {
                width: 1500,
                height: 1500
            }
        );
        assert_eq!(
            ResizeKind::LongEdge.to_mode(ResizeMode::None),
            ResizeMode::LongEdge(2048)
        );
        assert_eq!(
            ResizeKind::Megapixels.to_mode(ResizeMode::None),
            ResizeMode::Megapixels(8.0)
        );
        for k in ResizeKind::ALL {
            assert_eq!(ResizeKind::of(k.to_mode(ResizeMode::LongEdge(100))), k);
        }
    }

    #[test]
    fn the_preview_follows_the_template_and_reports_errors() {
        let spec = ExportSpec {
            naming: NamingSpec {
                template: "{Sequence:3}_{Filename}".into(),
                sequence_start: 9,
            },
            ..ExportSpec::default()
        };
        let names = preview_names(&spec, &[facts("a"), facts("b")]).unwrap();
        assert_eq!(names, ["009_a.jpg", "010_b.jpg"]);

        let bad = ExportSpec {
            naming: NamingSpec {
                template: "{Nope}".into(),
                sequence_start: 1,
            },
            ..ExportSpec::default()
        };
        assert!(preview_names(&bad, &[facts("a")]).is_err());
    }

    /// Runs the whole dialog for a few headless frames: it must draw without panicking for every
    /// format/resize/watermark/destination shape, and reflect validity in the Export action.
    #[test]
    fn the_dialog_draws_every_shape_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui_state = ExportUi::new(&dir.path().join("catalog.db"));
        let ctx = egui::Context::default();
        let shapes = [
            ExportSpec::default(),
            ExportSpec {
                format: FormatSpec::Png {
                    depth: BitDepth::Sixteen,
                },
                resize: nicti_preen::spec::ResizeSpec {
                    mode: ResizeMode::Fit {
                        width: 800,
                        height: 600,
                    },
                    dont_enlarge: false,
                },
                watermark: Some(WatermarkSpec::default()),
                destination: nicti_preen::spec::DestinationSpec {
                    base: DestinationBase::Folder(dir.path().to_path_buf()),
                    subfolder: Some("{Date:YYYY}".into()),
                },
                ..ExportSpec::default()
            },
            ExportSpec {
                format: FormatSpec::Tiff {
                    depth: BitDepth::Eight,
                    compression: TiffCompression::Lzw,
                },
                resize: nicti_preen::spec::ResizeSpec {
                    mode: ResizeMode::Megapixels(6.0),
                    dont_enlarge: true,
                },
                ..ExportSpec::default()
            },
        ];
        for spec in shapes {
            ui_state.request(vec![1, 2], "2 photos".into(), vec![facts("a"), facts("b")]);
            ui_state.dialog.as_mut().unwrap().spec = spec;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 900.0),
                )),
                ..Default::default()
            };
            for _ in 0..2 {
                let out = ctx.run_ui(input.clone(), |ui| {
                    ui_state.show_dialog(ui.ctx(), &|| None);
                    ui_state.show_status(ui);
                });
                out.drop_without_applying_deltas();
            }
            assert!(
                ui_state.dialog.is_some(),
                "drawing alone never closes the dialog"
            );
        }
    }

    #[test]
    fn requests_are_ignored_when_empty_or_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui_state = ExportUi::new(&dir.path().join("c.db"));
        ui_state.request(vec![], "nothing".into(), vec![]);
        assert!(ui_state.dialog.is_none());
        ui_state.request(vec![7], "one photo".into(), vec![facts("a")]);
        assert!(ui_state.dialog.is_some());
    }
}
