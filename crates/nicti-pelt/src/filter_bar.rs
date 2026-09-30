//! #242: the Library view's filter bar -- keyword, rating, flag, label, make/model, capture-date
//! range and filename controls that build a [`nicti_lair::Filter`] for the grid, inline facet
//! counts, and "save as smart collection" / "load smart collection".
//!
//! The query engine is #23's (`hunt`/`facets`/`set_smart_rule`); this file is only the widget
//! and the state it needs. Everything that touches the catalog runs on a short-lived worker
//! thread and is polled each frame, so a `GROUP BY` over a big narrowed set (facets) or a
//! `SELECT DISTINCT` over the whole asset table (makes/labels) never blocks the UI thread.
//!
//! Text fields (filename, dates) are applied on Enter / focus loss, not per keystroke: the
//! filename match is a plain `GLOB` scan (ADR-0067), and re-querying the grid on every character
//! would queue a full snapshot per key.

use std::sync::{mpsc, Arc};

use nicti_lair::{CatalogStore, Collection, CollectionKind, FacetCounts, Filter, Keyword};

/// The rating control's choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RatingChoice {
    #[default]
    Any,
    /// Nothing has rated it yet (`Filter::unrated`, #32) -- what is left after a culling pass.
    Unrated,
    /// Exactly `n` stars, `n` in `1..=5` (#32).
    Exactly(i64),
    /// `rating >= n`, `n` in `1..=5`.
    AtLeast(i64),
    Rejected,
    /// A loaded rule's rating shape the controls can't express (`include_unrated`, a `rating_max`,
    /// a 0-star floor...). The raw fields live in `FilterBar::custom_rating` and are applied
    /// untouched, so re-saving such a rule never widens or narrows it.
    Custom,
}

impl RatingChoice {
    fn from_filter(f: &Filter) -> Self {
        match (f.rating_min, f.rating_max, f.include_unrated, f.unrated) {
            (None, None, false, false) => Self::Any,
            (None, None, false, true) => Self::Unrated,
            (Some(-1), Some(-1), false, false) => Self::Rejected,
            (Some(n @ 1..=5), Some(m), false, false) if n == m => Self::Exactly(n),
            (Some(n @ 1..=5), None, false, false) => Self::AtLeast(n),
            _ => Self::Custom,
        }
    }

    fn apply(self, f: &mut Filter) {
        match self {
            Self::Any | Self::Custom => {}
            Self::Unrated => f.unrated = true,
            Self::Exactly(n) => {
                f.rating_min = Some(n);
                f.rating_max = Some(n);
            }
            Self::AtLeast(n) => f.rating_min = Some(n),
            Self::Rejected => {
                f.rating_min = Some(-1);
                f.rating_max = Some(-1);
            }
        }
    }
}

/// The flag control's choices (#32 added *Unflagged* to the original picks-only checkbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlagChoice {
    #[default]
    Any,
    Picked,
    /// No flag at all (`Filter::unflagged`).
    Unflagged,
    /// A loaded rule's flag shape the control can't express (`flag: Some(2)`, or a flag together
    /// with `unflagged`). The raw fields live in `FilterBar::custom_flag` and are applied
    /// untouched, so re-saving such a rule never widens or narrows it.
    Custom,
}

impl FlagChoice {
    fn from_filter(f: &Filter) -> Self {
        match (f.flag, f.unflagged) {
            (None, false) => Self::Any,
            (Some(1), false) => Self::Picked,
            (None, true) => Self::Unflagged,
            _ => Self::Custom,
        }
    }
}

/// The label control's choices (#32 added *No label* to the original any-or-a-name dropdown).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LabelChoice {
    #[default]
    Any,
    /// No colour label at all (`Filter::no_label`).
    None,
    Is(String),
    /// A label together with `no_label` (matches nothing): kept verbatim in
    /// `FilterBar::custom_label`, like `RatingChoice::Custom`.
    Custom,
}

impl LabelChoice {
    fn from_filter(f: &Filter) -> Self {
        match (&f.label, f.no_label) {
            (Some(l), false) => Self::Is(l.clone()),
            (None, true) => Self::None,
            (None, false) => Self::Any,
            (Some(_), true) => Self::Custom,
        }
    }
}

/// `YYYY-MM-DD` -> `(year, month, day)`, `None` for anything that isn't a real calendar date.
fn parse_date(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (u32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days).contains(&d).then_some((y, m, d))
}

/// `captured_at` text as ingest really stores it (`2026-01-31 13:05:09`: kamadak-exif's
/// `display_value` for `DateTimeOriginal`, NOT the raw `2026:01:31` EXIF form) -- what `Filter`'s
/// lexicographic bounds compare against. Mixing the two forms mis-sorts at the year's fourth
/// character (`-` < `:`), silently deciding every same-year comparison. `end_of_day` picks the inclusive upper
/// bound of the day rather than its start.
fn exif_bound((y, m, d): (u32, u32, u32), end_of_day: bool) -> String {
    let time = if end_of_day { "23:59:59" } else { "00:00:00" };
    format!("{y:04}-{m:02}-{d:02} {time}")
}

/// The inverse for showing a loaded rule's bound in the date box: the date part only.
fn date_from_exif(s: &str) -> String {
    s.get(..10)
        .map_or_else(String::new, |d| d.replace(':', "-"))
}

/// A text box whose value only counts once committed (Enter / focus lost).
fn commit_text(ui: &mut egui::Ui, draft: &mut String, committed: &mut String, width: f32) {
    let resp = ui.add(egui::TextEdit::singleline(draft).desired_width(width));
    if resp.lost_focus() && *draft != *committed {
        committed.clone_from(draft);
    }
}

/// Facet counts for the bar's dropdowns. Each dimension is counted with *its own* filter cleared
/// (the usual faceted-search rule), so picking a model doesn't zero out every other model in the
/// list -- the counts say "how many you'd get by switching to this one".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FacetView {
    /// Matches of the full current filter.
    pub total: u64,
    pub by_model: Vec<(Option<String>, u64)>,
    pub by_rating: Vec<(Option<i64>, u64)>,
    pub picks: u64,
    /// Photos with no flag (#32), counted like `picks`.
    pub unflagged: u64,
}

impl FacetView {
    fn model_count(&self, model: &str) -> u64 {
        self.by_model
            .iter()
            .find(|(m, _)| m.as_deref() == Some(model))
            .map_or(0, |(_, n)| *n)
    }

    fn at_least(&self, n: i64) -> u64 {
        self.by_rating
            .iter()
            .filter(|(r, _)| r.is_some_and(|r| r >= n))
            .map(|(_, c)| c)
            .sum()
    }

