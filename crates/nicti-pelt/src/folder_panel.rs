//! The Library's folder panel (#303): registered roots grouped under the drive they live on, with
//! drag-a-folder-onto-a-drive-or-folder to start a verified move (#26, [`MoveJob`]).
//!
//! The shell still registers every root under one placeholder volume (see `app.rs`), so the
//! "drive" a root belongs to is derived from its path ([`drive_of`]), not from the catalog's
//! `volume` table; offline-volume handling (ADR-0071) arrives when real volume identity does.
//! Everything except [`show`] is pure and unit-tested.
//!
//! [`MoveJob`]: nicti_lair::pounce_jobs::MoveJob

use std::path::{Path, PathBuf};

use nicti_lair::carry::Resolution;
use nicti_lair::{MoveState, Root, RootMove};

use crate::archive_drives::ArchiveDrives;
use crate::pending::Pending;

/// A move the user asked for by dropping `root_id` on a drive or another folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRequest {
    pub root_id: i64,
    /// The *parent* folder to move the root into (what `MoveJob::new` takes).
    pub dest_parent: PathBuf,
}

/// What the user clicked this frame (#368). A click on a folder filters the Library grid to it; a
/// click on the drive's "Import here" button opens the native folder picker on that drive.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PanelOutput {
    pub drop: Option<DropRequest>,
    /// `Some(Some(id))` = filter to that root, `Some(None)` = clear the filter (clicked the
    /// already-selected root again).
    pub select_root: Option<Option<i64>>,
    pub import_from: Option<String>,
    /// The root whose "Verify folder" context-menu entry was clicked (#386).
    pub verify: Option<i64>,
    /// A stuck journal row's resolution button that was clicked (#334): `(journal id, choice)`.
    pub resolve: Option<(i64, Resolution)>,
}

/// The root filter a click on `clicked` produces: toggles off when it is already `selected`.
pub fn toggle_root(selected: Option<i64>, clicked: i64) -> Option<i64> {
    (selected != Some(clicked)).then_some(clicked)
}

/// One drive node and the registered roots that live on it.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveNode {
    /// The drive's mount path: `D:\`, `/mnt/d`, `/Volumes/Archive`, or `/`.
    pub path: String,
    pub roots: Vec<Root>,
}

/// The drive/mount a path lives on: a Windows drive letter, the first component under a
/// conventional Unix mount parent (`/mnt`, `/media/<user>`, `/Volumes`), else `/`.
pub fn drive_of(path: &str) -> String {
    // `\\?\D:\x` (verbatim) is the same drive as `D:\x`; `\\?\UNC\srv\share` is a UNC share.
    let path = match path.strip_prefix(r"\\?\UNC\") {
        Some(rest) => return unc_share(rest),
        None => path.strip_prefix(r"\\?\").unwrap_or(path),
    };
    if let Some(rest) = path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//")) {
        return unc_share(rest);
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return format!("{}:\\", (bytes[0] as char).to_ascii_uppercase());
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        ["mnt", d, ..] | ["Volumes", d, ..] => format!("/{}/{d}", parts[0]),
        ["media", u, d, ..] => format!("/media/{u}/{d}"),
        _ => "/".to_string(),
    }
}

/// `\\server\share` for the part of a UNC path after its leading `\\`.
fn unc_share(rest: &str) -> String {
    let mut it = rest.split(['\\', '/']).filter(|p| !p.is_empty());
    match (it.next(), it.next()) {
        (Some(server), Some(share)) => format!(r"\\{server}\{share}"),
        (Some(server), None) => format!(r"\\{server}"),
        _ => "/".to_string(),
    }
}

