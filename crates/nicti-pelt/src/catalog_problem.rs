//! The launch gate: opens the catalog before the app is built, and when that fails shows a
//! blocking window (path, error, **Try Again** / **Choose Another Catalog...** / **Quit**)
//! instead of an app half-running on nothing.
//!
//! [`Problem`] is the pure state machine (tested below); [`Launcher`] is the thin eframe shell
//! that owns it until a catalog opens, then hands every frame to the real [`PeltApp`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use nicti_lair::SqliteCatalog;

use crate::app::PeltApp;
use crate::catalog;

/// What the user clicked in the problem window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    TryAgain,
    ChooseAnother,
    Quit,
}

/// Where the problem window stands after an [`Action`] or a finished picker.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// Still failing (or waiting on the picker); keep showing the window.
    Stay,
    /// A catalog opened at `.0`.
    Opened(PathBuf, T),
    Quit,
}

/// A catalog path that failed to open, and why.
#[derive(Debug, PartialEq, Eq)]
pub struct Problem {
    pub path: PathBuf,
    pub error: String,
    /// A file dialog is open, so Choose Another is disabled until it closes.
    pub picking: bool,
}

impl Problem {
    pub fn new(path: PathBuf, error: String) -> Self {
        Self {
            path,
            error,
            picking: false,
        }
    }

    /// Applies a button press. `open` is tried against the current path (Try Again); Choose
    /// Another only flips `picking`, the dialog's result arrives through [`Problem::picked`].
    pub fn act<T>(
        &mut self,
        action: Action,
        open: impl FnOnce(&Path) -> Result<T, String>,
    ) -> Outcome<T> {
        match action {
            Action::Quit => Outcome::Quit,
            // The picker's result will replace the path, so neither is meaningful while it is open.
            Action::ChooseAnother | Action::TryAgain if self.picking => Outcome::Stay,
            Action::ChooseAnother => {
                self.picking = true;
                Outcome::Stay
            }
            Action::TryAgain => self.try_open(open),
        }
    }

    /// The picker closed: `None` is a cancel (back to the window unchanged), `Some` is opened
    /// at once, and replaces the path and error if that fails too.
    pub fn picked<T>(
        &mut self,
        choice: Option<PathBuf>,
        open: impl FnOnce(&Path) -> Result<T, String>,
    ) -> Outcome<T> {
        self.picking = false;
        let Some(path) = choice else {
            return Outcome::Stay;
        };
        self.path = path;
        self.try_open(open)
    }

    fn try_open<T>(&mut self, open: impl FnOnce(&Path) -> Result<T, String>) -> Outcome<T> {
        match open(&self.path) {
            Ok(v) => Outcome::Opened(self.path.clone(), v),
            Err(e) => {
                self.error = e;
                Outcome::Stay
            }
        }
    }
}

fn open_catalog(path: &Path) -> Result<SqliteCatalog, String> {
    catalog::open(path).map_err(|e| e.to_string())
}

/// For a file the user picked: refuses anything that isn't already a Nicti catalog, because
/// opening migrates (and switches to WAL) whatever SQLite file it is given.
fn open_picked(path: &Path) -> Result<SqliteCatalog, String> {
    if !SqliteCatalog::is_existing_catalog(path) {
        return Err("That file isn't a Nicti catalog.".into());
    }
    open_catalog(path)
}

/// Asks for a catalog file on its own thread so the window keeps repainting.
fn spawn_picker(ctx: &egui::Context, start: &Path) -> Option<Receiver<Option<PathBuf>>> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let start = start.parent().map(Path::to_path_buf);
    std::thread::Builder::new()
        .name("catalog-dialog".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Choose a Nicti catalog")
                .add_filter("Nicti catalog", &["sqlite", "db"]);
            if let Some(dir) = start.filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_file());
            ctx.request_repaint();
        })
        .ok()
        .map(|_| rx)
}

enum Stage {
    Problem {
        problem: Problem,
        picker: Option<Receiver<Option<PathBuf>>>,
    },
    Running(Box<PeltApp>),
}

/// The root eframe app: the problem window until a catalog opens, then [`PeltApp`].
pub struct Launcher {
    stage: Stage,
    version: String,
    ctx: egui::Context,
    render_state: egui_wgpu::RenderState,
    /// The opt-in agent control channel (#543); started on the app that opens a catalog.
    control: Option<crate::control::ControlOptions>,
}

impl Launcher {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        version: String,
        control: Option<crate::control::ControlOptions>,
    ) -> Self {
        let render_state = cc
            .wgpu_render_state
            .clone()
            .expect("nicti-pelt requires the wgpu backend (eframe::Renderer::Wgpu)");
        let mut launcher = Self {
            // Replaced just below; a placeholder keeps construction in one place.
            stage: Stage::Problem {
                problem: Problem::new(PathBuf::new(), String::new()),
                picker: None,
            },
            version,
            ctx: cc.egui_ctx.clone(),
            render_state,
            control,
        };
        let path = catalog::resolve_path();
        launcher.stage = match open_catalog(&path) {
            Ok(store) => Stage::Running(Box::new(launcher.build(path, store))),
            Err(e) => Stage::Problem {
                problem: Problem::new(path, e),
                picker: None,
            },
        };
        launcher
    }

    fn build(&self, path: PathBuf, store: SqliteCatalog) -> PeltApp {
        let mut app = PeltApp::new(
            &self.ctx,
            &self.render_state,
            self.version.clone(),
            path,
            store,
        );
        if let Some(opts) = self.control {
            app.enable_control(opts, &self.ctx);
        }
        app
    }

    fn apply(&mut self, ctx: &egui::Context, outcome: Outcome<SqliteCatalog>) {
        match outcome {
            Outcome::Stay => {}
            Outcome::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Outcome::Opened(path, store) => {
                self.stage = Stage::Running(Box::new(self.build(path, store)));
                ctx.request_repaint();
            }
        }
    }
}

