//! The pelt command table (#542, ADR-0434 slice 1): a small, named, risk-tagged set of actions an
//! agent (or a test, or later a command palette) can run against the app without a window.
//!
//! Entries wrap what `PeltApp` already does; no logic moves here. The table talks to the app through
//! [`CommandHost`] rather than `&mut PeltApp` (ADR-0434's sketch): `PeltApp::new` needs eframe's
//! wgpu `RenderState`, so a `PeltApp` cannot be built in a unit test, while a host trait can be
//! mocked. `PeltApp` implements it in `app.rs`; the develop get/set core lives here as free
//! functions over a real `DevelopView`, which tests can build.
//!
//! Requests arrive over a [`CommandQueue`] (a plain `mpsc`, filled by whatever transport slice 2a
//! adds) and are drained on the UI thread, in both `ui` and `logic`, by [`CommandQueue::drain`].
//! [`Risk`] gates what a channel may do: `Destructive` commands run only when the [`Policy`] allows.

use std::sync::mpsc::{self, Receiver, Sender};

use nicti_tapetum::stages::WB;
use serde_json::{json, Map, Value};

use crate::render::DevelopView;

/// What a command may do. The control channel's gate (ADR-0434 section 3) keys off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Looks only.
    Read,
    /// Changes catalog or develop state through the app's normal, undoable paths. Never takes a
    /// filesystem path from the caller.
    Mutate,
    /// Deletes files or writes to a caller-supplied path. Runs only under [`Policy::allow_destructive`].
    Destructive,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Read => "read",
            Risk::Mutate => "mutate",
            Risk::Destructive => "destructive",
        }
    }
}

/// What the channel running a command is allowed to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `--control-allow-destructive`: lets [`Risk::Destructive`] commands run.
    pub allow_destructive: bool,
}

impl Policy {
    fn permits(self, risk: Risk) -> bool {
        risk != Risk::Destructive || self.allow_destructive
    }
}

/// The app, as far as the commands are concerned.
pub trait CommandHost {
    /// View, selection, active asset and the like -- the first thing an agent asks for.
    fn inspect(&self) -> Value;
    /// `"library"`, `"loupe"`, `"develop"`, `"survey"` or `"compare"`.
    fn view_name(&self) -> &'static str;
    /// Switches view. Only `"library"` and `"loupe"` can be entered on request.
    fn set_view(&mut self, view: &str) -> Result<(), String>;
    /// Makes `asset_id` the only selected photo (clearing any multi-selection). Library view only:
    /// the loupe and Develop follow their own cursor and would undo it.
    fn select_asset(&mut self, asset_id: i64) -> Result<(), String>;
    /// Whether the catalog holds a photo with this id.
    fn known_asset(&self, asset_id: i64) -> bool;
    /// The photos a mark acts on right now (the selection, else the cursor).
    fn target_ids(&self) -> Vec<i64>;
    /// Sets (or with `None` clears) the star rating, `-1` being reject, of `ids`.
    fn rate(&mut self, ids: &[i64], rating: Option<i64>) -> Result<(), String>;
    /// Whether Develop has a photo loaded, or why not.
    fn develop_ready(&self) -> Result<(), String>;
    fn develop_get(&self, stage: &str) -> Result<Value, String>;
    fn develop_set(&mut self, stage: &str, patch: &Value) -> Result<Value, String>;
    /// Opens the Export dialog for the current photos. (A headless run needs the dialog's
    /// settings flow to become callable; see ADR-0434.)
    fn export_open(&mut self) -> Result<(), String>;
    /// The photo Develop has loaded, if any. Edits go to this photo, which is not necessarily the
    /// one the grid cursor or the loupe shows.
    fn develop_asset(&self) -> Option<i64>;
    /// The folder `export.run` may write into without `--control-allow-destructive`: the
    /// operator's `--control-export-dir`.
    fn export_dir(&self) -> Option<std::path::PathBuf>;
    /// Starts an export without the dialog (see [`ExportRequest`]); returns at once, the run goes
    /// on in the background (`export.status`).
    fn export_run(&mut self, req: &ExportRequest) -> Result<Value, String>;
    /// Whether an export is running, how far it is, and the last finished run's report.
    fn export_status(&self) -> Value;
    /// The widgets registered on screen (name and rect), or why there are none.
    fn widgets_list(&self) -> Result<Value, String>;
    /// Queues a click on a registered widget (real egui pointer events; see `widgets.rs`).
    fn ui_click(
        &mut self,
        widget: &str,
        button: &str,
        modifiers: &[String],
    ) -> Result<Value, String>;
    /// Queues a key press and release.
    fn ui_key(&mut self, key: &str, modifiers: &[String]) -> Result<Value, String>;
    /// Queues a drag from a registered widget by `(dx, dy)` points.
    fn ui_drag(&mut self, widget: &str, dx: f32, dy: f32) -> Result<Value, String>;
    /// Whether queued input has been delivered and drawn (and, with `jobs`, no background job is
    /// queued or running).
    fn ui_settled(&self, jobs: bool) -> bool;
    /// Asks the window for a screenshot of what is painted (needs a visible frame). One at a
    /// time; the image is collected with [`Self::screenshot_take`].
    fn screenshot_start(&mut self) -> Result<(), String>;
    /// The finished screenshot, encoded for the reply, or `None` while it is still on its way.
    /// `give_up` abandons a request that ran out of time so a new one can start. `max_width == 0`
    /// means nobody wants the image any more (the client left): drop it without encoding.
    fn screenshot_take(&mut self, max_width: u32, give_up: bool) -> Option<Result<Value, String>>;
}

/// A validated `export.run` request. Every path in it is either the operator's export folder or,
/// only under `--control-allow-destructive`, one the caller named.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportRequest {
    /// Photos to export; `None` = what the Export dialog would offer (the selection or the cursor).
    pub assets: Option<Vec<i64>>,
    /// A saved preset (built-in or the user's) to start from; `None` = the dialog's last settings.
    pub preset: Option<String>,
    /// `Some`: a folder the caller named (needs `--control-allow-destructive`). `None`: the
    /// operator's `--control-export-dir`.
    pub destination: Option<std::path::PathBuf>,
    /// A subfolder below the destination, already validated as plain relative components.
    pub subfolder: Option<String>,
    /// `true`: files that already exist are replaced (needs `--control-allow-destructive`).
    /// Otherwise a clash gets a `-2` suffix and nothing is overwritten.
    pub overwrite: bool,
}

/// One entry of the table.
pub struct Command {
    /// `area.verb`, stable: agents key on it.
    pub id: &'static str,
    pub label: &'static str,
    /// Human/agent-readable parameter doc; rendered into MCP tool schemas later.
    pub params: &'static str,
    /// The risk with the fewest privileges this command ever needs.
    pub risk: Risk,
    /// Raises the risk for particular parameters (`export.run` writing to a caller-chosen folder
    /// is `Destructive`; into the operator's `--control-export-dir` it is `Mutate`). `None`: the
    /// risk is always `risk`.
    pub risk_for: Option<fn(&Value) -> Risk>,
    /// `Ok` when the command can run now, else the reason it can't ("why is Export greyed out").
    pub enabled: fn(&dyn CommandHost) -> Result<(), String>,
    pub run: fn(&mut dyn CommandHost, &Value) -> Result<Value, String>,
}

fn always(_: &dyn CommandHost) -> Result<(), String> {
    Ok(())
}

fn get_i64(params: &Value, key: &str) -> Result<i64, String> {
    params
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("`{key}` must be an integer"))
}

fn only_keys(params: &Value, allowed: &[&str]) -> Result<(), String> {
    let obj = params.as_object().ok_or("params must be a JSON object")?;
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown parameter `{k}`")),
        None => Ok(()),
    }
}

fn run_inspect(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    Ok(host.inspect())
}

fn run_view_set(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["view"])?;
    let view = params
        .get("view")
        .and_then(Value::as_str)
        .ok_or("`view` must be a string")?;
    host.set_view(view)?;
    Ok(json!({ "view": host.view_name() }))
}

fn run_grid_select(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["asset"])?;
    let id = get_i64(params, "asset")?;
    host.select_asset(id)?;
    Ok(json!({ "selected": id }))
}

