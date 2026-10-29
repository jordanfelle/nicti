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

use nicti_tapetum::coat::{ExposureParams, PresenceParams, ToneParams, VibranceParams, WbParams};
use nicti_tapetum::stages::{EXPOSURE, PRESENCE, TONE, VIBRANCE, WB};
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
}

/// One entry of the table.
pub struct Command {
    /// `area.verb`, stable: agents key on it.
    pub id: &'static str,
    pub label: &'static str,
    /// Human/agent-readable parameter doc; rendered into MCP tool schemas later.
    pub params: &'static str,
    pub risk: Risk,
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

fn run_develop_get(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["stage"])?;
    let stage = params
        .get("stage")
        .and_then(Value::as_str)
        .ok_or("`stage` must be a string")?;
    host.develop_get(stage)
}

fn run_develop_set(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &["stage", "set"])?;
    let stage = params
        .get("stage")
        .and_then(Value::as_str)
        .ok_or("`stage` must be a string")?;
    let patch = params.get("set").ok_or("`set` is required")?;
    host.develop_set(stage, patch)
}

fn run_export_open(host: &mut dyn CommandHost, params: &Value) -> Result<Value, String> {
    only_keys(params, &[])?;
    host.export_open()?;
    Ok(json!({ "opened": true }))
}

/// The table. Grows only when a verification ticket needs a command.
pub const COMMANDS: &[Command] = &[
    Command {
        id: "ui.inspect",
        label: "Inspect the app",
        params: "{}",
        risk: Risk::Read,
        enabled: always,
        run: run_inspect,
    },
    Command {
        id: "view.set",
        label: "Switch view",
        params: r#"{"view": "library" | "loupe"}  (loupe opens on the grid's selected photo)"#,
        risk: Risk::Mutate,
        enabled: always,
        run: run_view_set,
    },
    Command {
        id: "grid.select",
        label: "Select a photo in the grid",
        params: r#"{"asset": <catalog asset id>}"#,
        risk: Risk::Mutate,
        enabled: always,
        run: run_grid_select,
    },
    Command {
        id: "photo.rate",
        label: "Rate photos",
        params: r#"{"rating": null | -1 | 1..5, "assets"?: [ids]}  (default: the selection)"#,
        risk: Risk::Mutate,
        enabled: always,
        run: run_photo_rate,
    },
    Command {
        id: "develop.get",
        label: "Read a Develop stage's params",
        params: r#"{"stage": "nicti.wb" | "nicti.exposure" | "nicti.tone" | "nicti.vibrance" | "nicti.presence"}"#,
        risk: Risk::Read,
        enabled: develop_enabled,
        run: run_develop_get,
    },
    Command {
        id: "develop.set",
        label: "Set fields of a Develop stage",
        params: r#"{"stage": <as develop.get>, "set": {<field>: <number>, ...}}  (unknown fields are refused; undoable)"#,
        risk: Risk::Mutate,
        enabled: develop_enabled,
        run: run_develop_set,
    },
    Command {
        id: "export.open",
        label: "Open the Export dialog",
        params: "{}",
        risk: Risk::Mutate,
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
    if !policy.permits(cmd.risk) {
        return Err(format!(
            "`{id}` is {} and this channel is not allowed to run it",
            cmd.risk.as_str()
        ));
    }
    // A missing params object means "no params".
    let empty = json!({});
    let params = if params.is_null() { &empty } else { params };
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
}

/// The requests waiting for the UI thread.
pub struct CommandQueue {
    tx: Sender<Request>,
    rx: std::sync::Mutex<Receiver<Request>>,
}

impl Default for CommandQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: std::sync::Mutex::new(rx),
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
        while ran < MAX_PER_DRAIN {
            // The lock is held only for the pop, so a command may itself submit.
            let next = self.rx.lock().ok().and_then(|rx| rx.try_recv().ok());
            let Some(req) = next else { break };
            let result = if req.id == "commands" {
                Ok(list(host, policy))
            } else {
                dispatch(host, policy, &req.id, &req.params)
            };
            // The client may have gone; that is its problem, not the app's.
            let _ = req.reply.send(result);
            ran += 1;
        }
        ran
    }
}

// ---- Develop get/set over a real `DevelopView` ----

/// The stages `develop.get`/`develop.set` may touch, by id.
pub const DEVELOP_STAGES: &[&str] = &[WB, EXPOSURE, TONE, VIBRANCE, PRESENCE];

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

fn read_typed<T>(dev: &DevelopView, stage: &str) -> Value
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
{
    serde_json::to_value(dev.stage_params::<T>(stage)).expect("coat params always serialize")
}

/// A stage's current params as JSON (its default when the document has no entry).
pub fn develop_stage_get(dev: &DevelopView, stage: &str) -> Result<Value, String> {
    check_stage(stage)?;
    Ok(match stage {
        WB => {
            let mut v = read_typed::<WbParams>(dev, stage);
            // `temp_k` is left out of the document while the white balance is as-shot; show it.
            if let Some(o) = v.as_object_mut() {
                o.entry("temp_k").or_insert(Value::Null);
            }
            v
        }
        EXPOSURE => read_typed::<ExposureParams>(dev, stage),
        TONE => read_typed::<ToneParams>(dev, stage),
        VIBRANCE => read_typed::<VibranceParams>(dev, stage),
        _ => read_typed::<PresenceParams>(dev, stage),
    })
}