impl eframe::App for Launcher {
    fn on_exit(&mut self) {
        if let Stage::Running(app) = &mut self.stage {
            app.on_exit();
        }
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Stage::Running(app) = &mut self.stage {
            app.logic(ctx, frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let Stage::Problem { problem, picker } = &mut self.stage else {
            if let Stage::Running(app) = &mut self.stage {
                app.ui(ui, frame);
            }
            return;
        };
        let ctx = ui.ctx().clone();

        let mut outcome = Outcome::Stay;
        if let Some(rx) = picker {
            match rx.try_recv() {
                Ok(choice) => {
                    *picker = None;
                    outcome = problem.picked(choice, open_picked);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    *picker = None;
                    outcome = problem.picked(None, open_catalog);
                }
            }
        }

        let mut action = None;
        egui::Window::new("Couldn't open the catalog")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(&ctx, |ui| {
                ui.label("Nicti couldn't open this catalog:");
                ui.monospace(problem.path.display().to_string());
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::RED, &problem.error);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!problem.picking, egui::Button::new("Try Again"))
                        .clicked()
                    {
                        action = Some(Action::TryAgain);
                    }
                    if ui
                        .add_enabled(
                            !problem.picking,
                            egui::Button::new("Choose Another Catalog..."),
                        )
                        .clicked()
                    {
                        action = Some(Action::ChooseAnother);
                    }
                    if ui.button("Quit").clicked() {
                        action = Some(Action::Quit);
                    }
                });
            });

        // A catalog the picker just opened wins over a click on the same frame.
        if let (Outcome::Stay, Some(action)) = (&outcome, action) {
            outcome = problem.act(action, open_catalog);
            if action == Action::ChooseAnother {
                *picker = spawn_picker(&ctx, &problem.path);
                // A dialog that failed to spawn must not leave the button disabled forever.
                problem.picking = picker.is_some();
            }
        }
        self.apply(&ctx, outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem() -> Problem {
        Problem::new("a.sqlite".into(), "locked".into())
    }

    #[test]
    fn retry_succeeds() {
        let mut p = problem();
        let out = p.act(Action::TryAgain, |path| {
            assert_eq!(path, Path::new("a.sqlite"));
            Ok(7)
        });
        assert_eq!(out, Outcome::Opened("a.sqlite".into(), 7));
    }

    #[test]
    fn retry_fails_again_keeps_path_and_updates_error() {
        let mut p = problem();
        let out: Outcome<()> = p.act(Action::TryAgain, |_| Err("still locked".into()));
        assert_eq!(out, Outcome::Stay);
        assert_eq!(p.path, Path::new("a.sqlite"));
        assert_eq!(p.error, "still locked");
    }

    #[test]
    fn choose_another_disables_button_until_picker_closes() {
        let mut p = problem();
        let out: Outcome<()> = p.act(Action::ChooseAnother, |_| panic!("must not open"));
        assert_eq!(out, Outcome::Stay);
        assert!(p.picking);
        let out = p.picked(Some("b.sqlite".into()), |_| Ok(1));
        assert_eq!(out, Outcome::Opened("b.sqlite".into(), 1));
        assert!(!p.picking);
    }

    #[test]
    fn picked_catalog_that_fails_replaces_path_and_error() {
        let mut p = problem();
        p.picking = true;
        let out: Outcome<()> = p.picked(Some("b.sqlite".into()), |_| Err("not a catalog".into()));
        assert_eq!(out, Outcome::Stay);
        assert_eq!(p.path, Path::new("b.sqlite"));
        assert_eq!(p.error, "not a catalog");
        assert!(!p.picking);
    }

    #[test]
    fn cancelled_picker_changes_nothing() {
        let mut p = problem();
        p.picking = true;
        let out: Outcome<()> = p.picked(None, |_| panic!("must not open"));
        assert_eq!(out, Outcome::Stay);
        assert_eq!(p, problem());
    }

    #[test]
    fn try_again_and_choose_are_ignored_while_the_picker_is_open() {
        let mut p = problem();
        p.picking = true;
        let out: Outcome<()> = p.act(Action::TryAgain, |_| panic!("must not open"));
        assert_eq!(out, Outcome::Stay);
        let out: Outcome<()> = p.act(Action::Quit, |_| panic!("must not open"));
        assert_eq!(out, Outcome::Quit);
    }

    #[test]
    fn quit_quits() {
        let mut p = problem();
        let out: Outcome<()> = p.act(Action::Quit, |_| panic!("must not open"));
        assert_eq!(out, Outcome::Quit);
    }
}