/// Groups `roots` by drive and makes sure every drive in `extra_drives` (mounted but possibly
/// holding no root yet) is present so it can still receive a drop. Archived roots are skipped.
/// Sorted by drive path, roots by path.
pub fn build_tree(roots: &[Root], extra_drives: &[String]) -> Vec<DriveNode> {
    let mut nodes: Vec<DriveNode> = Vec::new();
    let node_for = |nodes: &mut Vec<DriveNode>, drive: String| -> usize {
        match nodes.iter().position(|n| n.path == drive) {
            Some(i) => i,
            None => {
                nodes.push(DriveNode {
                    path: drive,
                    roots: Vec::new(),
                });
                nodes.len() - 1
            }
        }
    };
    for d in extra_drives {
        node_for(&mut nodes, d.clone());
    }
    for root in roots {
        let i = node_for(&mut nodes, drive_of(&root.path));
        nodes[i].roots.push(root.clone());
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    for n in &mut nodes {
        n.roots.sort_by(|a, b| a.path.cmp(&b.path));
    }
    nodes
}

/// Drives currently mounted, best effort -- only used so an empty drive can be a drop target.
pub fn mounted_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|c| format!("{c}:\\"))
            .filter(|d| Path::new(d).exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        let mut out = Vec::new();
        for parent in ["/mnt", "/Volumes"] {
            if let Ok(rd) = std::fs::read_dir(parent) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        out.push(e.path().to_string_lossy().into_owned());
                    }
                }
            }
        }
        out
    }
}

/// How long a scan of the catalog/filesystem is reused: roots, journal rows and mounted drives
/// are re-read on this cadence, not per frame. Both reads run on worker threads (#332) -- the
/// drive probe can stall for seconds on a dead network drive, and the catalog calls contend with
/// import/sync/move jobs for the connection mutex -- so the panel keeps drawing the last result
/// until the new one lands.
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

type CatalogRead = (Vec<Root>, Vec<RootMove>);

/// One periodically re-read value: at most one worker in flight, so a read that hangs costs one
/// stuck thread rather than one per [`CACHE_TTL`].
struct Source<T> {
    job: Option<Pending<T>>,
    /// When the last read finished (or died).
    at: Option<std::time::Instant>,
    /// Re-read as soon as no worker is in flight, regardless of age. Stays set when it is raised
    /// while a read is running, because that read may predate whatever raised it.
    stale: bool,
    /// A result has arrived at least once.
    loaded: bool,
}

impl<T> Default for Source<T> {
    fn default() -> Self {
        Self {
            job: None,
            at: None,
            stale: false,
            loaded: false,
        }
    }
}

impl<T: Send + 'static> Source<T> {
    /// Collects a finished read and starts the next one when due. Returns the new value, if one
    /// landed this call.
    fn step(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let mut landed = None;
        if let Some(job) = &self.job {
            match job.poll() {
                Ok(None) => return None,
                Ok(Some(v)) => {
                    self.loaded = true;
                    landed = Some(v);
                }
                // The worker panicked: keep the old value and retry on the normal cadence.
                Err(_) => {}
            }
            self.job = None;
            self.at = Some(std::time::Instant::now());
        }
        if self.stale || self.at.is_none_or(|t| t.elapsed() >= CACHE_TTL) {
            self.stale = false;
            self.job = Some(Pending::spawn(ctx, work));
        }
        landed
    }
}

pub struct Cache {
    was_busy: bool,
    catalog: Source<CatalogRead>,
    mounted: Source<Vec<String>>,
    probe: fn() -> Vec<String>,
    pub roots: Vec<Root>,
    pub drives: Vec<String>,
    pub open_moves: Vec<RootMove>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_probe(mounted_drives)
    }
}

impl Cache {
    fn with_probe(probe: fn() -> Vec<String>) -> Self {
        Self {
            was_busy: false,
            catalog: Source::default(),
            mounted: Source::default(),
            probe,
            roots: Vec::new(),
            drives: Vec::new(),
            open_moves: Vec::new(),
        }
    }

    /// Marks the cached catalog scan out of date so it is re-read on the next frame (a stuck move
    /// was just settled). The old value keeps showing until the new one lands.
    pub fn invalidate(&mut self) {
        self.catalog.stale = true;
    }

    /// True once the first catalog read has landed -- before that `roots` is empty because
    /// nothing was read, not because there are no folders.
    pub fn loaded(&self) -> bool {
        self.catalog.loaded
    }