/// Fields a stage may have that serialize only when set (so `read_typed` does not show them).
fn optional_fields(stage: &str) -> &'static [&'static str] {
    if stage == WB {
        &["temp_k"]
    } else {
        &[]
    }
}

/// The accepted range of a stage's numeric fields, from the documented slider ranges. A value
/// outside it is refused rather than clamped, so an agent learns it was wrong.
fn field_range(stage: &str, field: &str) -> (f64, f64) {
    match (stage, field) {
        (WB, "temp_k") => (2000.0, 50000.0),
        (WB, _) => (-150.0, 150.0),
        (EXPOSURE, _) => (-5.0, 5.0),
        _ => (-1.0, 1.0),
    }
}

/// Merges `patch` into `current` (a stage's serialized params), refusing unknown fields,
/// non-numeric or out-of-range values (`null` clears an optional field). Pure, so it is tested
/// without a `DevelopView`.
fn merge_patch(
    stage: &str,
    mut current: Map<String, Value>,
    patch: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let optional = optional_fields(stage);
    for (k, v) in patch {
        if !current.contains_key(k) && !optional.contains(&k.as_str()) {
            return Err(format!("stage `{stage}` has no field `{k}`"));
        }
        if v.is_null() && optional.contains(&k.as_str()) {
            current.remove(k);
            continue;
        }
        let n = v
            .as_f64()
            .ok_or_else(|| format!("field `{k}` must be a number"))?;
        let (lo, hi) = field_range(stage, k);
        // `1e300` parses into an f32 as infinity, which would serialize back to `null` and poison
        // the document, so the bound is checked on the caller's number, not on the parsed one.
        if !(lo..=hi).contains(&n) {
            return Err(format!("field `{k}` must be within {lo}..={hi}"));
        }
        current.insert(k.clone(), v.clone());
    }
    Ok(current)
}

fn apply_patch<T>(
    dev: &mut DevelopView,
    stage: &str,
    patch: &Map<String, Value>,
) -> Result<Value, String>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
{
    let current = match read_typed::<T>(dev, stage) {
        Value::Object(o) => o,
        _ => return Err(format!("stage `{stage}` has no named fields")),
    };
    let merged = merge_patch(stage, current, patch)?;
    let next: T = serde_json::from_value(Value::Object(merged))
        .map_err(|e| format!("invalid params for `{stage}`: {e}"))?;
    // Each scripted write is its own undo step, never folded into the user's last slider drag.
    dev.without_gesture_merging(|dev| dev.set_stage_params(stage, &next));
    develop_stage_get(dev, stage)
}

/// Sets the named fields of a stage (leaving the rest), through the undo history. Unknown fields,
/// wrong types and non-finite numbers are refused before anything is written.
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
    match stage {
        WB => apply_patch::<WbParams>(dev, stage, patch),
        EXPOSURE => apply_patch::<ExposureParams>(dev, stage, patch),
        TONE => apply_patch::<ToneParams>(dev, stage, patch),
        VIBRANCE => apply_patch::<VibranceParams>(dev, stage, patch),
        _ => apply_patch::<PresenceParams>(dev, stage, patch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        view: &'static str,
        selected: Option<i64>,
        rated: Vec<(Vec<i64>, Option<i64>)>,
        develop: bool,
        opened_export: bool,
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
        })
        .unwrap();
        let (dead, _) = mpsc::channel();
        tx.send(Request {
            id: "ui.inspect".into(),
            params: Value::Null,
            reply: dead,
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
    fn merge_patch_refuses_unknown_fields_wrong_types_and_out_of_range_numbers() {
        let cur = obj(json!({"stops": 0.0}));
        let ok = merge_patch(EXPOSURE, cur.clone(), &obj(json!({"stops": 2.5}))).unwrap();
        assert_eq!(ok["stops"], 2.5);
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
            assert!(
                merge_patch(EXPOSURE, cur.clone(), &obj(bad.clone())).is_err(),
                "{bad}"
            );
        }
        // The documented limits are inclusive.
        assert!(merge_patch(EXPOSURE, cur.clone(), &obj(json!({"stops": 5.0}))).is_ok());
        assert!(merge_patch(
            TONE,
            obj(json!({"contrast": 0.0})),
            &obj(json!({"contrast": 1.0}))
        )
        .is_ok());
        assert!(merge_patch(
            TONE,
            obj(json!({"contrast": 0.0})),
            &obj(json!({"contrast": 1.5}))
        )
        .is_err());
    }

    #[test]
    fn white_balance_temperature_can_be_set_and_cleared_although_as_shot_omits_it() {
        let as_shot = obj(json!({"tint": 0.0})); // `temp_k` is not serialized while as-shot
        let set = merge_patch(WB, as_shot, &obj(json!({"temp_k": 5500.0}))).unwrap();
        assert_eq!(set["temp_k"], 5500.0);
        let cleared = merge_patch(WB, set, &obj(json!({"temp_k": null}))).unwrap();
        assert!(!cleared.contains_key("temp_k"), "null returns to as-shot");
        assert!(merge_patch(WB, cleared.clone(), &obj(json!({"temp_k": 100.0}))).is_err());
        assert!(merge_patch(WB, cleared.clone(), &obj(json!({"tint": 200.0}))).is_err());
        // `null` clears only optional fields.
        assert!(merge_patch(WB, cleared, &obj(json!({"tint": null}))).is_err());
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
}
