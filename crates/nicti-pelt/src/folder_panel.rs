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

/// A move the user asked for by dropping `root_id` on a drive or another folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRequest {
    pub root_id: i64,
    /// The *parent* folder to move the root into (what `MoveJob::new` takes).
    pub dest_parent: PathBuf,
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
    for root in roots.iter().filter(|r| !r.archived) {
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

/// True when moving `dragged` into `dest_parent` is obviously pointless or impossible, so the UI
/// doesn't offer the drop at all: onto itself, into its own subtree, or into the folder it's
/// already directly inside. (`Carry` re-checks and refuses the rest -- this is only the cheap
/// pre-filter for drop highlighting.)
pub fn is_noop_or_cyclic(dragged: &Path, dest_parent: &Path) -> bool {
    dest_parent.starts_with(dragged) || dragged.parent() == Some(dest_parent)
}

/// What a finished or interrupted move needs the user to look at, for the panel's warning block.
pub fn attention_lines(open: &[RootMove], moving: bool) -> Vec<String> {
    // While our own MoveJob runs its journal row is legitimately open.
    if moving {
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

/// Draws the tree. Returns the drop the user made this frame, if any. `moving` disables dropping
/// (a scan or another move is running -- same guard as the text-field path).
pub fn show(
    ui: &mut egui::Ui,
    tree: &[DriveNode],
    attention: &[String],
    status: Option<&str>,
    moving: bool,
) -> Option<DropRequest> {
    let mut request = None;
    if tree.is_empty() {
        ui.weak("No folders yet -- import one.");
    }
    for drive in tree {
        let (_, dropped) = ui.dnd_drop_zone::<i64, ()>(egui::Frame::group(ui.style()), |ui| {
            egui::CollapsingHeader::new(format!("\u{1F4BF} {}", drive.path))
                .id_salt(("folder_panel_drive", &drive.path))
                .default_open(true)
                .show(ui, |ui| {
                    for root in &drive.roots {
                        if let Some(r) = show_root(ui, root, moving) {
                            request = Some(r);
                        }
                    }
                });
        });
        // A drop on a folder row inside this drive was already claimed by that row.
        if let (Some(root_id), false, true) = (dropped, moving, request.is_none()) {
            if let Some(dragged) = find_root(tree, *root_id) {
                let dest = PathBuf::from(&drive.path);
                if !is_noop_or_cyclic(Path::new(&dragged.path), &dest) {
                    request = Some(DropRequest {
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
    request
}

fn find_root(tree: &[DriveNode], id: i64) -> Option<&Root> {
    tree.iter().flat_map(|d| &d.roots).find(|r| r.id == id)
}

/// One draggable folder row that is also a drop target (drop = move *into* this folder).
fn show_root(ui: &mut egui::Ui, root: &Root, moving: bool) -> Option<DropRequest> {
    let id = egui::Id::new(("folder_panel_root", root.id));
    let label = folder_name(&root.path);
    let response = if moving {
        ui.label(format!("\u{1F4C1} {label}"))
    } else {
        ui.dnd_drag_source(id, root.id, |ui| {
            ui.label(format!("\u{1F4C1} {label}"));
        })
        .response
    }
    .on_hover_text(&root.path);

    if moving {
        return None;
    }
    let dragged = response.dnd_hover_payload::<i64>()?;
    let dragged_id = *dragged;
    if dragged_id == root.id {
        return None;
    }
    if response.dnd_release_payload::<i64>().is_some() {
        return Some(DropRequest {
            root_id: dragged_id,
            dest_parent: PathBuf::from(&root.path),
        });
    }
    None
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
    fn tree_groups_by_drive_keeps_empty_extra_drives_and_skips_archived() {
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
        assert_eq!(d, [2, 1], "sorted by path, archived dropped");
        assert!(tree[2].roots.is_empty());
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
    fn folder_name_handles_trailing_separators() {
        assert_eq!(folder_name(r"D:\Photos\2026\"), "2026");
        assert_eq!(folder_name("/mnt/d"), "d");
    }
}