    /// Swaps in finished reads and starts the next ones when due; never blocks. The catalog read
    /// runs again right away when `busy` (an import/sync/move is running) just changed -- a
    /// finishing move has just re-pointed a root. The drive probe is independent, so a hung drive
    /// can't hold the catalog read back.
    pub fn refresh(
        &mut self,
        ctx: &egui::Context,
        busy: bool,
        read: impl FnOnce() -> CatalogRead + Send + 'static,
    ) {
        if std::mem::replace(&mut self.was_busy, busy) != busy {
            self.invalidate();
        }
        if let Some((roots, open_moves)) = self.catalog.step(ctx, read) {
            self.roots = roots;
            self.open_moves = open_moves;
        }
        if let Some(drives) = self.mounted.step(ctx, self.probe) {
            self.drives = drives;
        }
    }
}

/// True when moving `dragged` into `dest_parent` is obviously pointless or impossible, so the UI
/// doesn't offer the drop at all: onto itself, into its own subtree, or into the folder it's
/// already directly inside. (`Carry` re-checks and refuses the rest -- this is only the cheap
/// pre-filter for drop highlighting.)
pub fn is_noop_or_cyclic(dragged: &Path, dest_parent: &Path) -> bool {
    dest_parent.starts_with(dragged) || dragged.parent() == Some(dest_parent)
}

/// One interrupted move for the panel's warning block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    /// The `root_move` journal row.
    pub move_id: i64,
    pub text: String,
    /// `copying`/`renaming` rows can be settled by the user (#334); a `committed` row is only
    /// waiting for its source cleanup, which finishes on its own at the next start.
    pub resolvable: bool,
}

/// What an interrupted move needs the user to look at, for the panel's warning block.
/// (Leftovers from a *finished* move aren't journal rows -- they're in the move summary.)
pub fn attention_items(open: &[RootMove], move_running: bool) -> Vec<AttentionItem> {
    // While our own MoveJob runs its journal row is legitimately open.
    if move_running {
        return Vec::new();
    }
    open.iter()
        .map(|m| {
            let what = match m.state {
                MoveState::Copying => "copy interrupted",
                MoveState::Renaming => "rename in doubt",
                MoveState::Committed => "source cleanup pending",
            };
            AttentionItem {
                move_id: m.id,
                text: format!("{} -> {} ({what})", m.src_path, m.dest_path),
                resolvable: m.state != MoveState::Committed,
            }
        })
        .collect()
}

fn folder_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

/// The buttons under a stuck move (#334). None of them deletes or copies a file.
const RESOLUTIONS: [(&str, Resolution, &str); 3] = [
    (
        "Keep source",
        Resolution::KeepSource,
        "The catalog stays on the original folder. The destination folder is left on disk.",
    ),
    (
        "Keep destination",
        Resolution::KeepDestination,
        "Point the catalog at the destination folder. The original folder is left on disk.",
    ),
    (
        "Abandon",
        Resolution::Abandon,
        "Only clear this warning; the catalog and both folders stay exactly as they are.",
    ),
];