    fn unrated(&self) -> u64 {
        self.by_rating
            .iter()
            .filter(|(r, _)| r.is_none())
            .map(|(_, c)| c)
            .sum()
    }

    fn exactly(&self, n: i64) -> u64 {
        self.by_rating
            .iter()
            .filter(|(r, _)| *r == Some(n))
            .map(|(_, c)| c)
            .sum()
    }

    fn rejected(&self) -> u64 {
        self.by_rating
            .iter()
            .filter(|(r, _)| *r == Some(-1))
            .map(|(_, c)| c)
            .sum()
    }

    fn any(&self) -> u64 {
        self.by_rating.iter().map(|(_, c)| c).sum()
    }
}

/// Runs the (up to four) facet queries for `filter`, skipping any whose dimension-cleared filter
/// equals one already run, so an untouched bar costs one `facets` call, not four. (Even an
/// unfiltered `facets` still runs a live `by_flag` scan -- only model/rating come from ADR-0103's
/// cache -- hence the memo, and the caller running this on a worker thread.)
pub fn compute_facets(
    store: &dyn CatalogStore,
    filter: &Filter,
) -> Result<FacetView, nicti_lair::CatalogError> {
    let mut memo: Vec<(Filter, FacetCounts)> = Vec::new();
    let mut run = |f: Filter| -> Result<FacetCounts, nicti_lair::CatalogError> {
        if let Some((_, c)) = memo.iter().find(|(k, _)| *k == f) {
            return Ok(c.clone());
        }
        let c = store.facets(&f)?;
        memo.push((f, c.clone()));
        Ok(c)
    };
    let total = run(filter.clone())?.total;
    let by_model = run(Filter {
        model: None,
        ..filter.clone()
    })?
    .by_model;
    // Each dimension is counted with its OWN constraint cleared -- including the `unrated` /
    // `unflagged` fields #32 added, or a chosen "Unrated" would zero every other rating's count.
    let by_rating = run(Filter {
        rating_min: None,
        rating_max: None,
        include_unrated: false,
        unrated: false,
        ..filter.clone()
    })?
    .by_rating;
    let by_flag = run(Filter {
        flag: None,
        unflagged: false,
        ..filter.clone()
    })?
    .by_flag;
    let count_flag = |wanted: Option<i64>| {
        by_flag
            .iter()
            .filter(|(f, _)| *f == wanted)
            .map(|(_, c)| c)
            .sum()
    };
    Ok(FacetView {
        total,
        by_model,
        by_rating,
        picks: count_flag(Some(1)),
        unflagged: count_flag(None),
    })
}

/// The dropdowns' option lists, read off the UI thread.
#[derive(Debug, Clone, Default)]
struct Options {
    /// Depth-first, siblings name-sorted: `(depth, keyword)`.
    keywords: Vec<(usize, Keyword)>,
    makes: Vec<String>,
    labels: Vec<String>,
    smart_collections: Vec<Collection>,
}

/// Flattens `list_keywords`' name-sorted rows into a depth-first `(depth, keyword)` list. An
/// orphan whose parent is missing is treated as top-level rather than dropped.
fn keyword_tree(all: Vec<Keyword>) -> Vec<(usize, Keyword)> {
    let ids: std::collections::HashSet<i64> = all.iter().map(|k| k.id).collect();
    let is_root = |k: &Keyword| k.parent_id.is_none_or(|p| !ids.contains(&p));
    let mut out = Vec::with_capacity(all.len());
    fn walk(
        all: &[Keyword],
        parent: Option<i64>,
        depth: usize,
        is_root: &dyn Fn(&Keyword) -> bool,
        out: &mut Vec<(usize, Keyword)>,
    ) {
        for k in all {
            let child = match parent {
                None => is_root(k),
                Some(p) => k.parent_id == Some(p),
            };
            if child {
                out.push((depth, k.clone()));
                walk(all, Some(k.id), depth + 1, is_root, out);
            }
        }
    }
    walk(&all, None, 0, &is_root, &mut out);
    out
}

fn load_options(store: &dyn CatalogStore) -> Result<Options, nicti_lair::CatalogError> {
    Ok(Options {
        keywords: keyword_tree(store.list_keywords()?),
        makes: store.distinct_makes()?,
        labels: store.distinct_labels()?,
        smart_collections: store
            .list_collections()?
            .into_iter()
            .filter(|c| c.kind == CollectionKind::Smart)
            .collect(),
    })
}

/// A result being computed on a worker thread. Replacing the `Pending` drops the receiver, so a
/// superseded worker's answer is discarded when it finishes.
struct Pending<T>(mpsc::Receiver<T>);