fn run_photo_rate(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["rating", "assets"])?;
    // `null` clears; 1..=5 are stars; -1 rejects. 0 is not a rating, so it is refused rather than
    // silently meaning "clear".
    let rating = match params.get("rating") {
        Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(r) if r == -1 || (1..=5).contains(&r) => Some(r),
            _ => return Err("`rating` must be null, -1 (reject) or 1..=5".into()),
        },
        None => return Err("`rating` is required (null clears)".into()),
    };
    let ids: Vec<i64> = match params.get("assets") {
        None => host.target_ids(),
        Some(v) => v
            .as_array()
            .ok_or("`assets` must be an array of ids")?
            .iter()
            .map(|a| a.as_i64().ok_or("`assets` must be an array of ids"))
            .collect::<Result<_, _>>()?,
    };
    if ids.is_empty() {
        return Err("no photo is selected".into());
    }
    if ids.len() > MAX_RATE_BATCH {
        return Err(format!("at most {MAX_RATE_BATCH} photos per call"));
    }
    // Ratings write XMP sidecars, so only photos the catalog really holds are touched, each once,
    // and the count reported is the count rated.
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<i64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
    if let Some(bad) = ids.iter().find(|id| !host.known_asset(**id)) {
        return Err(format!("photo {bad} is not in the catalog"));
    }
    host.rate(&ids, rating)?;
    Ok(json!({ "rated": ids.len(), "rating": rating }))
}

/// Most photos one `photo.rate` call may touch: the channel is for driving the app, not bulk edits.
pub const MAX_RATE_BATCH: usize = 1000;

fn develop_enabled(host: &dyn CommandHost) -> Result<(), String> {
    host.develop_ready()
}

/// Checks the caller's `asset` against the photo Develop actually has loaded. Develop edits that
/// photo, which can differ from the one on screen in the grid or loupe, so a write names its
/// target and is refused when it does not match (`required`), and a read that names one is held to
/// the same check.
fn check_develop_asset(
    host: &dyn CommandHost,
    params: &Value,
    required: bool,
) -> Result<(), String> {
    let loaded = host.develop_asset();
    match params.get("asset") {
        None if required => Err(match loaded {
            Some(id) => format!(
                "`asset` is required: Develop has photo {id} loaded, which is not necessarily the \
                 photo on screen; name it to confirm the edit is meant for it"
            ),
            None => "`asset` is required (no photo is loaded in Develop)".into(),
        }),
        None => Ok(()),
        Some(v) => {
            let wanted = v.as_i64().ok_or("`asset` must be an integer")?;
            match loaded {
                Some(id) if id == wanted => Ok(()),
                Some(id) => Err(format!(
                    "Develop has photo {id} loaded, not {wanted}; open {wanted} in the loupe first"
                )),
                None => Err("no photo is loaded in Develop".into()),
            }
        }
    }
}

fn with_asset(host: &dyn CommandHost, mut result: Value) -> Value {
    if let (Some(obj), Some(id)) = (result.as_object_mut(), host.develop_asset()) {
        obj.insert("asset".into(), json!(id));
    }
    result
}

fn run_develop_get(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["stage", "asset"])?;
    check_develop_asset(host, params, false)?;
    let stage = params
        .get("stage")
        .and_then(Value::as_str)
        .ok_or("`stage` must be a string")?;
    let got = host.develop_get(stage)?;
    Ok(with_asset(host, got))
}

fn run_develop_set(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["stage", "set", "asset"])?;
    check_develop_asset(host, params, true)?;
    let stage = params
        .get("stage")
        .and_then(Value::as_str)
        .ok_or("`stage` must be a string")?;
    let patch = params.get("set").ok_or("`set` is required")?;
    let set = host.develop_set(stage, patch)?;
    Ok(with_asset(host, set))
}

fn run_export_open(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    host.export_open()?;
    Ok(json!({ "opened": true }))
}

/// Most photos one `export.run` may name.
pub const MAX_EXPORT_BATCH: usize = 1000;
/// Longest `subfolder` accepted.
const MAX_SUBFOLDER_LEN: usize = 120;

/// `export.run` is `Destructive` when it names a folder or asks to overwrite; otherwise it only
/// writes new files into the operator's `--control-export-dir`.
fn export_run_risk(params: &Value) -> Risk {
    let names_folder = params.get("destination").is_some_and(|v| !v.is_null());
    let overwrites = params.get("overwrite").and_then(Value::as_bool) == Some(true);
    if names_folder || overwrites {
        Risk::Destructive
    } else {
        Risk::Mutate
    }
}

/// A `subfolder` is relative plain components joined by `/`: no `..`, no empty or hidden parts, no
/// separators of the other kind, no drive or stream syntax, no control characters. Token templates
/// (`{Date}`) are not accepted from the channel.
pub fn validate_subfolder(s: &str) -> Result<String, String> {
    if s.is_empty() || s.len() > MAX_SUBFOLDER_LEN {
        return Err(format!(
            "`subfolder` must be 1-{MAX_SUBFOLDER_LEN} characters"
        ));
    }
    for part in s.split('/') {
        let bad = part.is_empty()
            || part.starts_with('.')
            || part.ends_with(' ')
            || part.chars().any(|c| {
                c.is_control()
                    || matches!(
                        c,
                        '\\' | ':' | '{' | '}' | '*' | '?' | '"' | '<' | '>' | '|'
                    )
            });
        // The export renders every part through preen's sanitiser (it trims leading spaces and
        // trailing dots/spaces, drops parts that come out empty, prefixes reserved device names
        // and maps bidi controls). A part that it would change is a part that lands somewhere
        // other than where it was checked: `link.` or ` link` name the folder `link`, and ` .`
        // vanishes. Accept only parts it leaves exactly as they are.
        let changed = nicti_preen::naming::sanitize_component(part) != part;
        if bad || changed {
            return Err(format!(
                "`subfolder` part {part:?} is not a plain folder name (no empty or hidden parts, \
                 `..`, backslashes, drive letters, wildcards, templates, leading spaces, trailing \
                 dots or spaces, or reserved device names)"
            ));
        }
    }
    Ok(s.to_owned())
}

/// The spec an `export.run` really exports with, from the starting settings (a preset or the
/// dialog's last): the destination is replaced by `base` + the validated subfolder (whatever the
/// preset said is discarded), the clash policy is decided here and never taken from the preset,
/// and GPS is never included.
pub fn channel_export_spec(
    mut spec: nicti_preen::spec::ExportSpec,
    base: std::path::PathBuf,
    subfolder: Option<String>,
    overwrite: bool,
) -> nicti_preen::spec::ExportSpec {
    use nicti_preen::spec::{CollisionPolicy, DestinationBase, DestinationSpec};
    spec.destination = DestinationSpec {
        base: DestinationBase::Folder(base),
        subfolder,
    };
    spec.collision = if overwrite {
        CollisionPolicy::Overwrite
    } else {
        CollisionPolicy::UniqueSuffix
    };
    spec.metadata.include_location = false;
    spec
}

/// Refuses a `subfolder` whose existing parts under `base` are links (or not folders): the
/// destination would then resolve somewhere other than below the operator's export folder.
/// Parts that do not exist yet are fine (the export creates them as plain folders).
pub fn check_subfolder_unlinked(base: &std::path::Path, subfolder: &str) -> Result<(), String> {
    let mut walk = base.to_path_buf();
    for part in subfolder.split('/') {
        walk.push(part);
        match std::fs::symlink_metadata(&walk) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is a link or not a folder; exports only go into real folders",
                    walk.display()
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("{}: {e}", walk.display())),
        }
    }
    Ok(())
}

fn run_export_run(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(
        params,
        &["assets", "preset", "destination", "subfolder", "overwrite"],
    )?;
    let assets = match params.get("assets") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or("`assets` must be an array of photo ids")?;
            if arr.is_empty() || arr.len() > MAX_EXPORT_BATCH {
                return Err(format!("`assets` must hold 1-{MAX_EXPORT_BATCH} photo ids"));
            }
            let mut seen = std::collections::HashSet::new();
            let mut ids = Vec::with_capacity(arr.len());
            for item in arr {
                let id = item
                    .as_i64()
                    .ok_or("`assets` must be an array of photo ids")?;
                if seen.insert(id) {
                    if !host.known_asset(id) {
                        return Err(format!("photo {id} is not in the catalog"));
                    }
                    ids.push(id);
                }
            }
            Some(ids)
        }
    };
    let preset = match params.get("preset") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().ok_or("`preset` must be a string")?.to_owned()),
    };
    let destination = match params.get("destination") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let path =
                std::path::PathBuf::from(v.as_str().ok_or("`destination` must be a string")?);
            if !path.is_absolute() {
                return Err("`destination` must be an absolute folder path".into());
            }
            Some(path)
        }
    };
    let subfolder = match params.get("subfolder") {
        None | Some(Value::Null) => None,
        Some(v) => Some(validate_subfolder(
            v.as_str().ok_or("`subfolder` must be a string")?,
        )?),
    };
    let overwrite = match params.get("overwrite") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or("`overwrite` must be true or false")?,
    };
    if destination.is_none() && host.export_dir().is_none() {
        return Err(
            "no export folder: start Nicti with --control-export-dir DIR, or (with \
             --control-allow-destructive) pass `destination`"
                .into(),
        );
    }
    host.export_run(&ExportRequest {
        assets,
        preset,
        destination,
        subfolder,
        overwrite,
    })
}