/// Draws the tree. Reports the drop the user made this frame, if any (only drives accept drops),
/// and any folder/drive click (#368). `moving` disables dropping (a scan or another move is
/// running -- same guard as the text-field path); `selected_root` is the grid's current filter.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    tree: &[DriveNode],
    attention: &[AttentionItem],
    status: Option<&str>,
    moving: bool,
    archive: &ArchiveDrives,
    set_archive: &mut Option<(String, bool)>,
    selected_root: Option<i64>,
    verify_enabled: bool,
) -> PanelOutput {
    let mut out = PanelOutput::default();
    if tree.is_empty() {
        ui.weak("No folders yet -- import one.");
    }
    for drive in tree {
        let is_archive = archive.is_location(&drive.path);
        let badge = if is_archive { " \u{1F5C4} archive" } else { "" };
        let (_, dropped) = ui.dnd_drop_zone::<i64, ()>(egui::Frame::group(ui.style()), |ui| {
            egui::CollapsingHeader::new(format!("\u{1F4BF} {}{badge}", drive.path))
                .id_salt(("folder_panel_drive", &drive.path))
                .default_open(true)
                .show(ui, |ui| {
                    let mut flag = is_archive;
                    if ui
                        .checkbox(&mut flag, "Archive drive")
                        .on_hover_text(
                            "Folders moved here keep their thumbnails as .thumb.jpg files \
                             next to the photos instead of in the catalog.",
                        )
                        .changed()
                    {
                        *set_archive = Some((drive.path.clone(), flag));
                    }
                    // Not for the `/` catch-all "drive": seeding it would make one stray Import
                    // click scan the whole filesystem.
                    if drive.path != "/"
                        && ui
                            .small_button("Import here\u{2026}")
                            .on_hover_text("Pick a folder on this drive to import.")
                            .clicked()
                    {
                        out.import_from = Some(drive.path.clone());
                    }
                    for root in &drive.roots {
                        let row = show_root(
                            ui,
                            root,
                            moving,
                            selected_root == Some(root.id),
                            verify_enabled,
                        );
                        if row.clicked {
                            out.select_root = Some(toggle_root(selected_root, root.id));
                        }
                        if row.verify {
                            out.verify = Some(root.id);
                        }
                    }
                });
        });
        if let (Some(root_id), false) = (dropped, moving) {
            if let Some(dragged) = find_root(tree, *root_id) {
                let dest = PathBuf::from(&drive.path);
                if !is_noop_or_cyclic(Path::new(&dragged.path), &dest) {
                    out.drop = Some(DropRequest {
                        root_id: *root_id,
                        dest_parent: dest,
                    });
                }
            }
        }
    }
    if !attention.is_empty() {
        ui.separator();
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "Interrupted moves need attention (check both folders):",
        );
        for item in attention {
            ui.add_space(4.0);
            ui.label(&item.text);
            if !item.resolvable {
                continue;
            }
            ui.horizontal_wrapped(|ui| {
                for (label, how, hint) in RESOLUTIONS {
                    if ui.small_button(label).on_hover_text(hint).clicked() {
                        out.resolve = Some((item.move_id, how));
                    }
                }
            });
        }
    }
    if let Some(status) = status {
        ui.separator();
        ui.label(status);
    }
    out
}

fn find_root(tree: &[DriveNode], id: i64) -> Option<&Root> {
    tree.iter().flat_map(|d| &d.roots).find(|r| r.id == id)
}

/// What one folder row reported this frame.
struct RowOutput {
    /// Clicked: selects the folder as the grid filter.
    clicked: bool,
    /// "Verify folder" was picked from the row's context menu (#386).
    verify: bool,
}