impl<T: Send + 'static> Pending<T> {
    fn spawn(ctx: &egui::Context, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint();
        });
        Self(rx)
    }

    /// `Ok(Some)` = ready, `Ok(None)` = still running, `Err` = the worker died without a result.
    fn poll(&self) -> Result<Option<T>, mpsc::TryRecvError> {
        match self.0.try_recv() {
            Ok(v) => Ok(Some(v)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

type DynStore = Arc<dyn CatalogStore + Send + Sync>;

#[derive(Default)]
pub struct FilterBar {
    keyword_id: Option<i64>,
    include_subtree: bool,
    rating: RatingChoice,
    /// `(rating_min, rating_max, include_unrated, unrated)` of a loaded `RatingChoice::Custom` rule.
    custom_rating: (Option<i64>, Option<i64>, bool, bool),
    flag: FlagChoice,
    /// `(flag, unflagged)` of a loaded `FlagChoice::Custom` rule.
    custom_flag: (Option<i64>, bool),
    label: LabelChoice,
    /// `(label, no_label)` of a loaded `LabelChoice::Custom` rule.
    custom_label: (Option<String>, bool),
    make: Option<String>,
    model: Option<String>,

    filename: String,
    filename_draft: String,
    from: String,
    from_draft: String,
    to: String,
    to_draft: String,

    /// A loaded rule's fields the bar has no control for (`rel_path_prefix`), and its raw date
    /// bounds -- both carried through untouched so loading then re-saving a rule never silently
    /// rewrites it. `raw_*` are only honoured while the matching date box still shows what the
    /// rule showed.
    rel_path_prefix: Option<String>,
    raw_after: Option<(String, String)>,
    raw_before: Option<(String, String)>,

    save_name: String,
    status: Option<String>,

    options: Option<Options>,
    options_job: Option<Pending<Result<Options, String>>>,
    facets: Option<FacetView>,
    facets_for: Option<Filter>,
    facets_job: Option<Pending<Result<FacetView, String>>>,
    /// The last failure of each background job, cleared when that same job next succeeds.
    /// Separate from `error`, which belongs to a user action (load/save).
    options_error: Option<String>,
    facets_error: Option<String>,
    error: Option<String>,
}

impl FilterBar {
    /// A bar with just the three marker controls set -- lets other modules' end-to-end tests drive
    /// the real controls-to-`Filter` mapping instead of hand-building a `Filter`.
    #[cfg(test)]
    pub fn with_markers(rating: RatingChoice, flag: FlagChoice, label: LabelChoice) -> Self {
        FilterBar {
            rating,
            flag,
            label,
            ..Default::default()
        }
    }

    /// The `Filter` the current controls describe, scoped to `root_id`.
    pub fn to_filter(&self, root_id: Option<i64>) -> Filter {
        let mut f = Filter {
            keyword_id: self.keyword_id,
            include_subtree: self.include_subtree,
            flag: (self.flag == FlagChoice::Picked).then_some(1),
            unflagged: self.flag == FlagChoice::Unflagged,
            label: match &self.label {
                LabelChoice::Is(l) => Some(l.clone()),
                _ => None,
            },
            no_label: self.label == LabelChoice::None,
            make: self.make.clone(),
            model: self.model.clone(),
            root_id,
            rel_path_prefix: self.rel_path_prefix.clone(),
            filename_contains: Some(self.filename.trim().to_string()).filter(|s| !s.is_empty()),
            ..Default::default()
        };
        if self.flag == FlagChoice::Custom {
            (f.flag, f.unflagged) = self.custom_flag;
        }
        if self.label == LabelChoice::Custom {
            (f.label, f.no_label) = self.custom_label.clone();
        }
        self.rating.apply(&mut f);
        if self.rating == RatingChoice::Custom {
            (f.rating_min, f.rating_max, f.include_unrated, f.unrated) = self.custom_rating;
        }
        f.captured_after = bound(&self.from, false, &self.raw_after);
        f.captured_before = bound(&self.to, true, &self.raw_before);
        f
    }

    /// Replaces every control with `f`'s values (loading a smart collection's rule).
    pub fn load_filter(&mut self, f: &Filter) {
        self.keyword_id = f.keyword_id;
        self.include_subtree = f.include_subtree;
        self.rating = RatingChoice::from_filter(f);
        self.custom_rating = (f.rating_min, f.rating_max, f.include_unrated, f.unrated);
        self.flag = FlagChoice::from_filter(f);
        self.custom_flag = (f.flag, f.unflagged);
        self.label = LabelChoice::from_filter(f);
        self.custom_label = (f.label.clone(), f.no_label);
        self.make.clone_from(&f.make);
        self.model.clone_from(&f.model);
        self.filename = f.filename_contains.clone().unwrap_or_default();
        self.filename_draft.clone_from(&self.filename);
        self.rel_path_prefix.clone_from(&f.rel_path_prefix);
        (self.from, self.raw_after) = load_bound(&f.captured_after);
        (self.to, self.raw_before) = load_bound(&f.captured_before);
        self.from_draft.clone_from(&self.from);
        self.to_draft.clone_from(&self.to);
    }

    pub fn clear(&mut self) {
        let options = self.options.take();
        let options_job = self.options_job.take();
        *self = Self {
            options,
            options_job,
            ..Default::default()
        };
    }

    /// Forces the facet counts to be recomputed for the current filter. Marking photos changes
    /// what every count says ("Unrated (N)", "Picked (k)") without changing the filter, so the
    /// culling code calls this once marking goes quiet.
    pub fn invalidate_facets(&mut self) {
        self.facets_for = None;
    }

    /// Forces the dropdown option lists to be re-read (after an import or sync may have added
    /// keywords/makes/labels).
    pub fn invalidate_options(&mut self) {
        self.options = None;
        self.options_job = None;
        self.facets_for = None;
    }

    /// Whether any control is narrowing the grid (the folder selection is the toolbar's, not
    /// counted here).
    pub fn is_active(&self) -> bool {
        self.to_filter(None) != Filter::default()
    }

    fn date_error(text: &str) -> bool {
        !text.trim().is_empty() && parse_date(text).is_none()
    }

    /// Draws the bar. `root_id` is the Library toolbar's folder selection; loading a smart
    /// collection may change it. The grid's filter is then `to_filter(*root_id)`.
    pub fn show(&mut self, ui: &mut egui::Ui, store: &DynStore, root_id: &mut Option<i64>) {
        self.poll(ui.ctx());
        if self.options.is_none() && self.options_job.is_none() {
            let s = store.clone();
            self.options_job = Some(Pending::spawn(ui.ctx(), move || {
                load_options(s.as_ref()).map_err(|e| e.to_string())
            }));
        }

        let facets = self.facets.clone().unwrap_or_default();
        let have_facets = self.facets.is_some();
        let options = self.options.clone().unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            // Keyword (+ subtree).
            let kw_label = match self.keyword_id {
                None => "Any keyword".to_string(),
                Some(id) => options
                    .keywords
                    .iter()
                    .find(|(_, k)| k.id == id)
                    .map(|(_, k)| k.name.clone())
                    // Never show "Any" while a keyword filter is live (options still loading, or
                    // the rule's keyword was deleted -- which matches nothing).
                    .unwrap_or_else(|| {
                        if self.options.is_some() {
                            format!("Missing keyword (#{id})")
                        } else {
                            "Loading…".to_string()
                        }
                    }),
            };
            egui::ComboBox::from_id_salt("fb_keyword")
                .selected_text(kw_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.keyword_id, None, "Any keyword");
                    for (depth, k) in &options.keywords {
                        let text = format!("{}{}", "    ".repeat(*depth), k.name);
                        ui.selectable_value(&mut self.keyword_id, Some(k.id), text);
                    }
                });
            if self.keyword_id.is_some() {
                ui.checkbox(&mut self.include_subtree, "+ sub-keywords");
            }

            // Rating.
            let count = |n: u64| {
                if have_facets {
                    format!(" ({n})")
                } else {
                    String::new()
                }
            };
            let rating_text = match self.rating {
                RatingChoice::Any => "Any rating".to_string(),
                RatingChoice::Unrated => "Unrated".to_string(),
                RatingChoice::Exactly(n) => format!("{n} star{}", if n == 1 { "" } else { "s" }),
                RatingChoice::AtLeast(n) => format!("{n}+ stars"),
                RatingChoice::Rejected => "Rejected".to_string(),
                RatingChoice::Custom => "Custom (saved rule)".to_string(),
            };
            egui::ComboBox::from_id_salt("fb_rating")
                .selected_text(rating_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.rating,
                        RatingChoice::Any,
                        format!("Any rating{}", count(facets.any())),
                    );
                    ui.selectable_value(
                        &mut self.rating,
                        RatingChoice::Unrated,
                        format!("Unrated{}", count(facets.unrated())),
                    );
                    for n in 1..=5 {
                        ui.selectable_value(
                            &mut self.rating,
                            RatingChoice::Exactly(n),
                            format!(
                                "{n} star{} exactly{}",
                                if n == 1 { "" } else { "s" },
                                count(facets.exactly(n))
                            ),
                        );
                    }
                    for n in 1..=5 {
                        ui.selectable_value(
                            &mut self.rating,
                            RatingChoice::AtLeast(n),
                            format!("{n}+ stars{}", count(facets.at_least(n))),
                        );
                    }
                    ui.selectable_value(
                        &mut self.rating,
                        RatingChoice::Rejected,
                        format!("Rejected{}", count(facets.rejected())),
                    );
                });

            let flag_text = match self.flag {
                FlagChoice::Any => "Any flag",
                FlagChoice::Picked => "Picked",
                FlagChoice::Unflagged => "Unflagged",
                FlagChoice::Custom => "Custom (saved rule)",
            };
            egui::ComboBox::from_id_salt("fb_flag")
                .selected_text(flag_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.flag, FlagChoice::Any, "Any flag");
                    ui.selectable_value(
                        &mut self.flag,
                        FlagChoice::Picked,
                        format!("Picked{}", count(facets.picks)),
                    );
                    ui.selectable_value(
                        &mut self.flag,
                        FlagChoice::Unflagged,
                        format!("Unflagged{}", count(facets.unflagged)),
                    );
                });

            let label_text = match &self.label {
                LabelChoice::Any => "Any label".to_string(),
                LabelChoice::None => "No label".to_string(),
                LabelChoice::Is(l) => l.clone(),
                LabelChoice::Custom => "Custom (saved rule)".to_string(),
            };
            egui::ComboBox::from_id_salt("fb_label")
                .selected_text(label_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.label, LabelChoice::Any, "Any label");
                    ui.selectable_value(&mut self.label, LabelChoice::None, "No label");
                    // A loaded rule may name a label that isn't in the (online) list; keep it
                    // selectable.
                    if let LabelChoice::Is(l) = &self.label {
                        if !options.labels.contains(l) {
                            let l = l.clone();
                            ui.selectable_value(&mut self.label, LabelChoice::Is(l.clone()), l);
                        }
                    }
                    for l in &options.labels {
                        ui.selectable_value(&mut self.label, LabelChoice::Is(l.clone()), l);
                    }
                });
            string_combo(
                ui,
                "fb_make",
                "Any make",
                &mut self.make,
                &options.makes,
                |_| String::new(),
            );
            let models: Vec<String> = facets
                .by_model
                .iter()
                .filter_map(|(m, _)| m.clone())
                .collect();
            string_combo(ui, "fb_model", "Any model", &mut self.model, &models, |m| {
                if have_facets {
                    format!(" ({})", facets.model_count(m))
                } else {
                    String::new()
                }
            });
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Captured");
            date_box(ui, &mut self.from_draft, &mut self.from, "from");
            ui.label("to");
            date_box(ui, &mut self.to_draft, &mut self.to, "to");
            ui.label("Filename");
            commit_text(ui, &mut self.filename_draft, &mut self.filename, 140.0);
            if ui.button("Clear filters").clicked() {
                self.clear();
            }
        });

        // Facet totals for the current filter, plus save/load of smart collections.
        let mut load_id: Option<i64> = None;
        let mut save = false;
        ui.horizontal_wrapped(|ui| {
            if have_facets {
                ui.weak(format!("{} match", facets.total));
            }
            if self.facets_job.is_some() {
                ui.spinner();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.save_name)
                    .hint_text("Smart collection name")
                    .desired_width(160.0),
            );
            let can_save = !self.save_name.trim().is_empty();
            if ui
                .add_enabled(can_save, egui::Button::new("Save as smart collection"))
                .clicked()
            {
                save = true;
            }
            egui::ComboBox::from_id_salt("fb_load_smart")
                .selected_text("Load smart collection")
                .show_ui(ui, |ui| {
                    if options.smart_collections.is_empty() {
                        ui.weak("None saved yet");
                    }
                    for c in &options.smart_collections {
                        if ui.selectable_label(false, &c.name).clicked() {
                            load_id = Some(c.id);
                        }
                    }
                });
            if ui
                .small_button("↻")
                .on_hover_text("Reload option lists")
                .clicked()
            {
                self.invalidate_options();
            }
        });
        if let Some(status) = &self.status {
            ui.weak(status);
        }
        for err in self
            .options_error
            .iter()
            .chain(&self.facets_error)
            .chain(&self.error)
        {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }

        if let Some(id) = load_id {
            self.load_collection(store, id, root_id);
        }
        let filter = self.to_filter(*root_id);
        if save {
            self.save_smart_collection(ui.ctx(), store, &filter);
        }
        if self.facets_for.as_ref() != Some(&filter) {
            self.facets_for = Some(filter.clone());
            let (s, f) = (store.clone(), filter.clone());
            self.facets_job = Some(Pending::spawn(ui.ctx(), move || {
                compute_facets(s.as_ref(), &f).map_err(|e| e.to_string())
            }));
        }
    }

    fn poll(&mut self, _ctx: &egui::Context) {
        if let Some(job) = &self.options_job {
            let outcome = match job.poll() {
                Ok(None) => None,
                Ok(Some(r)) => Some(r),
                Err(_) => Some(Err("the option loader stopped unexpectedly".to_string())),
            };
            if let Some(outcome) = outcome {
                self.options_job = None;
                match outcome {
                    Ok(o) => {
                        self.options_error = None;
                        self.options = Some(o);
                    }
                    Err(e) => {
                        self.options_error = Some(format!("Couldn't read filter options: {e}"));
                        self.options = Some(Options::default());
                    }
                }
            }
        }
        if let Some(job) = &self.facets_job {
            let outcome = match job.poll() {
                Ok(None) => None,
                Ok(Some(r)) => Some(r),
                Err(_) => Some(Err("the counter stopped unexpectedly".to_string())),
            };
            if let Some(outcome) = outcome {
                self.facets_job = None;
                match outcome {
                    Ok(f) => {
                        self.facets_error = None;
                        self.facets = Some(f);
                    }
                    Err(e) => {
                        // Never leave the previous filter's counts on screen against the new one.
                        self.facets = None;
                        self.facets_error = Some(format!("Couldn't count matches: {e}"));
                    }
                }
            }
        }
    }

    fn load_collection(&mut self, store: &DynStore, id: i64, root_id: &mut Option<i64>) {
        match store.collection_filter(id) {
            Ok(Some(f)) => {
                self.load_filter(&f);
                *root_id = f.root_id;
                self.status = None;
                self.error = None;
            }
            Ok(None) => self.error = Some("That smart collection has no rule saved.".into()),
            Err(e) => self.error = Some(format!("Couldn't load the smart collection: {e}")),
        }
    }

    /// Creates the collection then stores the rule; if the second step fails the empty
    /// collection is removed again rather than left behind as a rule-less orphan.
    fn save_smart_collection(&mut self, ctx: &egui::Context, store: &DynStore, filter: &Filter) {
        let name = self.save_name.trim().to_string();
        if Self::date_error(&self.from_draft) || Self::date_error(&self.to_draft) {
            self.error = Some("Fix the date range (YYYY-MM-DD) before saving.".into());
            return;
        }
        let saved = store
            .create_collection(None, &name, CollectionKind::Smart)
            .and_then(|id| {
                store.set_smart_rule(id, filter).inspect_err(|_| {
                    let _ = store.delete_collection(id);
                })
            });
        match saved {
            Ok(()) => {
                self.status = Some(format!("Saved smart collection “{name}”."));
                self.error = None;
                self.save_name.clear();
                self.options = None; // re-read so it shows up in "Load smart collection"
                self.options_job = None;
                ctx.request_repaint();
            }
            Err(e) => self.error = Some(format!("Couldn't save the smart collection: {e}")),
        }
    }
}