fn run_ui_widgets(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    host.widgets_list()
}

fn str_list(params: &Value, key: &str) -> Result<Vec<String>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .filter(|a| a.len() <= 4)
            .ok_or_else(|| format!("`{key}` must be an array of at most 4 names"))?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{key}` must be an array of names"))
            })
            .collect(),
    }
}

fn widget_param(params: &Value) -> Result<&str, String> {
    params
        .get("widget")
        .and_then(Value::as_str)
        .ok_or_else(|| "`widget` must be a registered widget name (ui.widgets)".to_owned())
}

fn run_ui_click(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["widget", "button", "modifiers"])?;
    let widget = widget_param(params)?;
    let button = match params.get("button") {
        None | Some(Value::Null) => "primary",
        Some(v) => v.as_str().ok_or("`button` must be a string")?,
    };
    host.ui_click(widget, button, &str_list(params, "modifiers")?)
}

fn run_ui_key(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["key", "modifiers"])?;
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .ok_or("`key` must be a key name (\"Delete\", \"A\", \"ArrowRight\", ...)")?;
    host.ui_key(key, &str_list(params, "modifiers")?)
}

fn run_ui_drag(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["widget", "dx", "dy"])?;
    let widget = widget_param(params)?;
    let num = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .ok_or_else(|| format!("`{k}` must be a number of points"))
    };
    host.ui_drag(widget, num("dx")?, num("dy")?)
}

/// What `ui.settle` waits for; parsed here so the queue (which defers the reply) and the plain
/// dispatch agree.
pub fn settle_params(params: &Value) -> Result<(std::time::Duration, bool), String> {
    only_keys(params, &["timeout_ms", "jobs"])?;
    let ms = match params.get("timeout_ms") {
        None | Some(Value::Null) => 5000,
        Some(v) => v
            .as_u64()
            .filter(|ms| (50..=30_000).contains(ms))
            .ok_or("`timeout_ms` must be 50-30000")?,
    };
    let jobs = match params.get("jobs") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or("`jobs` must be true or false")?,
    };
    Ok((std::time::Duration::from_millis(ms), jobs))
}

fn run_ui_settle(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    let (_, jobs) = settle_params(params)?;
    Ok(json!({ "settled": host.ui_settled(jobs) }))
}

/// What `ui.screenshot` asks for: the longest the reply may wait, and the widest image returned
/// (wider windows are scaled down, so a 4K window cannot produce a multi-megabyte reply).
pub fn screenshot_params(params: &Value) -> Result<(std::time::Duration, u32), String> {
    only_keys(params, &["timeout_ms", "max_width"])?;
    let ms = match params.get("timeout_ms") {
        None | Some(Value::Null) => 10_000,
        Some(v) => v
            .as_u64()
            // Below the transport's own 30 s request timeout, so this error is the one the caller sees.
            .filter(|ms| (500..=25_000).contains(ms))
            .ok_or("`timeout_ms` must be 500-25000")?,
    };
    let width = match params.get("max_width") {
        None | Some(Value::Null) => 1600,
        Some(v) => v
            .as_u64()
            .filter(|w| (64..=4096).contains(w))
            .ok_or("`max_width` must be 64-4096")? as u32,
    };
    Ok((std::time::Duration::from_millis(ms), width))
}

fn run_ui_screenshot(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    screenshot_params(params)?;
    host.screenshot_start()?;
    Ok(json!({ "pending": true }))
}

fn run_develop_controls(_: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    Ok(crate::develop_controls::listing())
}

fn run_export_status(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    Ok(host.export_status())
}

/// The table. Grows only when a verification ticket needs a command.
pub const COMMANDS: &[Command] = &[
    Command {
        id: "ui.inspect",
        label: "Inspect the app",
        params: "{}",
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_inspect,
    },
    Command {
        id: "view.set",
        label: "Switch view",
        params: r#"{"view": "library" | "loupe"}  (loupe opens on the grid's selected photo)"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_view_set,
    },
    Command {
        id: "grid.select",
        label: "Select a photo in the grid",
        params: r#"{"asset": <catalog asset id>}"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_grid_select,
    },
    Command {
        id: "photo.rate",
        label: "Rate photos",
        params: r#"{"rating": null | -1 | 1..5, "assets"?: [ids]}  (default: the selection)"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_photo_rate,
    },
    Command {
        id: "develop.get",
        label: "Read a Develop stage's params",
        params: r#"{"stage": <a `stage` listed by develop.controls, for example "nicti.tone", "nicti.hsl", "nicti.color_grade">, "asset"?: <id>}  (the result names the photo Develop has loaded; `asset`, if given, must be it)"#,
        risk: Risk::Read,
        risk_for: None,
        enabled: develop_enabled,
        run: run_develop_get,
    },
    Command {
        id: "develop.set",
        label: "Set fields of a Develop stage",
        params: r#"{"stage": <as develop.get>, "asset": <id of the photo Develop has loaded -- required>, "set": {<field>: <number>, ...}}  (`field` is the control's `field` from develop.controls, a dotted path for nested values such as "bands.2.hue" or "shadows.sat"; unknown fields and out-of-range values are refused; a window whose low end is above its high end is refused; undoable)"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: develop_enabled,
        run: run_develop_set,
    },
    Command {
        id: "develop.controls",
        label: "List the Develop controls (id, section, stage, field, range, default)",
        params: "{}  -> [{id, label, section, stage, field, min, max, default, optional}]  (the panel's own slider specs; use stage + field with develop.get/set)",
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_develop_controls,
    },
    Command {
        id: "ui.widgets",
        label: "List the widgets that can be clicked",
        params: "{}",
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_ui_widgets,
    },
    Command {
        id: "ui.click",
        label: "Click a widget by name",
        params: r#"{"widget": "<name from ui.widgets>", "button"?: "primary" | "secondary" | "middle", "modifiers"?: ["ctrl", "shift", "alt"]}  (real pointer events; returns when queued, then ui.settle)"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_ui_click,
    },
    Command {
        id: "ui.key",
        label: "Press a key",
        params: r#"{"key": "<egui key name>", "modifiers"?: ["ctrl", "shift", "alt"]}  (no text entry)"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_ui_key,
    },
    Command {
        id: "ui.drag",
        label: "Drag from a widget",
        params: r#"{"widget": "<name>", "dx": <points>, "dy": <points>}"#,
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_ui_drag,
    },
    Command {
        id: "ui.settle",
        label: "Wait until queued input has played out",
        params: r#"{"timeout_ms"?: 50-30000 (default 5000), "jobs"?: true (also wait for background jobs)}  -> {"settled": bool}"#,
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_ui_settle,
    },
    Command {
        id: "ui.screenshot",
        label: "Screenshot of the window as painted",
        params: r#"{"timeout_ms"?: 500-25000 (default 10000), "max_width"?: 64-4096 (default 1600; wider windows are scaled down)}  -> {"width", "height", "png_base64"}. A minimised window cannot be captured (times out). The reply line can be several MiB: read it without the request line cap. Never writes a file."#,
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_ui_screenshot,
    },
    Command {
        id: "export.run",
        label: "Export photos without the dialog",
        params: r#"{"assets"?: [ids] (default: the selection or cursor), "preset"?: "<name>" (default: the dialog's last settings), "subfolder"?: "a/b" (plain relative folders under the export folder), "destination"?: "<absolute folder>" and "overwrite"?: true -- both need --control-allow-destructive}  Writes into --control-export-dir; never overwrites unless allowed; GPS is never included. Returns at once; poll export.status"#,
        risk: Risk::Mutate,
        risk_for: Some(export_run_risk),
        enabled: always,
        run: run_export_run,
    },
    Command {
        id: "export.status",
        label: "Export progress and last result",
        params: "{}",
        risk: Risk::Read,
        risk_for: None,
        enabled: always,
        run: run_export_status,
    },
    Command {
        id: "export.open",
        label: "Open the Export dialog",
        params: "{}",
        risk: Risk::Mutate,
        risk_for: None,
        enabled: always,
        run: run_export_open,
    },
];

/// Runs `id` on `host` if the policy allows its risk and it is enabled right now.
pub fn dispatch(
    host: &mut dyn CommandHost,
    policy: Policy,
    id: &str,
    params: &Value,
) -> Result<Value, String> {
    dispatch_in(COMMANDS, host, policy, id, params)
}

/// [`dispatch`] over an explicit table, so the gate can be tested with a destructive entry.
fn dispatch_in(
    table: &[Command],
    host: &mut dyn CommandHost,
    policy: Policy,
    id: &str,
    params: &Value,
) -> Result<Value, String> {
    let cmd = table
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("unknown command `{id}`"))?;
    // A missing params object means "no params".
    let empty = json!({});
    let params = if params.is_null() { &empty } else { params };
    let risk = cmd.risk_for.map_or(cmd.risk, |f| f(params)).max(cmd.risk);
    if !policy.permits(risk) {
        return Err(format!(
            "`{id}` is {} with these parameters and this channel is not allowed to run it",
            risk.as_str()
        ));
    }
    (cmd.enabled)(host).map_err(|why| format!("`{id}` is not available: {why}"))?;
    (cmd.run)(host, params)
}

/// Every command with whether it can run now (and why not), for an agent to discover the surface.
pub fn list(host: &dyn CommandHost, policy: Policy) -> Value {
    Value::Array(
        COMMANDS
            .iter()
            .map(|c| {
                let allowed = policy.permits(c.risk);
                let blocked = if !allowed {
                    Some("not allowed on this channel".to_owned())
                } else {
                    (c.enabled)(host).err()
                };
                json!({
                    "id": c.id,
                    "label": c.label,
                    "params": c.params,
                    "risk": c.risk.as_str(),
                    "enabled": blocked.is_none(),
                    "reason": blocked,
                })
            })
            .collect(),
    )
}

/// One queued request and where its reply goes.
pub struct Request {
    pub id: String,
    pub params: Value,
    pub reply: Sender<Result<Value, String>>,
    /// Set by the transport when it gave up waiting (and told the client so): the request must
    /// not run afterwards.
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// The requests waiting for the UI thread.
pub struct CommandQueue {
    tx: Sender<Request>,
    rx: std::sync::Mutex<Receiver<Request>>,
    /// `ui.settle` requests waiting for the app to go quiet.
    deferred: std::sync::Mutex<Vec<Deferred>>,
}

/// A `ui.settle` whose reply is sent once the UI has settled or its deadline passes. Polled each
/// drain (a deadline loop, never a fixed sleep).
struct Deferred {
    reply: Sender<Result<Value, String>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deadline: std::time::Instant,
    kind: DeferredKind,
}

enum DeferredKind {
    Settle { jobs: bool },
    Screenshot { max_width: u32 },
}

impl Default for CommandQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: std::sync::Mutex::new(rx),
            deferred: std::sync::Mutex::new(Vec::new()),
        }
    }
}

/// Most requests one drain runs: a flooding client must not stall a frame.
pub const MAX_PER_DRAIN: usize = 16;

impl CommandQueue {
    /// A handle a transport thread submits requests with. (The transport is slice 2a; until then
    /// only tests submit.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sender(&self) -> Sender<Request> {
        self.tx.clone()
    }

    /// Runs up to [`MAX_PER_DRAIN`] waiting requests on `host` and replies to each. Returns how
    /// many ran, so the caller can ask for another frame when more are left.
    pub fn drain(&self, host: &mut dyn CommandHost, policy: Policy) -> usize {
        let mut ran = 0;
        // Expired and abandoned waits first, so a retry right after a timeout finds the host's
        // capture slot free.
        self.poll_deferred(host);
        while ran < MAX_PER_DRAIN {
            // The lock is held only for the pop, so a command may itself submit.
            let next = self.rx.lock().ok().and_then(|rx| rx.try_recv().ok());
            let Some(req) = next else { break };
            if req.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                ran += 1; // dropped requests count against the cap too
                continue; // the client was already told it timed out
            }
            let result = if req.id == "commands" {
                Ok(list(host, policy))
            } else {
                dispatch(host, policy, &req.id, &req.params)
            };
            // `ui.settle` that is not settled yet keeps its reply until it is (or times out); a
            // started `ui.screenshot` keeps it until the image arrives.
            let deferral = match (req.id.as_str(), &result) {
                ("ui.settle", Ok(v)) if v["settled"] == false => settle_params(&req.params)
                    .ok()
                    .map(|(t, jobs)| (t, DeferredKind::Settle { jobs })),
                ("ui.screenshot", Ok(_)) => screenshot_params(&req.params)
                    .ok()
                    .map(|(t, max_width)| (t, DeferredKind::Screenshot { max_width })),
                _ => None,
            };
            if let Some((timeout, kind)) = deferral {
                if let Ok(mut d) = self.deferred.lock() {
                    d.push(Deferred {
                        reply: req.reply,
                        cancelled: req.cancelled,
                        deadline: std::time::Instant::now() + timeout,
                        kind,
                    });
                }
                ran += 1;
                continue;
            }
            // The client may have gone; that is its problem, not the app's.
            let _ = req.reply.send(result);
            ran += 1;
        }
        self.poll_deferred(host);
        ran
    }

    /// Answers the waiting `ui.settle` requests that have settled or run out of time.
    fn poll_deferred(&self, host: &mut dyn CommandHost) {
        let Ok(mut waiting) = self.deferred.lock() else {
            return;
        };
        let now = std::time::Instant::now();
        waiting.retain(|d| {
            if d.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                if matches!(d.kind, DeferredKind::Screenshot { .. }) {
                    // Free the host's single capture slot, or every later one would be refused.
                    let _ = host.screenshot_take(0, true);
                }
                return false; // the client gave up
            }
            let timed_out = now >= d.deadline;
            match d.kind {
                DeferredKind::Settle { jobs } => {
                    if host.ui_settled(jobs) {
                        let _ = d.reply.send(Ok(json!({ "settled": true })));
                        return false;
                    }
                    if timed_out {
                        let _ = d.reply.send(Ok(json!({
                            "settled": false,
                            "reason": "timed out before the UI went quiet (input still queued, or the \
                                       window is not being painted)"
                        })));
                        return false;
                    }
                }
                DeferredKind::Screenshot { max_width } => {
                    if let Some(done) = host.screenshot_take(max_width, timed_out) {
                        let _ = d.reply.send(done);
                        return false;
                    }
                    if timed_out {
                        let _ = d.reply.send(Err("no screenshot arrived in time (is the window \
                                                  visible and being painted?)"
                            .to_owned()));
                        return false;
                    }
                }
            }
            true
        });
    }

    /// Whether a `ui.settle` is still waiting (the app should keep painting to answer it).
    pub fn has_deferred(&self) -> bool {
        self.deferred.lock().is_ok_and(|d| !d.is_empty())
    }
}

// ---- Develop get/set over a real `DevelopView` ----

/// The stages `develop.get`/`develop.set` may touch, by id.
pub const DEVELOP_STAGES: &[&str] = crate::develop_controls::STAGES;

fn check_stage(stage: &str) -> Result<(), String> {
    if DEVELOP_STAGES.contains(&stage) {
        Ok(())
    } else {
        Err(format!(
            "stage `{stage}` is not available (one of: {})",
            DEVELOP_STAGES.join(", ")
        ))
    }
}

/// A stage's current params as JSON (its default when the document has no entry).
pub fn develop_stage_get(dev: &DevelopView, stage: &str) -> Result<Value, String> {
    check_stage(stage)?;
    let draft = crate::develop_controls::Draft::load(dev, stage)
        .ok_or_else(|| format!("stage `{stage}` is not available"))?;
    let mut v = draft.to_json();
    if stage == WB {
        // `temp_k` is left out of the document while the white balance is as-shot; show it (from
        // the registry's optional control, `null` = as-shot).
        if let (Some(o), Some(temp)) = (
            v.as_object_mut(),
            crate::develop_controls::find(WB, "temp_k"),
        ) {
            o.insert("temp_k".to_owned(), json!(temp.get(dev)));
        }
    }
    Ok(v)
}

/// The writes a `develop.set` patch asks for, checked against the control registry (the panel's
/// own slider specs) before anything is touched: unknown fields, non-numbers, non-finite or
/// out-of-range values are refused (`null` clears only an optional field such as `temp_k`). Pure,
/// so it is tested without a `DevelopView`.
fn plan_patch(
    stage: &str,
    patch: &Map<String, Value>,
) -> Result<Vec<(&'static crate::develop_controls::Control, Option<f32>)>, String> {
    patch
        .iter()
        .map(|(k, v)| {
            let control = crate::develop_controls::find(stage, k)
                .ok_or_else(|| format!("stage `{stage}` has no field `{k}`"))?;
            let number = if v.is_null() {
                None
            } else {
                Some(
                    v.as_f64()
                        .ok_or_else(|| format!("field `{k}` must be a number"))?,
                )
            };
            Ok((control, control.check(number)?))
        })
        .collect()
}

/// Sets the named fields of a stage (leaving the rest), through the undo history, as one typed
/// `stage_params`/`set_stage_params` round trip (the same path a slider edit takes) and so one undo
/// step. Everything is checked before anything is written.
pub fn develop_stage_set(
    dev: &mut DevelopView,
    stage: &str,
    patch: &Value,
) -> Result<Value, String> {
    check_stage(stage)?;
    let patch = patch.as_object().ok_or("`set` must be an object")?;
    if patch.is_empty() {
        return Err("`set` is empty".into());
    }
    let plan = plan_patch(stage, patch)?;
    let mut draft = crate::develop_controls::Draft::load(dev, stage)
        .ok_or_else(|| format!("stage `{stage}` is not available"))?;
    for (control, value) in plan {
        control.write(&mut draft, value);
    }
    draft.validate()?;
    // Each scripted write is its own undo step, never folded into the user's last slider drag.
    dev.without_gesture_merging(|dev| draft.store(dev));
    develop_stage_get(dev, stage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::stages::{EXPOSURE, TONE};

    #[derive(Default)]
    struct Mock {
        view: &'static str,
        selected: Option<i64>,
        rated: Vec<(Vec<i64>, Option<i64>)>,
        develop: bool,
        opened_export: bool,
        export_dir: Option<std::path::PathBuf>,
        export_requests: Vec<ExportRequest>,
        ui_calls: Vec<String>,
        shot_polls: u32,
        shot_gave_up: u32,
        settled: bool,
    }

    impl CommandHost for Mock {
        fn inspect(&self) -> Value {
            json!({ "view": self.view, "selected": self.selected })
        }
        fn view_name(&self) -> &'static str {
            self.view
        }
        fn set_view(&mut self, view: &str) -> Result<(), String> {
            self.view = match view {
                "library" => "library",
                "loupe" => "loupe",
                other => return Err(format!("cannot switch to `{other}`")),
            };
            Ok(())
        }
        fn select_asset(&mut self, id: i64) -> Result<(), String> {
            self.selected = Some(id);
            Ok(())
        }
        fn known_asset(&self, id: i64) -> bool {
            (1..=100).contains(&id)
        }
        fn target_ids(&self) -> Vec<i64> {
            self.selected.into_iter().collect()
        }
        fn rate(&mut self, ids: &[i64], r: Option<i64>) -> Result<(), String> {
            self.rated.push((ids.to_vec(), r));
            Ok(())
        }
        fn develop_ready(&self) -> Result<(), String> {
            if self.develop {
                Ok(())
            } else {
                Err("no photo is loaded in Develop".into())
            }
        }
        fn develop_get(&self, stage: &str) -> Result<Value, String> {
            Ok(json!({ "stage": stage }))
        }
        fn develop_set(&mut self, stage: &str, patch: &Value) -> Result<Value, String> {
            Ok(json!({ "stage": stage, "patch": patch }))
        }
        fn export_open(&mut self) -> Result<(), String> {
            self.opened_export = true;
            Ok(())
        }
        fn develop_asset(&self) -> Option<i64> {
            self.develop.then_some(7)
        }
        fn export_dir(&self) -> Option<std::path::PathBuf> {
            self.export_dir.clone()
        }
        fn export_run(&mut self, req: &ExportRequest) -> Result<Value, String> {
            self.export_requests.push(req.clone());
            Ok(json!({ "started": true }))
        }
        fn export_status(&self) -> Value {
            json!({ "running": false })
        }
        fn widgets_list(&self) -> Result<Value, String> {
            Ok(json!([{"name": "button:go"}]))
        }
        fn ui_click(&mut self, w: &str, b: &str, m: &[String]) -> Result<Value, String> {
            self.ui_calls.push(format!("click {w} {b} {m:?}"));
            Ok(json!({ "queued": true }))
        }
        fn ui_key(&mut self, k: &str, m: &[String]) -> Result<Value, String> {
            self.ui_calls.push(format!("key {k} {m:?}"));
            Ok(json!({ "queued": true }))
        }
        fn ui_drag(&mut self, w: &str, dx: f32, dy: f32) -> Result<Value, String> {
            self.ui_calls.push(format!("drag {w} {dx} {dy}"));
            Ok(json!({ "queued": true }))
        }
        fn screenshot_start(&mut self) -> Result<(), String> {
            self.ui_calls.push("screenshot".into());
            Ok(())
        }
        fn screenshot_take(&mut self, w: u32, give_up: bool) -> Option<Result<Value, String>> {
            self.shot_polls += 1;
            if give_up {
                self.shot_gave_up += 1;
            }
            if give_up || self.shot_polls < 3 {
                return None;
            }
            Some(Ok(json!({ "width": w, "height": 1, "png_base64": "AA==" })))
        }
        fn ui_settled(&self, _: bool) -> bool {
            self.settled
        }
    }

    const OPEN: Policy = Policy {
        allow_destructive: false,
    };

    #[test]
    fn ids_are_unique_and_every_command_has_docs() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.id), "duplicate id {}", c.id);
            assert!(c.id.contains('.') && !c.label.is_empty() && !c.params.is_empty());
        }
    }

    #[test]
    fn unknown_commands_and_unknown_params_are_refused() {
        let mut m = Mock::default();
        assert!(dispatch(&mut m, OPEN, "nope.nope", &json!({})).is_err());
        let e = dispatch(
            &mut m,
            OPEN,
            "grid.select",
            &json!({"asset": 1, "extra": 2}),
        )
        .unwrap_err();
        assert!(e.contains("unknown parameter `extra`"), "{e}");
        let e = dispatch(&mut m, OPEN, "grid.select", &json!({"asset": "x"})).unwrap_err();
        assert!(e.contains("integer"), "{e}");
        assert_eq!(m.selected, None, "nothing ran");
        // `null` params mean none.
        assert!(dispatch(&mut m, OPEN, "ui.inspect", &Value::Null).is_ok());
    }

    #[test]
    fn select_then_rate_acts_on_the_selection_and_validates_the_rating() {
        let mut m = Mock::default();
        let e = dispatch(&mut m, OPEN, "photo.rate", &json!({"rating": 3})).unwrap_err();
        assert!(e.contains("no photo is selected"), "{e}");
        // Explicit ids work with nothing selected; unknown ids refuse the whole call; repeats are
        // rated once and counted once.
        let out = dispatch(
            &mut m,
            OPEN,
            "photo.rate",
            &json!({"rating": 2, "assets": [9, 9, 10]}),
        )
        .unwrap();
        assert_eq!(out, json!({"rated": 2, "rating": 2}));
        assert_eq!(m.rated.pop().unwrap().0, vec![9, 10]);
        let e = dispatch(
            &mut m,
            OPEN,
            "photo.rate",
            &json!({"rating": 2, "assets": [9, 5000]}),
        )
        .unwrap_err();
        assert!(e.contains("5000") && m.rated.is_empty(), "{e}");
        dispatch(&mut m, OPEN, "grid.select", &json!({"asset": 42})).unwrap();
        let out = dispatch(&mut m, OPEN, "photo.rate", &json!({"rating": 3})).unwrap();
        assert_eq!(out, json!({"rated": 1, "rating": 3}));
        dispatch(&mut m, OPEN, "photo.rate", &json!({"rating": null})).unwrap();
        dispatch(
            &mut m,
            OPEN,
            "photo.rate",
            &json!({"rating": -1, "assets": [7, 8]}),
        )
        .unwrap();
        assert_eq!(
            m.rated,
            vec![
                (vec![42], Some(3)),
                (vec![42], None),
                (vec![7, 8], Some(-1))
            ]
        );
        for bad in [
            json!({"rating": 0}),
            json!({"rating": 6}),
            json!({"rating": 2.5}),
            json!({}),
        ] {
            assert!(dispatch(&mut m, OPEN, "photo.rate", &bad).is_err(), "{bad}");
        }
        let many: Vec<i64> = (0..=MAX_RATE_BATCH as i64).collect();
        assert!(dispatch(
            &mut m,
            OPEN,
            "photo.rate",
            &json!({"rating": 1, "assets": many})
        )
        .is_err());
        assert_eq!(m.rated.len(), 3, "the refused calls rated nothing");
    }

    #[test]
    fn commands_report_why_they_are_unavailable_and_list_shows_it() {
        let mut m = Mock::default();
        let e = dispatch(&mut m, OPEN, "develop.get", &json!({"stage": "nicti.wb"})).unwrap_err();
        assert!(e.contains("no photo is loaded in Develop"), "{e}");
        let listed = list(&m, OPEN);
        let get = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "develop.get")
            .unwrap();
        assert_eq!(get["enabled"], false);
        assert_eq!(get["reason"], "no photo is loaded in Develop");
        m.develop = true;
        assert!(dispatch(&mut m, OPEN, "develop.get", &json!({"stage": "nicti.wb"})).is_ok());
    }

    #[test]
    fn the_risk_gate_blocks_destructive_commands_unless_the_policy_allows() {
        // No destructive command exists yet; pin the gate itself so the first one cannot slip by.
        assert!(!OPEN.permits(Risk::Destructive));
        assert!(OPEN.permits(Risk::Mutate) && OPEN.permits(Risk::Read));
        assert!(Policy {
            allow_destructive: true
        }
        .permits(Risk::Destructive));
        assert!(Risk::Read < Risk::Mutate && Risk::Mutate < Risk::Destructive);
        // And the table advertises nothing destructive by accident.
        assert!(COMMANDS.iter().all(|c| c.risk != Risk::Destructive));
        // A destructive entry is refused without the policy (and never run), and runs with it.
        fn boom(host: &mut dyn CommandHost, _: &Value) -> Result<Value, String> {
            host.select_asset(666)?;
            Ok(json!("ran"))
        }
        let table = [Command {
            id: "test.boom",
            label: "Boom",
            params: "{}",
            risk: Risk::Destructive,
            risk_for: None,
            enabled: always,
            run: boom,
        }];
        let mut m = Mock::default();
        let e = dispatch_in(&table, &mut m, OPEN, "test.boom", &json!({})).unwrap_err();
        assert!(e.contains("not allowed"), "{e}");
        assert_eq!(m.selected, None, "a refused command must not run");
        let allow = Policy {
            allow_destructive: true,
        };
        assert_eq!(
            dispatch_in(&table, &mut m, allow, "test.boom", &json!({})).unwrap(),
            json!("ran")
        );
        assert_eq!(m.selected, Some(666));
    }

    #[test]
    fn the_queue_runs_requests_in_order_replies_to_each_and_caps_a_flood() {
        let q = CommandQueue::default();
        let tx = q.sender();
        let mut replies = Vec::new();
        for i in 0..(MAX_PER_DRAIN + 4) {
            let (rtx, rrx) = mpsc::channel();
            tx.send(Request {
                id: "grid.select".into(),
                params: json!({ "asset": i as i64 }),
                reply: rtx,
                cancelled: Default::default(),
            })
            .unwrap();
            replies.push(rrx);
        }
        let mut m = Mock::default();
        assert_eq!(q.drain(&mut m, OPEN), MAX_PER_DRAIN, "one drain is capped");
        assert_eq!(
            m.selected,
            Some(MAX_PER_DRAIN as i64 - 1),
            "in submit order"
        );
        assert!(replies[0].try_recv().unwrap().is_ok());
        assert!(replies[MAX_PER_DRAIN].try_recv().is_err(), "not yet run");
        assert_eq!(q.drain(&mut m, OPEN), 4);
        assert!(replies[MAX_PER_DRAIN + 3].try_recv().unwrap().is_ok());
        // `commands` lists the table; a dropped reply channel doesn't panic the drain.
        let (rtx, rrx) = mpsc::channel();
        tx.send(Request {
            id: "commands".into(),
            params: Value::Null,
            reply: rtx,
            cancelled: Default::default(),
        })
        .unwrap();
        let (dead, _) = mpsc::channel();
        tx.send(Request {
            id: "ui.inspect".into(),
            params: Value::Null,
            reply: dead,
            cancelled: Default::default(),
        })
        .unwrap();
        assert_eq!(q.drain(&mut m, OPEN), 2);
        assert_eq!(
            rrx.try_recv().unwrap().unwrap().as_array().unwrap().len(),
            COMMANDS.len()
        );
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn plan_patch_refuses_unknown_fields_wrong_types_and_out_of_range_numbers() {
        let ok = plan_patch(EXPOSURE, &obj(json!({"stops": 2.5}))).unwrap();
        assert_eq!(ok[0].1, Some(2.5));
        for bad in [
            json!({"nope": 1}),
            json!({"stops": "high"}),
            json!({"stops": true}),
            json!({"stops": null}),
            json!({"stops": 5.1}),
            json!({"stops": -5.1}),
            // f32 parses this as infinity, which would serialize to null and poison the document.
            json!({"stops": 1e300}),
            json!({"stops": 3e38}),
        ] {
            assert!(plan_patch(EXPOSURE, &obj(bad.clone())).is_err(), "{bad}");
        }
        // The documented limits are inclusive.
        assert!(plan_patch(EXPOSURE, &obj(json!({"stops": 5.0}))).is_ok());
        assert!(plan_patch(TONE, &obj(json!({"contrast": 1.0}))).is_ok());
        assert!(plan_patch(TONE, &obj(json!({"contrast": 1.5}))).is_err());
        // One bad field refuses the whole patch.
        assert!(plan_patch(TONE, &obj(json!({"contrast": 0.5, "nope": 1}))).is_err());
    }

    #[test]
    fn white_balance_temperature_can_be_set_and_cleared_although_as_shot_omits_it() {
        let set = plan_patch(WB, &obj(json!({"temp_k": 5500.0}))).unwrap();
        assert_eq!(set[0].1, Some(5500.0));
        let cleared = plan_patch(WB, &obj(json!({"temp_k": null}))).unwrap();
        assert_eq!(cleared[0].1, None, "null returns to as-shot");
        assert!(plan_patch(WB, &obj(json!({"temp_k": 100.0}))).is_err());
        assert!(plan_patch(WB, &obj(json!({"tint": 200.0}))).is_err());
        // `null` clears only optional fields.
        assert!(plan_patch(WB, &obj(json!({"tint": null}))).is_err());
    }

    #[test]
    fn abandoned_requests_count_against_the_per_drain_cap() {
        let q = CommandQueue::default();
        let tx = q.sender();
        for _ in 0..(MAX_PER_DRAIN + 4) {
            let (rtx, _rrx) = mpsc::channel();
            tx.send(Request {
                id: "ui.inspect".into(),
                params: Value::Null,
                reply: rtx,
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            })
            .unwrap();
        }
        let mut m = Mock::default();
        assert_eq!(
            q.drain(&mut m, OPEN),
            MAX_PER_DRAIN,
            "a flood of abandoned requests stalled the frame"
        );
        assert_eq!(
            q.drain(&mut m, OPEN),
            4,
            "the rest are left for the next frame"
        );
    }

    fn develop() -> Option<DevelopView> {
        Some(DevelopView::new(crate::test_gpu::shared()?))
    }

    #[test]
    fn develop_set_patches_fields_through_the_undo_history_and_refuses_bad_input() {
        let Some(mut dev) = develop() else { return };
        let before = develop_stage_get(&dev, EXPOSURE).unwrap();
        assert_eq!(before["stops"], 0.0);
        let out = develop_stage_set(&mut dev, EXPOSURE, &json!({"stops": 1.5})).unwrap();
        assert_eq!(out["stops"], 1.5);
        assert_eq!(develop_stage_get(&dev, EXPOSURE).unwrap()["stops"], 1.5);
        // Patching one field keeps the others.
        develop_stage_set(&mut dev, TONE, &json!({"contrast": 0.25})).unwrap();
        // History timestamps are whole milliseconds; two writes in the same one are one gesture.
        std::thread::sleep(std::time::Duration::from_millis(3));
        develop_stage_set(&mut dev, TONE, &json!({"shadows": 0.5})).unwrap();
        let tone = develop_stage_get(&dev, TONE).unwrap();
        assert_eq!(
            (tone["contrast"].as_f64(), tone["shadows"].as_f64()),
            (Some(0.25), Some(0.5))
        );
        // Each scripted write is its own undo step, even straight after another one: undoing
        // the `shadows` write leaves the earlier `contrast` write in place.
        assert!(dev.undo());
        let tone = develop_stage_get(&dev, TONE).unwrap();
        assert_eq!(
            (tone["contrast"].as_f64(), tone["shadows"].as_f64()),
            (Some(0.25), Some(0.0))
        );
        // White balance reads back with an explicit as-shot `temp_k`, and can be set and cleared.
        assert!(develop_stage_get(&dev, WB).unwrap()["temp_k"].is_null());
        develop_stage_set(&mut dev, WB, &json!({"temp_k": 6500.0})).unwrap();
        assert_eq!(develop_stage_get(&dev, WB).unwrap()["temp_k"], 6500.0);
        develop_stage_set(&mut dev, WB, &json!({"temp_k": null})).unwrap();
        assert!(develop_stage_get(&dev, WB).unwrap()["temp_k"].is_null());
        assert!(develop_stage_set(&mut dev, EXPOSURE, &json!({"stops": 1e300})).is_err());
        // Bad input writes nothing.
        let stops = develop_stage_get(&dev, EXPOSURE).unwrap();
        for (stage, patch) in [
            (EXPOSURE, json!({"nope": 1})),
            (EXPOSURE, json!({"stops": "high"})),
            (EXPOSURE, json!({})),
            (EXPOSURE, json!([1])),
            ("nicti.heal", json!({"x": 1})),
        ] {
            assert!(
                develop_stage_set(&mut dev, stage, &patch).is_err(),
                "{stage} {patch}"
            );
        }
        assert_eq!(develop_stage_get(&dev, EXPOSURE).unwrap(), stops);
        assert!(develop_stage_get(&dev, "nicti.heal").is_err());
    }

    const ALLOW: Policy = Policy {
        allow_destructive: true,
    };

    /// An absolute folder path on any platform (`/tmp/x` is not absolute on Windows).
    fn abs_path(name: &str) -> String {
        std::env::temp_dir().join(name).display().to_string()
    }

    fn export_host() -> Mock {
        Mock {
            export_dir: Some("/srv/exports".into()),
            ..Mock::default()
        }
    }

    #[test]
    fn export_run_into_the_operators_folder_is_mutate_and_anything_else_is_destructive() {
        // Into --control-export-dir: allowed on a default channel.
        let mut m = export_host();
        let r = dispatch(
            &mut m,
            OPEN,
            "export.run",
            &json!({"assets": [3, 3, 4], "preset": "Web", "subfolder": "shoot/a"}),
        )
        .unwrap();
        assert_eq!(r["started"], true);
        let req = m.export_requests.pop().unwrap();
        assert_eq!(req.assets, Some(vec![3, 4]), "duplicates dropped");
        assert_eq!(req.preset.as_deref(), Some("Web"));
        assert_eq!(req.subfolder.as_deref(), Some("shoot/a"));
        assert!(req.destination.is_none() && !req.overwrite);

        // A caller-named folder, or overwrite, needs --control-allow-destructive.
        for params in [
            json!({"destination": abs_path("anywhere")}),
            json!({"overwrite": true}),
            json!({"destination": abs_path("x"), "overwrite": true}),
        ] {
            let mut m = export_host();
            let e = dispatch(&mut m, OPEN, "export.run", &params).unwrap_err();
            assert!(e.contains("not allowed"), "{params}: {e}");
            assert!(
                m.export_requests.is_empty(),
                "a refused export must not start"
            );
            dispatch(&mut m, ALLOW, "export.run", &params).unwrap();
            assert_eq!(m.export_requests.len(), 1, "{params}");
        }
        // overwrite:false is not an overwrite.
        let mut m = export_host();
        dispatch(&mut m, OPEN, "export.run", &json!({"overwrite": false})).unwrap();
    }

    #[test]
    fn export_run_needs_a_folder_and_checks_its_parameters() {
        // No operator folder and no destination: nothing to write to.
        let mut m = Mock::default();
        let e = dispatch(&mut m, OPEN, "export.run", &json!({})).unwrap_err();
        assert!(e.contains("--control-export-dir"), "{e}");
        // ...but a named destination works where destructive is allowed.
        dispatch(
            &mut m,
            ALLOW,
            "export.run",
            &json!({"destination": abs_path("out")}),
        )
        .unwrap();
        assert!(dispatch(
            &mut m,
            ALLOW,
            "export.run",
            &json!({"destination": "relative/dir"})
        )
        .is_err());

        let mut m = export_host();
        for bad in [
            json!({"assets": []}),
            json!({"assets": "all"}),
            json!({"assets": [1, "x"]}),
            json!({"assets": [999]}),
            json!({"assets": (1..=1001).collect::<Vec<i64>>()}),
            json!({"preset": 5}),
            json!({"subfolder": ".."}),
            json!({"subfolder": 7}),
            json!({"overwrite": "yes"}),
            json!({"format": "png"}),
        ] {
            assert!(dispatch(&mut m, OPEN, "export.run", &bad).is_err(), "{bad}");
        }
        assert!(m.export_requests.is_empty());
        // The status command is read-only and takes no parameters.
        assert!(dispatch(&mut m, OPEN, "export.status", &json!({})).unwrap()["running"] == false);
        assert!(dispatch(&mut m, OPEN, "export.status", &json!({"x": 1})).is_err());
    }

    #[test]
    fn a_subfolder_is_plain_relative_folders_only() {
        for ok in ["shoot", "2026/10/anthrocon", "a b/c-d_e"] {
            assert_eq!(validate_subfolder(ok).as_deref(), Ok(ok));
        }
        let long = "a".repeat(MAX_SUBFOLDER_LEN + 1);
        for bad in [
            "",
            "..",
            "../x",
            "a/../b",
            "/abs",
            "a//b",
            "a/",
            ".hidden",
            "a/.git",
            "a\\b",
            "C:x",
            "a:b",
            "{Date}",
            "x/{Folder}",
            "a*",
            "q?",
            "a|b",
            "tab\there",
            "trail ",
            // What preen's sanitiser would change: a symlink named `link` inside the export folder
            // is reached through these, past the link check.
            "link.",
            " link",
            " ./link",
            "x/ .",
            "a/b.",
            "a /b",
            "CON",
            "con.txt",
            "a/NUL",
            "\u{202E}gpj",
            long.as_str(),
        ] {
            assert!(validate_subfolder(bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_subfolder_that_runs_through_a_link_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let (base, elsewhere) = (t.path().join("exports"), t.path().join("elsewhere"));
        std::fs::create_dir_all(base.join("real/inner")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("link")).unwrap();
        std::fs::write(base.join("file"), b"x").unwrap();
        assert!(check_subfolder_unlinked(&base, "real/inner").is_ok());
        assert!(
            check_subfolder_unlinked(&base, "real/new/deeper").is_ok(),
            "not yet created"
        );
        assert!(check_subfolder_unlinked(&base, "link").is_err());
        assert!(check_subfolder_unlinked(&base, "link/inside").is_err());
        assert!(
            check_subfolder_unlinked(&base, "file/x").is_err(),
            "a file is not a folder"
        );
    }

    #[test]
    fn develop_edits_name_their_photo_and_are_refused_for_any_other() {
        let mut m = Mock {
            develop: true,
            ..Mock::default()
        };
        // develop.set must say which photo (Develop's loaded one is 7 in the mock).
        let e = dispatch(
            &mut m,
            OPEN,
            "develop.set",
            &json!({"stage": "nicti.wb", "set": {}}),
        )
        .unwrap_err();
        assert!(e.contains("`asset` is required") && e.contains('7'), "{e}");
        let e = dispatch(
            &mut m,
            OPEN,
            "develop.set",
            &json!({"stage": "nicti.wb", "set": {}, "asset": 8}),
        )
        .unwrap_err();
        assert!(e.contains("not 8"), "{e}");
        assert!(dispatch(
            &mut m,
            OPEN,
            "develop.set",
            &json!({"stage": "nicti.wb", "set": {}, "asset": "7"})
        )
        .is_err());
        let ok = dispatch(
            &mut m,
            OPEN,
            "develop.set",
            &json!({"stage": "nicti.wb", "set": {}, "asset": 7}),
        )
        .unwrap();
        assert_eq!(ok["asset"], 7, "the result names the edited photo");

        // develop.get: asset optional, held to the same check when given, and always reported.
        let got = dispatch(&mut m, OPEN, "develop.get", &json!({"stage": "nicti.wb"})).unwrap();
        assert_eq!(got["asset"], 7);
        assert!(dispatch(
            &mut m,
            OPEN,
            "develop.get",
            &json!({"stage": "nicti.wb", "asset": 9})
        )
        .is_err());
    }

    #[test]
    fn the_command_list_shows_export_run_at_its_base_risk() {
        let host = export_host();
        let list = list(&host, OPEN);
        let run = list
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "export.run")
            .unwrap();
        assert_eq!(run["risk"], "mutate");
        assert_eq!(run["enabled"], true);
        assert!(run["params"]
            .as_str()
            .unwrap()
            .contains("--control-allow-destructive"));
    }

    #[test]
    fn the_channels_export_settings_override_whatever_the_preset_says() {
        use nicti_preen::spec::{CollisionPolicy, DestinationBase, DestinationSpec, ExportSpec};
        let mut preset = ExportSpec {
            collision: CollisionPolicy::Overwrite,
            destination: DestinationSpec {
                base: DestinationBase::Folder("/somewhere/else".into()),
                subfolder: Some("../../escape".into()),
            },
            ..ExportSpec::default()
        };
        preset.metadata.include_location = true;
        let base = std::path::PathBuf::from("/srv/exports");

        let spec = channel_export_spec(preset.clone(), base.clone(), Some("shoot/a".into()), false);
        assert_eq!(spec.destination.base, DestinationBase::Folder(base.clone()));
        assert_eq!(spec.destination.subfolder.as_deref(), Some("shoot/a"));
        assert_eq!(
            spec.collision,
            CollisionPolicy::UniqueSuffix,
            "never the preset's overwrite"
        );
        assert!(
            !spec.metadata.include_location,
            "GPS is never published from the channel"
        );
        assert_eq!(spec.format, preset.format, "the rest of the preset is kept");

        let spec = channel_export_spec(preset.clone(), base.clone(), None, true);
        assert_eq!(
            spec.collision,
            CollisionPolicy::Overwrite,
            "only an explicit overwrite"
        );
        assert!(spec.destination.subfolder.is_none());
    }

    #[test]
    fn nothing_that_passes_validation_changes_under_the_export_sanitiser() {
        // The property the link check relies on: a validated part is exactly the folder name preen
        // will create, so what was inspected is what gets written to.
        for ok in [
            "shoot",
            "2026/10/anthrocon",
            "a b/c-d_e",
            "caf\u{e9}/\u{65e5}\u{672c}",
        ] {
            let v = validate_subfolder(ok).unwrap();
            for part in v.split('/') {
                assert_eq!(nicti_preen::naming::sanitize_component(part), part);
            }
        }
    }

    #[test]
    fn ui_input_commands_validate_their_parameters_and_reach_the_host() {
        let mut m = Mock::default();
        // They are Mutate: a default channel may run them.
        let r = dispatch(
            &mut m,
            OPEN,
            "ui.click",
            &json!({"widget": "button:export", "button": "secondary", "modifiers": ["ctrl"]}),
        )
        .unwrap();
        assert_eq!(r["queued"], true);
        dispatch(&mut m, OPEN, "ui.key", &json!({"key": "Delete"})).unwrap();
        dispatch(
            &mut m,
            OPEN,
            "ui.drag",
            &json!({"widget": "cell:7", "dx": 30, "dy": -4.5}),
        )
        .unwrap();
        assert_eq!(
            m.ui_calls,
            vec![
                "click button:export secondary [\"ctrl\"]".to_string(),
                "key Delete []".to_string(),
                "drag cell:7 30 -4.5".to_string(),
            ]
        );
        for (id, bad) in [
            ("ui.click", json!({})),
            ("ui.click", json!({"widget": 3})),
            ("ui.click", json!({"widget": "button:x", "button": 1})),
            (
                "ui.click",
                json!({"widget": "button:x", "modifiers": "ctrl"}),
            ),
            (
                "ui.click",
                json!({"widget": "button:x", "modifiers": ["a", "b", "c", "d", "e"]}),
            ),
            ("ui.click", json!({"widget": "button:x", "x": 10, "y": 10})),
            ("ui.key", json!({})),
            ("ui.key", json!({"key": "A", "text": "hello"})),
            ("ui.drag", json!({"widget": "cell:7", "dx": "far", "dy": 0})),
            ("ui.drag", json!({"widget": "cell:7", "dx": 1})),
            ("ui.widgets", json!({"x": 1})),
        ] {
            let before = m.ui_calls.len();
            assert!(dispatch(&mut m, OPEN, id, &bad).is_err(), "{id} {bad}");
            assert_eq!(
                m.ui_calls.len(),
                before,
                "a refused call must not reach the host"
            );
        }
        assert!(dispatch(&mut m, OPEN, "ui.widgets", &json!({}))
            .unwrap()
            .is_array());
    }

    fn settle_request(params: Value) -> (Request, mpsc::Receiver<Result<Value, String>>) {
        let (rtx, rrx) = mpsc::channel();
        (
            Request {
                id: "ui.settle".into(),
                params,
                reply: rtx,
                cancelled: Default::default(),
            },
            rrx,
        )
    }

    #[test]
    fn ui_settle_replies_when_settled_or_out_of_time_and_never_sleeps() {
        let q = CommandQueue::default();
        let tx = q.sender();
        let mut m = Mock::default(); // not settled

        // Already settled: an immediate reply.
        let mut quiet = Mock {
            settled: true,
            ..Mock::default()
        };
        let (req, rx) = settle_request(json!({}));
        tx.send(req).unwrap();
        q.drain(&mut quiet, OPEN);
        assert_eq!(rx.try_recv().unwrap().unwrap()["settled"], true);
        assert!(!q.has_deferred());

        // Not settled yet: no reply, the request is kept...
        let (req, rx) = settle_request(json!({"timeout_ms": 30_000}));
        tx.send(req).unwrap();
        q.drain(&mut m, OPEN);
        assert!(rx.try_recv().is_err());
        assert!(q.has_deferred());
        q.drain(&mut m, OPEN);
        assert!(rx.try_recv().is_err(), "still waiting, not busy-replying");
        // ...and answered by a later drain once the host settles.
        m.settled = true;
        q.drain(&mut m, OPEN);
        assert_eq!(rx.try_recv().unwrap().unwrap()["settled"], true);
        assert!(!q.has_deferred());

        // A deadline: answered `settled:false` with a reason, not left to the transport's timeout.
        m.settled = false;
        let (req, rx) = settle_request(json!({"timeout_ms": 50}));
        tx.send(req).unwrap();
        q.drain(&mut m, OPEN);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reply = loop {
            q.drain(&mut m, OPEN);
            if let Ok(r) = rx.try_recv() {
                break r.unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the deadline never fired"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(reply["settled"], false);
        assert!(reply["reason"].as_str().unwrap().contains("timed out"));

        // A client that gave up is dropped without a reply.
        let (req, rx) = settle_request(json!({"timeout_ms": 30_000}));
        let cancelled = req.cancelled.clone();
        tx.send(req).unwrap();
        q.drain(&mut m, OPEN);
        assert!(q.has_deferred());
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        q.drain(&mut m, OPEN);
        assert!(!q.has_deferred() && rx.try_recv().is_err());

        // Bad parameters are refused at once.
        for bad in [
            json!({"timeout_ms": 5}),
            json!({"timeout_ms": 99_999}),
            json!({"jobs": 1}),
            json!({"x": 1}),
        ] {
            let (req, rx) = settle_request(bad.clone());
            tx.send(req).unwrap();
            q.drain(&mut m, OPEN);
            assert!(rx.try_recv().unwrap().is_err(), "{bad}");
        }
        assert!(!q.has_deferred());
    }

    #[test]
    fn ui_screenshot_waits_for_the_image_and_rejects_bad_parameters() {
        let q = CommandQueue::default();
        let tx = q.sender();
        let mut m = Mock::default();
        let shot = |params: Value| {
            let (rtx, rrx) = mpsc::channel();
            (
                Request {
                    id: "ui.screenshot".into(),
                    params,
                    reply: rtx,
                    cancelled: Default::default(),
                },
                rrx,
            )
        };
        let (req, rx) = shot(json!({"max_width": 800}));
        tx.send(req).unwrap();
        q.drain(&mut m, OPEN);
        assert_eq!(m.ui_calls, ["screenshot"]);
        assert!(rx.try_recv().is_err(), "the image has not arrived yet");
        assert!(q.has_deferred());
        q.drain(&mut m, OPEN);
        q.drain(&mut m, OPEN);
        let v = rx.try_recv().unwrap().unwrap();
        assert_eq!(v["width"], 800, "max_width reaches the host");
        assert!(!q.has_deferred());

        // A deadline gives an error (and tells the host to give up), never a hang.
        let mut slow = Mock::default();
        let (req, rx) = shot(json!({"timeout_ms": 500}));
        tx.send(req).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reply = loop {
            q.drain(&mut slow, OPEN);
            slow.shot_polls = 0; // the image never arrives
            if let Ok(r) = rx.try_recv() {
                break r;
            }
            assert!(std::time::Instant::now() < end, "deadline never fired");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(reply.unwrap_err().contains("no screenshot arrived"));
        assert!(slow.shot_gave_up > 0, "the host was told to give up");

        // A client that gave up frees the host's capture slot.
        let (req, _rx) = shot(json!({}));
        let cancelled = req.cancelled.clone();
        tx.send(req).unwrap();
        q.drain(&mut slow, OPEN);
        slow.shot_polls = 0;
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        let polls = slow.shot_gave_up;
        q.drain(&mut slow, OPEN);
        assert!(slow.shot_gave_up > polls, "the host was told to give up");
        assert!(!q.has_deferred());

        for bad in [
            json!({"timeout_ms": 5}),
            json!({"timeout_ms": 30_000}),
            json!({"max_width": 10}),
            json!({"max_width": 99_999}),
            json!({"path": "/tmp/x.png"}),
        ] {
            let before = m.ui_calls.len();
            let (req, rx) = shot(bad.clone());
            tx.send(req).unwrap();
            q.drain(&mut m, OPEN);
            assert!(rx.try_recv().unwrap().is_err(), "{bad}");
            assert_eq!(
                m.ui_calls.len(),
                before,
                "a refused call must not start a capture"
            );
        }
    }
}
