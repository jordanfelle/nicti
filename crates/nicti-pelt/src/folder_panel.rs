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

use nicti_lair::{MoveState, Root, RootMove};

use crate::archive_drives::ArchiveDrives;

/// A move the user asked for by dropping `root_id` on a drive or another folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRequest {
    pub root_id: i64,
    /// The *parent* folder to move the root into (what `MoveJob::new` takes).
    pub dest_parent: PathBuf,
}

/// What the user clicked this frame (#368). A click on a folder filters the Library grid to it; a
/// click on the drive's "Import here" button seeds the Folder field with the drive path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PanelOutput {
    pub drop: Option<DropRequest>,
    /// `Some(Some(id))` = filter to that root, `Some(None)` = clear the filter (clicked the
    /// already-selected root again).
    pub select_root: Option<Option<i64>>,
    pub import_from: Option<String>,
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
/// are read on this cadence, not per frame (`mounted_drives` can stall on a dead network drive,
/// and the catalog calls contend with import/sync/move jobs for the connection mutex).
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Default)]
pub struct Cache {
    at: Option<std::time::Instant>,
    was_busy: bool,
    pub roots: Vec<Root>,
    pub drives: Vec<String>,
    pub open_moves: Vec<RootMove>,
}

impl Cache {
    /// Refreshes if stale, or right away when `busy` (an import/sync/move is running) just changed
    /// -- a finishing move has just re-pointed a root.
    pub fn refresh(&mut self, busy: bool, read: impl FnOnce() -> (Vec<Root>, Vec<RootMove>)) {
        let force = std::mem::replace(&mut self.was_busy, busy) != busy;
        if !force && self.at.is_some_and(|t| t.elapsed() < CACHE_TTL) {
            return;
        }
        (self.roots, self.open_moves) = read();
        self.drives = mounted_drives();
        self.at = Some(std::time::Instant::now());
    }
}

/// True when moving `dragged` into `dest_parent` is obviously pointless or impossible, so the UI
/// doesn't offer the drop at all: onto itself, into its own subtree, or into the folder it's
/// already directly inside. (`Carry` re-checks and refuses the rest -- this is only the cheap
/// pre-filter for drop highlighting.)
pub fn is_noop_or_cyclic(dragged: &Path, dest_parent: &Path) -> bool {
    dest_parent.starts_with(dragged) || dragged.parent() == Some(dest_parent)
}

/// What an interrupted move needs the user to look at, for the panel's warning block.
/// (Leftovers from a *finished* move aren't journal rows -- they're in the move summary.)
pub fn attention_lines(open: &[RootMove], move_running: bool) -> Vec<String> {
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
            format!("{} -> {} ({what})", m.src_path, m.dest_path)
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

/// Draws the tree. Reports the drop the user made this frame, if any (only drives accept drops),
/// and any folder/drive click (#368). `moving` disables dropping (a scan or another move is
/// running -- same guard as the text-field path); `selected_root` is the grid's current filter.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    tree: &[DriveNode],
    attention: &[String],
    status: Option<&str>,
    moving: bool,
    archive: &ArchiveDrives,
    set_archive: &mut Option<(String, bool)>,
    selected_root: Option<i64>,
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
                            .on_hover_text(
                                "Put this drive's path in the (empty) Folder field, then \
                                 append the folder to import.",
                            )
                            .clicked()
                    {
                        out.import_from = Some(drive.path.clone());
                    }
                    for root in &drive.roots {
                        if show_root(ui, root, moving, selected_root == Some(root.id)) {
                            out.select_root = Some(toggle_root(selected_root, root.id));
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
        for line in attention {
            ui.label(line);
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

/// One folder row: click selects it as the grid filter (returns true), drag starts a move.
/// Deliberately not a drop target: `Carry` refuses any destination inside another registered
/// root (overlapping roots), so "move into this folder" can only fail.
fn show_root(ui: &mut egui::Ui, root: &Root, moving: bool, selected: bool) -> bool {
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
        r.interact(egui::Sense::click())
    };
    let clicked = response.clicked() || label_clicked;
    response.on_hover_text(&root.path);
    clicked
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
    fn attention_lines_hidden_while_our_move_runs() {
        let m = RootMove {
            id: 1,
            root_id: 1,
            src_path: "/a".into(),
            dest_path: "/b".into(),
            state: MoveState::Committed,
        };
        assert!(attention_lines(std::slice::from_ref(&m), true).is_empty());
        let lines = attention_lines(&[m], false);
        assert_eq!(lines, ["/a -> /b (source cleanup pending)"]);
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
    fn clicking_a_folder_row_selects_it_and_a_drive_button_seeds_import() {
        let out = click_in_panel("\u{1F4C1} 2026", None);
        assert_eq!(out.select_root, Some(Some(7)));
        let out = click_in_panel("\u{1F4C1} 2026", Some(7));
        assert_eq!(out.select_root, Some(None), "second click clears");
        let out = click_in_panel("Import here\u{2026}", None);
        assert_eq!(out.import_from.as_deref(), Some("/mnt/d"));
        assert_eq!(out.select_root, None);
    }

    #[test]
    fn folder_name_handles_trailing_separators() {
        assert_eq!(folder_name(r"D:\Photos\2026\"), "2026");
        assert_eq!(folder_name("/mnt/d"), "d");
    }
}