/// One folder row: click selects it as the grid filter, drag starts a move, right-click opens a
/// menu with "Verify folder" (disabled while `verify_enabled` is false).
/// Deliberately not a drop target: `Carry` refuses any destination inside another registered
/// root (overlapping roots), so "move into this folder" can only fail.
fn show_root(
    ui: &mut egui::Ui,
    root: &Root,
    moving: bool,
    selected: bool,
    verify_enabled: bool,
) -> RowOutput {
    let id = egui::Id::new(("folder_panel_root", root.id));
    let label = folder_name(&root.path);
    let icon = if root.archived {
        "\u{1F5C4}"
    } else {
        "\u{1F4C1}"
    };
    let text = format!("{icon} {label}");
    let mut label_clicked = false;
    let response = if moving {
        ui.selectable_label(selected, text)
    } else {
        // The drag source only senses drags and would swallow the press, so widen its response
        // to also sense clicks (a press-release without movement is a click, not a drag).
        let r = ui
            .dnd_drag_source(id, root.id, |ui| {
                // Mouse clicks land on the widened drag widget below; the label still gets
                // keyboard activation (Tab + Enter/Space), so honour that too.
                label_clicked = ui.selectable_label(selected, text).clicked();
            })
            .response;
        // `Response::interact` is undefined on `dnd_drag_source`'s unioned response, so
        // register the click sense on the drag widget's id and rect directly (senses merge).
        ui.interact(r.rect, id, egui::Sense::click())
    };
    let clicked = response.clicked() || label_clicked;
    let mut verify = false;
    response.on_hover_text(&root.path).context_menu(|ui| {
        let item = ui
            .add_enabled(verify_enabled, egui::Button::new("Verify folder"))
            .on_hover_text(
                "Re-read every photo here and compare it to the checksum recorded \
                 when it was copied.",
            );
        if item.clicked() {
            verify = true;
            ui.close();
        }
    });
    RowOutput { clicked, verify }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(id: i64, path: &str) -> Root {
        Root {
            id,
            volume_id: 1,
            path: path.into(),
            archived: false,
        }
    }

    #[test]
    fn drive_of_windows_and_unix() {
        assert_eq!(drive_of(r"d:\Photos\2026"), r"D:\");
        assert_eq!(drive_of("C:/Users/x"), r"C:\");
        assert_eq!(drive_of("/mnt/e/RAW/a"), "/mnt/e");
        assert_eq!(drive_of("/Volumes/Archive/x"), "/Volumes/Archive");
        assert_eq!(drive_of("/media/jo/usb/x"), "/media/jo/usb");
        assert_eq!(drive_of("/home/jo/pics"), "/");
    }

    #[test]
    fn tree_groups_by_drive_keeps_empty_extra_drives_and_shows_archived() {
        let mut archived = root(4, r"D:\old");
        archived.archived = true;
        let roots = [
            root(1, r"D:\b"),
            root(2, r"D:\a"),
            root(3, r"C:\x"),
            archived,
        ];
        let tree = build_tree(&roots, &[r"E:\".to_string(), r"D:\".to_string()]);
        let paths: Vec<_> = tree.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, [r"C:\", r"D:\", r"E:\"]);
        let d: Vec<_> = tree[1].roots.iter().map(|r| r.id).collect();
        assert_eq!(d, [2, 1, 4], "sorted by path, archived roots still shown");
        assert!(tree[2].roots.is_empty());
    }

    #[test]
    fn drive_of_unc_and_verbatim() {
        assert_eq!(drive_of(r"\\srv\share\x\y"), r"\\srv\share");
        assert_eq!(drive_of(r"\\?\UNC\srv\share\x"), r"\\srv\share");
        assert_eq!(drive_of(r"\\?\d:\x"), r"D:\");
        assert_eq!(drive_of("//srv/share/x/y"), r"\\srv\share");
    }

    #[test]
    fn noop_and_cyclic_drops_are_filtered() {
        let p = Path::new("/mnt/d/RAW/2026");
        assert!(
            is_noop_or_cyclic(p, Path::new("/mnt/d/RAW")),
            "already there"
        );
        assert!(is_noop_or_cyclic(p, p), "onto itself");
        assert!(
            is_noop_or_cyclic(p, Path::new("/mnt/d/RAW/2026/sub")),
            "into itself"
        );
        assert!(!is_noop_or_cyclic(p, Path::new("/mnt/e")));
        assert!(
            !is_noop_or_cyclic(Path::new("/mnt/d/RAW/20"), Path::new("/mnt/d/RAW/2026")),
            "sibling sharing a name prefix is not a descendant"
        );
    }

    #[test]
    fn attention_hidden_while_our_move_runs() {
        let m = RootMove {
            id: 1,
            root_id: 1,
            src_path: "/a".into(),
            dest_path: "/b".into(),
            state: MoveState::Committed,
        };
        assert!(attention_items(std::slice::from_ref(&m), true).is_empty());
        let items = attention_items(&[m], false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "/a -> /b (source cleanup pending)");
        assert!(!items[0].resolvable, "cleanup settles itself");
    }

    #[test]
    fn clicking_a_root_toggles_the_filter() {
        assert_eq!(toggle_root(None, 3), Some(3));
        assert_eq!(toggle_root(Some(2), 3), Some(3), "switches roots");
        assert_eq!(toggle_root(Some(3), 3), None, "second click clears");
    }

    /// Draws the real panel under the headless harness and returns what the last click produced.
    fn click_in_panel(label: &str, selected: Option<i64>) -> PanelOutput {
        use egui_kittest::kittest::Queryable;
        let tree = build_tree(&[root(7, "/mnt/d/2026")], &[]);
        let state = (PanelOutput::default(), tree);
        // A plain kittest harness, not `swat::harness`: the panel uses only egui's default fonts,
        // and installing the app theme here perturbs `library_grid_snapshot`'s pixels when the
        // suite runs in parallel.
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, (out, tree): &mut (PanelOutput, Vec<DriveNode>)| {
                let archive = ArchiveDrives::with_locations(&[]);
                let mut set_archive = None;
                let got = show(
                    ui,
                    tree,
                    &[],
                    None,
                    false,
                    &archive,
                    &mut set_archive,
                    selected,
                    true,
                );
                if got != PanelOutput::default() {
                    *out = got;
                }
            },
            state,
        );
        h.get_by_label(label).click();
        h.run();
        h.state().0.clone()
    }

    #[test]
    fn clicking_a_folder_row_selects_it_and_a_drive_button_requests_the_import_picker() {
        let out = click_in_panel("\u{1F4C1} 2026", None);
        assert_eq!(out.select_root, Some(Some(7)));
        let out = click_in_panel("\u{1F4C1} 2026", Some(7));
        assert_eq!(out.select_root, Some(None), "second click clears");
        let out = click_in_panel("Import here\u{2026}", None);
        assert_eq!(out.import_from.as_deref(), Some("/mnt/d"));
        assert_eq!(out.select_root, None);
    }

    #[test]
    fn the_folder_context_menu_requests_a_verify() {
        use egui_kittest::kittest::Queryable;
        let tree = build_tree(&[root(7, "/mnt/d/2026")], &[]);
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, (out, tree): &mut (PanelOutput, Vec<DriveNode>)| {
                let archive = ArchiveDrives::with_locations(&[]);
                let mut set_archive = None;
                let got = show(
                    ui,
                    tree,
                    &[],
                    None,
                    false,
                    &archive,
                    &mut set_archive,
                    None,
                    true,
                );
                if got != PanelOutput::default() {
                    *out = got;
                }
            },
            (PanelOutput::default(), tree),
        );
        h.get_by_label("\u{1F4C1} 2026").click_secondary();
        h.run();
        h.get_by_label("Verify folder").click();
        h.run();
        let out = h.state().0.clone();
        assert_eq!(out.verify, Some(7));
        assert_eq!(out.select_root, None, "the menu pick is not a folder click");
    }

    #[test]
    fn a_stuck_move_offers_resolution_buttons_but_a_pending_cleanup_does_not() {
        use egui_kittest::kittest::Queryable;
        let stuck = RootMove {
            id: 5,
            root_id: 7,
            src_path: "/a".into(),
            dest_path: "/b".into(),
            state: MoveState::Renaming,
        };
        let cleanup = RootMove {
            id: 6,
            state: MoveState::Committed,
            ..stuck.clone()
        };
        let items = attention_items(&[stuck, cleanup], false);
        assert_eq!(
            items.iter().map(|i| i.resolvable).collect::<Vec<_>>(),
            [true, false]
        );
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, (out, items): &mut (PanelOutput, Vec<AttentionItem>)| {
                let archive = ArchiveDrives::with_locations(&[]);
                let mut set_archive = None;
                let got = show(
                    ui,
                    &[],
                    items,
                    None,
                    false,
                    &archive,
                    &mut set_archive,
                    None,
                    true,
                );
                if got != PanelOutput::default() {
                    *out = got;
                }
            },
            (PanelOutput::default(), items),
        );
        h.run();
        assert_eq!(
            h.query_all_by_label("Keep destination").count(),
            1,
            "only the resolvable row gets buttons"
        );
        h.get_by_label("Keep destination").click();
        h.run();
        assert_eq!(h.state().0.resolve, Some((5, Resolution::KeepDestination)));
    }

    #[test]
    fn folder_name_handles_trailing_separators() {
        assert_eq!(folder_name(r"D:\Photos\2026\"), "2026");
        assert_eq!(folder_name("/mnt/d"), "d");
    }

    // ---- Cache (#332): reads run on workers, never on the calling thread ----

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn no_drives() -> Vec<String> {
        Vec::new()
    }

    static RELEASE_PROBE: AtomicBool = AtomicBool::new(false);

    fn hung_probe() -> Vec<String> {
        while !RELEASE_PROBE.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Vec::new()
    }

    fn one_root(id: i64) -> CatalogRead {
        (vec![root(id, "/mnt/d/x")], Vec::new())
    }

    /// Calls `refresh` until `done` holds or a generous deadline passes (never a fixed wait).
    fn pump(
        cache: &mut Cache,
        ctx: &egui::Context,
        read: impl Fn() -> CatalogRead + Clone + Send + 'static,
        done: impl Fn(&Cache) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            cache.refresh(ctx, false, read.clone());
            if done(cache) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn refresh_returns_before_a_slow_read_finishes() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_probe(no_drives);
        let (release, gate) = mpsc::channel::<()>();
        let gate = std::sync::Arc::new(std::sync::Mutex::new(gate));
        let read = move || {
            let _ = gate.lock().unwrap().recv();
            one_root(1)
        };
        cache.refresh(&ctx, false, read);
        assert!(!cache.loaded(), "the read is still blocked");
        assert!(cache.roots.is_empty());
        release.send(()).unwrap();
        let done = pump(&mut cache, &ctx, || one_root(1), Cache::loaded);
        assert!(done, "the result lands once the read is released");
        assert_eq!(cache.roots.len(), 1);
    }

    #[test]
    fn a_hung_drive_probe_does_not_hold_back_the_catalog() {
        RELEASE_PROBE.store(false, Ordering::Relaxed);
        let ctx = egui::Context::default();
        let mut cache = Cache::with_probe(hung_probe);
        let done = pump(&mut cache, &ctx, || one_root(2), Cache::loaded);
        RELEASE_PROBE.store(true, Ordering::Relaxed);
        assert!(done, "roots arrive while the probe is still hung");
        assert!(cache.drives.is_empty());
    }

    #[test]
    fn no_second_read_within_the_ttl() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_probe(no_drives);
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let read = move || {
            c.fetch_add(1, Ordering::SeqCst);
            one_root(1)
        };
        assert!(pump(&mut cache, &ctx, read.clone(), Cache::loaded));
        for _ in 0..20 {
            cache.refresh(&ctx, false, read.clone());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidating_during_a_read_rereads_after_it_lands() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_probe(no_drives);
        let (release, gate) = mpsc::channel::<()>();
        let gate = std::sync::Arc::new(std::sync::Mutex::new(gate));
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        // The first read is held open; the settle happens while it runs.
        cache.refresh(&ctx, false, move || {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = gate.lock().unwrap().recv();
            one_root(1)
        });
        cache.invalidate();
        release.send(()).unwrap();
        let c = calls.clone();
        let second = move || {
            c.fetch_add(1, Ordering::SeqCst);
            one_root(2)
        };
        // The step that collects the first (possibly pre-settle) result must already have started
        // the next read -- not left it for the TTL.
        assert!(pump(&mut cache, &ctx, second.clone(), Cache::loaded));
        assert!(
            cache.catalog.job.is_some(),
            "a read raised stale mid-flight is re-run right after it lands"
        );
        let done = pump(&mut cache, &ctx, second, |c| {
            c.roots.first().is_some_and(|r| r.id == 2)
        });
        assert!(done, "the stale first result is superseded by a fresh read");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_busy_edge_rereads_immediately() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_probe(no_drives);
        assert!(pump(&mut cache, &ctx, || one_root(1), Cache::loaded));
        // Within the TTL, but `busy` flipped: a finishing move just re-pointed a root.
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.roots.first().is_none_or(|r| r.id != 9) && Instant::now() < deadline {
            cache.refresh(&ctx, true, || one_root(9));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(cache.roots[0].id, 9);
    }
}