/// One committed date box, tinted red while its text isn't a real `YYYY-MM-DD` date. An
/// invalid date filters nothing (see [`bound`]) rather than silently matching nothing.
fn date_box(ui: &mut egui::Ui, draft: &mut String, committed: &mut String, hint: &str) {
    let bad = FilterBar::date_error(draft);
    let resp = ui.add(
        egui::TextEdit::singleline(draft)
            .hint_text(format!("{hint} YYYY-MM-DD"))
            .desired_width(110.0)
            .text_color_opt(bad.then(|| ui.visuals().error_fg_color)),
    );
    if resp.lost_focus() && *draft != *committed {
        committed.clone_from(draft);
    }
}

/// A dropdown over plain strings with an "any" entry. `suffix` renders the inline count.
fn string_combo(
    ui: &mut egui::Ui,
    id: &str,
    any_text: &str,
    value: &mut Option<String>,
    choices: &[String],
    suffix: impl Fn(&str) -> String,
) {
    let shown = value
        .as_deref()
        .map_or_else(|| any_text.to_string(), |v| format!("{v}{}", suffix(v)));
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, any_text);
            // A loaded rule may name a value that isn't in the (online) list; keep it selectable.
            if let Some(v) = value.clone().filter(|v| !choices.contains(v)) {
                ui.selectable_value(value, Some(v.clone()), format!("{v}{}", suffix(&v)));
            }
            for c in choices {
                ui.selectable_value(value, Some(c.clone()), format!("{c}{}", suffix(c)));
            }
        });
}

/// A date box's bound: the loaded rule's raw text while the box still shows that rule's date,
/// else the box's own date converted to EXIF form; an empty or invalid box is no bound.
fn bound(text: &str, end_of_day: bool, raw: &Option<(String, String)>) -> Option<String> {
    if let Some((shown, raw)) = raw {
        if shown == text {
            return Some(raw.clone());
        }
    }
    parse_date(text).map(|d| exif_bound(d, end_of_day))
}

/// Rewrites a raw-EXIF colon-form bound (`2026:01:05 00:00:00`, the shape `Filter`'s own early
/// tests used) into the dashed form ingest stores; anything else passes through verbatim.
fn normalize_bound(raw: &str) -> String {
    let b = raw.as_bytes();
    let colon_date = b.len() >= 10
        && b[4] == b':'
        && b[7] == b':'
        && b[..4]
            .iter()
            .chain(&b[5..7])
            .chain(&b[8..10])
            .all(u8::is_ascii_digit);
    if colon_date {
        format!("{}{}", raw[..10].replace(':', "-"), &raw[10..])
    } else {
        raw.to_string()
    }
}

fn load_bound(raw: &Option<String>) -> (String, Option<(String, String)>) {
    match raw {
        Some(r) => {
            let raw = normalize_bound(r);
            let shown = date_from_exif(&raw);
            (shown.clone(), Some((shown, raw)))
        }
        None => (String::new(), None),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // builder-style setup reads clearer here
mod tests {
    use super::*;
    use nicti_lair::{NewAsset, SqliteCatalog};

    #[test]
    fn parse_date_accepts_real_dates_only() {
        assert_eq!(parse_date("2026-01-31"), Some((2026, 1, 31)));
        assert_eq!(parse_date(" 2024-02-29 "), Some((2024, 2, 29)));
        for bad in [
            "",
            "2026-1-31",
            "2026-02-29",
            "2100-02-29",
            "2026-13-01",
            "2026-04-31",
            "2026-00-10",
            "26-01-01",
            "2026/01/01",
            "2026-01-01-01",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn date_bounds_use_exif_format_and_cover_the_whole_day() {
        let mut bar = FilterBar::default();
        bar.from = "2026-01-05".into();
        bar.to = "2026-01-06".into();
        let f = bar.to_filter(None);
        assert_eq!(f.captured_after.as_deref(), Some("2026-01-05 00:00:00"));
        assert_eq!(f.captured_before.as_deref(), Some("2026-01-06 23:59:59"));
    }

    /// Regression: bounds were once built in raw-EXIF colon form while ingest stores the dashed
    /// `display_value` form, so same-year comparisons were decided by `-` vs `:` alone.
    #[test]
    fn date_range_matches_captured_at_as_ingest_stores_it() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v = store.upsert_volume("v", None, None, 0).unwrap();
        let r = store.ensure_root(v, "").unwrap();
        let mut ids = Vec::new();
        for (name, at) in [
            ("jan4", "2026-01-04 23:59:59"),
            ("jan5", "2026-01-05 00:00:00"),
            ("jan6", "2026-01-06 23:59:59"),
            ("jan7", "2026-01-07 00:00:00"),
            ("mar", "2026-03-01 10:00:00"),
        ] {
            let mut a = asset(&format!("{name}.NEF"), "Z8");
            a.captured_at = Some(at.into());
            ids.push(store.insert_asset(r, &a, None).unwrap());
        }
        let mut bar = FilterBar::default();
        bar.from = "2026-01-05".into();
        bar.to = "2026-01-06".into();
        let sort = nicti_lair::Sort {
            field: nicti_lair::SortField::Captured,
            direction: nicti_lair::SortDirection::Asc,
        };
        let got = store.hunt_ids(&bar.to_filter(None), sort).unwrap();
        assert_eq!(got, vec![ids[1], ids[2]]);
    }

    #[test]
    fn an_invalid_date_is_no_bound_not_an_empty_result() {
        let mut bar = FilterBar::default();
        bar.from = "2026-02-30".into();
        assert_eq!(bar.to_filter(None).captured_after, None);
    }

    #[test]
    fn default_bar_yields_the_default_filter_and_carries_the_root() {
        let mut bar = FilterBar::default();
        assert_eq!(bar.to_filter(None), Filter::default());
        assert_eq!(bar.to_filter(Some(7)).root_id, Some(7));
        assert!(!bar.is_active());
        bar.flag = FlagChoice::Picked;
        assert!(bar.is_active());
    }

    #[test]
    fn every_control_maps_onto_its_filter_field() {
        let mut bar = FilterBar::default();
        bar.keyword_id = Some(3);
        bar.include_subtree = true;
        bar.rating = RatingChoice::AtLeast(4);
        bar.flag = FlagChoice::Picked;
        bar.label = LabelChoice::Is("Red".into());
        bar.make = Some("NIKON".into());
        bar.model = Some("Z8".into());
        bar.filename = "  dsc_ ".into();
        let f = bar.to_filter(Some(2));
        assert_eq!(f.keyword_id, Some(3));
        assert!(f.include_subtree);
        assert_eq!((f.rating_min, f.rating_max), (Some(4), None));
        assert_eq!(f.flag, Some(1));
        assert_eq!(f.label.as_deref(), Some("Red"));
        assert_eq!(f.make.as_deref(), Some("NIKON"));
        assert_eq!(f.model.as_deref(), Some("Z8"));
        assert_eq!(f.filename_contains.as_deref(), Some("dsc_"));
        assert_eq!(f.root_id, Some(2));
        let mut rejected = FilterBar::default();
        rejected.rating = RatingChoice::Rejected;
        let f = rejected.to_filter(None);
        assert_eq!((f.rating_min, f.rating_max), (Some(-1), Some(-1)));
    }

    #[test]
    fn loading_then_rebuilding_a_rule_is_lossless() {
        let rule = Filter {
            keyword_id: Some(9),
            include_subtree: true,
            rating_min: Some(3),
            flag: Some(1),
            label: Some("Blue".into()),
            make: Some("NIKON".into()),
            model: Some("Z6".into()),
            // Not on a day boundary, and a prefix the bar has no control for.
            captured_after: Some("2026-01-01 08:30:00".into()),
            captured_before: Some("2026-02-01 00:00:00".into()),
            root_id: Some(4),
            rel_path_prefix: Some("2026/trip".into()),
            filename_contains: Some("dsc".into()),
            ..Default::default()
        };
        let mut bar = FilterBar::default();
        bar.load_filter(&rule);
        assert_eq!(bar.from, "2026-01-01");
        assert_eq!(bar.to_filter(rule.root_id), rule);
        // Editing the box drops the raw bound in favour of the typed day.
        bar.from = "2026-01-02".into();
        assert_eq!(
            bar.to_filter(rule.root_id).captured_after.as_deref(),
            Some("2026-01-02 00:00:00")
        );
    }

    #[test]
    fn legacy_colon_form_bounds_are_normalised_on_load() {
        let rule = Filter {
            captured_after: Some("2026:01:05 08:00:00".into()),
            captured_before: Some("not a date".into()),
            ..Default::default()
        };
        let mut bar = FilterBar::default();
        bar.load_filter(&rule);
        let f = bar.to_filter(None);
        assert_eq!(f.captured_after.as_deref(), Some("2026-01-05 08:00:00"));
        // An unrecognised bound is kept verbatim rather than guessed at.
        assert_eq!(f.captured_before.as_deref(), Some("not a date"));
        assert_eq!(
            normalize_bound("2026-01-05 08:00:00"),
            "2026-01-05 08:00:00"
        );
    }

    #[test]
    fn switching_away_from_a_custom_rating_and_clearing_leave_no_trace() {
        let mut bar = FilterBar::default();
        bar.load_filter(&Filter {
            rating_min: Some(3),
            include_unrated: true,
            ..Default::default()
        });
        bar.rating = RatingChoice::Any;
        assert_eq!(bar.to_filter(None), Filter::default());
        bar.rating = RatingChoice::Custom;
        bar.clear();
        assert_eq!(bar.to_filter(None), Filter::default());
    }

    fn wait_for(bar: &mut FilterBar, ctx: &egui::Context, done: impl Fn(&FilterBar) -> bool) {
        for _ in 0..400 {
            bar.poll(ctx);
            if done(bar) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("background job never finished");
    }

    #[test]
    fn a_failed_facets_job_drops_stale_counts_and_only_its_own_success_clears_it() {
        let ctx = egui::Context::default();
        let mut bar = FilterBar::default();
        bar.facets = Some(FacetView {
            total: 9,
            ..Default::default()
        });
        bar.facets_job = Some(Pending::spawn(&ctx, || Err("boom".to_string())));
        wait_for(&mut bar, &ctx, |b| b.facets_job.is_none());
        assert_eq!(bar.facets, None);
        assert!(bar.facets_error.as_deref().unwrap().contains("boom"));
        // A successful options load must not hide the facets failure.
        bar.options_job = Some(Pending::spawn(&ctx, || Ok(Options::default())));
        wait_for(&mut bar, &ctx, |b| b.options_job.is_none());
        assert!(bar.options.is_some());
        assert!(bar.facets_error.is_some());
        // A facets success does clear it.
        bar.facets_job = Some(Pending::spawn(&ctx, || Ok(FacetView::default())));
        wait_for(&mut bar, &ctx, |b| b.facets_job.is_none());
        assert_eq!(bar.facets_error, None);
    }

    #[test]
    fn a_rule_with_an_unsupported_rating_shape_is_kept_verbatim() {
        for (min, max, unrated) in [
            (Some(2), Some(4), false),
            (Some(3), None, true),
            (Some(0), None, false),
            (None, Some(2), false),
        ] {
            let rule = Filter {
                rating_min: min,
                rating_max: max,
                include_unrated: unrated,
                ..Default::default()
            };
            assert_eq!(RatingChoice::from_filter(&rule), RatingChoice::Custom);
            let mut bar = FilterBar::default();
            bar.load_filter(&rule);
            assert_eq!(bar.to_filter(None), rule);
            // Picking a real choice replaces the custom shape entirely.
            bar.rating = RatingChoice::AtLeast(4);
            let f = bar.to_filter(None);
            assert_eq!(
                (f.rating_min, f.rating_max, f.include_unrated),
                (Some(4), None, false)
            );
        }
    }

    #[test]
    fn clear_resets_the_controls_but_keeps_loaded_options() {
        let mut bar = FilterBar::default();
        bar.options = Some(Options {
            makes: vec!["NIKON".into()],
            ..Default::default()
        });
        bar.model = Some("Z8".into());
        bar.filename = "x".into();
        bar.clear();
        assert_eq!(bar.to_filter(None), Filter::default());
        assert_eq!(bar.options.as_ref().unwrap().makes, vec!["NIKON"]);
    }

    fn kw(id: i64, parent: Option<i64>, name: &str) -> Keyword {
        Keyword {
            id,
            parent_id: parent,
            name: name.into(),
            path: String::new(),
        }
    }

    #[test]
    fn keyword_tree_is_depth_first_and_keeps_orphans() {
        // Input is name-sorted, as `list_keywords` returns it.
        let tree = keyword_tree(vec![
            kw(1, None, "Animals"),
            kw(3, Some(1), "Birds"),
            kw(2, Some(1), "Cats"),
            kw(4, Some(2), "Tabby"),
            kw(9, Some(99), "Orphan"),
        ]);
        let shape: Vec<_> = tree.iter().map(|(d, k)| (*d, k.id)).collect();
        assert_eq!(shape, vec![(0, 1), (1, 3), (1, 2), (2, 4), (0, 9)]);
    }

    fn asset(path: &str, model: &str) -> NewAsset {
        NewAsset {
            rel_path: path.into(),
            rel_path_fold: path.to_lowercase(),
            size_bytes: 1,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: Some(model.into()),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    #[test]
    fn facets_count_each_dimension_with_its_own_filter_cleared() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v = store.upsert_volume("v", None, None, 0).unwrap();
        let r = store.ensure_root(v, "").unwrap();
        let z8a = store.insert_asset(r, &asset("a.NEF", "Z8"), None).unwrap();
        let z8b = store.insert_asset(r, &asset("b.NEF", "Z8"), None).unwrap();
        let z6 = store.insert_asset(r, &asset("c.NEF", "Z6"), None).unwrap();
        store.set_rating(&[z8a, z6], Some(5)).unwrap();
        store.set_rating(&[z8b], Some(1)).unwrap();
        store.set_flag(&[z8a], Some(1)).unwrap();

        let filter = Filter {
            model: Some("Z8".into()),
            rating_min: Some(4),
            flag: Some(1),
            ..Default::default()
        };
        let f = compute_facets(&store, &filter).unwrap();
        // Full filter: only the 5-star, picked Z8.
        assert_eq!(f.total, 1);
        // Model counts ignore the model filter but keep rating + flag: only the picked 5-star Z8
        // qualifies, so switching to Z6 (unpicked) would give 0.
        assert_eq!(f.model_count("Z8"), 1);
        assert_eq!(f.model_count("Z6"), 0);
        // Rating counts ignore the rating filter but keep model = Z8 + flag: only `z8a`.
        assert_eq!(f.at_least(1), 1);
        assert_eq!(f.at_least(5), 1);
        assert_eq!(f.any(), 1);
        // Picks ignore the flag filter but keep model + rating: one Z8 pick within 4+ stars. If the
        // flag weren't cleared this would still be 1, so also check the dimension with no picks:
        // the unpicked 5-star Z6 must not be counted, the picked Z8 must.
        assert_eq!(f.picks, 1);
        let z6_only = Filter {
            model: Some("Z6".into()),
            flag: Some(1),
            ..Default::default()
        };
        let g = compute_facets(&store, &z6_only).unwrap();
        assert_eq!(g.total, 0);
        // Flag cleared for the picks count: Z6 has no picks either way -> 0; but rating counts
        // (rating cleared, flag kept) are 0 while model counts (model cleared, flag kept) show Z8.
        assert_eq!(g.picks, 0);
        assert_eq!(g.model_count("Z8"), 1);
    }

    #[test]
    fn facets_of_the_empty_filter_cover_every_asset() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v = store.upsert_volume("v", None, None, 0).unwrap();
        let r = store.ensure_root(v, "").unwrap();
        store.insert_asset(r, &asset("a.NEF", "Z8"), None).unwrap();
        store.insert_asset(r, &asset("b.NEF", "Z6"), None).unwrap();
        let f = compute_facets(&store, &Filter::default()).unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.by_model.len(), 2);
    }

    #[test]
    fn saving_stores_a_smart_collection_whose_rule_round_trips() {
        let store: DynStore = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let mut bar = FilterBar::default();
        bar.model = Some("Z8".into());
        bar.rating = RatingChoice::AtLeast(3);
        bar.from = "2026-01-01".into();
        bar.save_name = "Good Z8".into();
        let filter = bar.to_filter(None);
        bar.save_smart_collection(&egui::Context::default(), &store, &filter);
        assert_eq!(bar.error, None);

        let smart = load_options(store.as_ref()).unwrap().smart_collections;
        assert_eq!(smart.len(), 1);
        assert_eq!(smart[0].name, "Good Z8");

        let mut loaded = FilterBar::default();
        let mut root = Some(99);
        loaded.load_collection(&store, smart[0].id, &mut root);
        assert_eq!(root, None);
        assert_eq!(loaded.to_filter(root), filter);
    }

    #[test]
    fn saving_with_a_bad_date_is_refused_and_creates_nothing() {
        let store: DynStore = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let mut bar = FilterBar::default();
        bar.from_draft = "2026-99-99".into();
        bar.save_name = "x".into();
        let filter = bar.to_filter(None);
        bar.save_smart_collection(&egui::Context::default(), &store, &filter);
        assert!(bar.error.is_some());
        assert!(store.list_collections().unwrap().is_empty());
    }

    // ---- #32: unrated / exactly-N / unflagged / no-label ------------------------------------

    fn marker_filter(r: RatingChoice, f: FlagChoice, l: LabelChoice) -> Filter {
        FilterBar::with_markers(r, f, l).to_filter(None)
    }

    #[test]
    fn the_culling_choices_map_onto_their_filter_fields() {
        let unrated = marker_filter(RatingChoice::Unrated, FlagChoice::Any, LabelChoice::Any);
        assert!(unrated.unrated);
        assert_eq!((unrated.rating_min, unrated.rating_max), (None, None));

        let two = marker_filter(RatingChoice::Exactly(2), FlagChoice::Any, LabelChoice::Any);
        assert_eq!((two.rating_min, two.rating_max), (Some(2), Some(2)));
        assert!(!two.unrated);

        let unflagged = marker_filter(RatingChoice::Any, FlagChoice::Unflagged, LabelChoice::Any);
        assert!(unflagged.unflagged && unflagged.flag.is_none());
        let picked = marker_filter(RatingChoice::Any, FlagChoice::Picked, LabelChoice::Any);
        assert!(!picked.unflagged && picked.flag == Some(1));

        let none = marker_filter(RatingChoice::Any, FlagChoice::Any, LabelChoice::None);
        assert!(none.no_label && none.label.is_none());
        let red = marker_filter(
            RatingChoice::Any,
            FlagChoice::Any,
            LabelChoice::Is("Red".into()),
        );
        assert!(!red.no_label && red.label.as_deref() == Some("Red"));
    }

    #[test]
    fn the_culling_choices_survive_a_smart_collection_save_and_load() {
        for (r, f, l) in [
            (
                RatingChoice::Unrated,
                FlagChoice::Unflagged,
                LabelChoice::None,
            ),
            (
                RatingChoice::Exactly(1),
                FlagChoice::Picked,
                LabelChoice::Is("Blue".into()),
            ),
            (RatingChoice::Exactly(5), FlagChoice::Any, LabelChoice::Any),
        ] {
            let saved = marker_filter(r, f, l.clone());
            let mut bar = FilterBar::default();
            bar.load_filter(&saved);
            assert_eq!(bar.rating, r);
            assert_eq!(bar.flag, f);
            assert_eq!(bar.label, l);
            assert_eq!(
                bar.to_filter(None),
                saved,
                "loading then rebuilding is lossless"
            );
        }
    }

    #[test]
    fn unrated_combined_with_a_range_is_kept_verbatim_not_widened() {
        // A hand-edited rule: unrated AND rating >= 3 (matches nothing). The controls can't say
        // that, so it must round-trip through `Custom` untouched.
        let rule = Filter {
            unrated: true,
            rating_min: Some(3),
            ..Filter::default()
        };
        assert_eq!(RatingChoice::from_filter(&rule), RatingChoice::Custom);
        let mut bar = FilterBar::default();
        bar.load_filter(&rule);
        assert_eq!(bar.to_filter(None), rule);
    }

    #[test]
    fn a_range_of_exactly_one_star_is_recognised_but_a_wider_one_is_custom() {
        let exact = Filter {
            rating_min: Some(4),
            rating_max: Some(4),
            ..Filter::default()
        };
        assert_eq!(RatingChoice::from_filter(&exact), RatingChoice::Exactly(4));
        let wider = Filter {
            rating_min: Some(2),
            rating_max: Some(4),
            ..Filter::default()
        };
        assert_eq!(RatingChoice::from_filter(&wider), RatingChoice::Custom);
    }

    #[test]
    fn facets_of_a_chosen_unrated_or_unflagged_still_count_the_other_choices() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(volume, "").unwrap();
        let ids: Vec<i64> = (0..4)
            .map(|i| {
                store
                    .insert_asset(root, &asset(&format!("{i}.NEF"), "Z8"), None)
                    .unwrap()
            })
            .collect();
        store.set_rating(&[ids[0]], Some(3)).unwrap();
        store.set_rating(&[ids[1]], Some(3)).unwrap();
        store.set_flag(&[ids[0]], Some(1)).unwrap();

        // Only "Unrated" chosen: the rating facets are counted with that constraint CLEARED, so
        // the other ratings still show real numbers (2 photos rated 3, 2 unrated). Had `unrated`
        // leaked into that query, every rating but "unrated" would read 0.
        let v = compute_facets(
            &store,
            &Filter {
                unrated: true,
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(v.exactly(3), 2, "rating counts ignore the chosen 'unrated'");
        assert_eq!(v.unrated(), 2);
        assert_eq!(
            v.total, 2,
            "while the grid itself shows only the unrated ones"
        );

        // Only "Unflagged" chosen: the flag facets clear it, so "Picked" still counts 1 (and
        // "Unflagged" 3). A leaked `unflagged` would make "Picked" read 0.
        let v = compute_facets(
            &store,
            &Filter {
                unflagged: true,
                ..Filter::default()
            },
        )
        .unwrap();
        assert_eq!(v.picks, 1, "flag counts ignore the chosen 'unflagged'");
        assert_eq!(v.unflagged, 3);
    }

    // ---- contradictory / unexpressible saved rules must round-trip untouched ------------------

    fn assert_round_trips(rule: Filter) {
        let mut bar = FilterBar::default();
        bar.load_filter(&rule);
        assert_eq!(
            bar.to_filter(None),
            rule,
            "rule was widened or narrowed on load"
        );
    }

    #[test]
    fn a_flag_together_with_unflagged_is_kept_not_widened_to_all_picks() {
        // Matches nothing; reloading it as plain "Picked" would match every pick.
        let rule = Filter {
            flag: Some(1),
            unflagged: true,
            ..Filter::default()
        };
        assert_eq!(FlagChoice::from_filter(&rule), FlagChoice::Custom);
        assert_round_trips(rule);
    }

    #[test]
    fn a_label_together_with_no_label_is_kept_not_widened_to_all_of_that_label() {
        let rule = Filter {
            label: Some("Red".into()),
            no_label: true,
            ..Filter::default()
        };
        assert_eq!(LabelChoice::from_filter(&rule), LabelChoice::Custom);
        assert_round_trips(rule);
    }

    #[test]
    fn a_flag_value_the_control_cannot_express_is_kept_not_dropped() {
        // `flag: Some(2)` used to reload as no flag at all, widening the rule to everything.
        let rule = Filter {
            flag: Some(2),
            ..Filter::default()
        };
        assert_eq!(FlagChoice::from_filter(&rule), FlagChoice::Custom);
        assert_round_trips(rule);
    }

    #[test]
    fn expressible_flag_and_label_shapes_stay_ordinary_choices() {
        assert_eq!(
            FlagChoice::from_filter(&Filter {
                flag: Some(1),
                ..Filter::default()
            }),
            FlagChoice::Picked
        );
        assert_eq!(
            FlagChoice::from_filter(&Filter {
                unflagged: true,
                ..Filter::default()
            }),
            FlagChoice::Unflagged
        );
        assert_eq!(
            LabelChoice::from_filter(&Filter::default()),
            LabelChoice::Any
        );
    }

    #[test]
    fn switching_a_custom_flag_or_label_to_a_real_choice_drops_the_saved_shape() {
        let mut bar = FilterBar::default();
        bar.load_filter(&Filter {
            flag: Some(1),
            unflagged: true,
            label: Some("Red".into()),
            no_label: true,
            ..Filter::default()
        });
        bar.flag = FlagChoice::Picked;
        bar.label = LabelChoice::Any;
        let f = bar.to_filter(None);
        assert_eq!((f.flag, f.unflagged), (Some(1), false));
        assert_eq!((f.label, f.no_label), (None, false));
    }
}
