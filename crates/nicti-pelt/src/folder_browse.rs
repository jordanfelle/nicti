//! Browsing a drive's subfolders in the Library sidebar before importing anything (#472).
//!
//! The sidebar used to list only *imported* roots. This adds lazy, expandable folder rows:
//! expanding a drive or folder reads its immediate subfolders on a worker thread (never on the UI
//! thread, #332: a dead network share can hang `read_dir` for tens of seconds), shows a spinner
//! meanwhile, and collapsing drops the listing so the next expand re-reads it. Nothing here
//! registers or scans a folder; the panel offers an explicit "Import" button per row.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::pending::Pending;

/// Most subfolders listed per folder. A folder with more shows the first of them and a note;
/// a directory of 100k entries must not become 100k widgets.
pub const MAX_BROWSE_ENTRIES: usize = 500;

/// One folder's immediate subfolders.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    /// Display names (lossy for a non-UTF-8 folder), case-insensitively sorted.
    pub names: Vec<String>,
    /// The real file names, parallel to `names`: `dir.join` these, never the lossy display name,
    /// or a folder called `caf\xe9` would browse and import a path that does not exist.
    pub os_names: Vec<std::ffi::OsString>,
    /// More subfolders existed than [`MAX_BROWSE_ENTRIES`].
    pub truncated: bool,
    /// Why the folder could not be read (permissions, gone, not a directory); `names` is empty.
    pub error: Option<String>,
}

/// Folders that exist on a Windows drive but are never somewhere a photographer browses.
fn is_system_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "$recycle.bin" | "system volume information" | "$windows.~bt" | "recovery"
    )
}

/// The immediate subfolders of `dir`: real directories only (a symlinked or junction directory is
/// skipped, so a link back up the tree can never make the tree infinite), hidden (`.name`) and
/// system folders left out, sorted without regard to case.
pub fn list_subfolders(dir: &Path) -> Listing {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            return Listing {
                error: Some(e.to_string()),
                ..Listing::default()
            }
        }
    };
    let mut found: Vec<(String, std::ffi::OsString)> = Vec::new();
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow links: a symlink to a directory is a symlink here.
        let Ok(ty) = entry.file_type() else { continue };
        if !ty.is_dir() {
            continue;
        }
        let os = entry.file_name();
        let name = os.to_string_lossy().into_owned();
        if name.starts_with('.') || is_system_name(&name) {
            continue;
        }
        found.push((name, os));
    }
    found.sort_by_cached_key(|(n, os)| (n.to_lowercase(), n.clone(), os.clone()));
    let truncated = found.len() > MAX_BROWSE_ENTRIES;
    found.truncate(MAX_BROWSE_ENTRIES);
    let (names, os_names) = found.into_iter().unzip();
    Listing {
        names,
        os_names,
        truncated,
        error: None,
    }
}

enum Slot {
    Loading(Pending<Listing>),
    Ready(Listing),
}

/// Which browsed folders are open and what each one's last read returned.
#[derive(Default)]
pub struct Browse {
    open: HashSet<PathBuf>,
    slots: HashMap<PathBuf, Slot>,
    lister: Option<fn(&Path) -> Listing>,
    /// The root whose location was already opened for the user (once per selection).
    revealed: Option<i64>,
}

impl Browse {
    #[cfg(test)]
    pub(crate) fn with_lister(lister: fn(&Path) -> Listing) -> Self {
        Self {
            lister: Some(lister),
            ..Self::default()
        }
    }

    /// A browser that already revealed `key`'s location (tests that select a root and are not
    /// about the reveal: it would start real directory reads).
    #[cfg(test)]
    pub fn already_revealed(key: i64) -> Self {
        Self {
            revealed: Some(key),
            ..Self::default()
        }
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.open.contains(path)
    }

    /// Opens or closes `path`. Closing forgets its listing (and cancels a read in flight by
    /// dropping the receiver), so reopening shows what is on disk now.
    pub fn set_open(&mut self, path: &Path, open: bool) {
        if open {
            self.open.insert(path.to_path_buf());
        } else {
            self.open.remove(path);
            // A read still in flight stays: dropping it would let the next open spawn another
            // thread, and a hung share would leak one per open/close. A finished listing is
            // dropped, so reopening re-reads what is on disk.
            if !matches!(self.slots.get(path), Some(Slot::Loading(_))) {
                self.slots.remove(path);
            }
        }
    }

    /// Opens every ancestor of `target` (and `target`'s own parent chain from `root` down), so a
    /// folder the user is already working in is visible when the panel is first browsed.
    pub fn reveal(&mut self, root: &Path, target: &Path) {
        if !target.starts_with(root) {
            return;
        }
        let mut p = root.to_path_buf();
        self.open.insert(p.clone());
        if let Ok(rel) = target.strip_prefix(root) {
            for part in rel.components() {
                p.push(part);
                self.open.insert(p.clone());
            }
        }
        // The target itself is shown, not expanded.
        self.open.remove(target);
    }

    /// [`Self::reveal`] for the folder the user selected, but only the first time `key` (the root
    /// id) is seen: afterwards the user's own expanding and collapsing is left alone.
    pub fn reveal_once(&mut self, key: i64, drive: &Path, target: &Path) {
        if self.revealed != Some(key) {
            self.revealed = Some(key);
            self.reveal(drive, target);
        }
    }

    /// The listing for an open folder, starting the read the first time it is asked for. `None`
    /// while the read is still running.
    pub fn listing(&mut self, ctx: &egui::Context, path: &Path) -> Option<&Listing> {
        if !self.slots.contains_key(path) {
            let dir = path.to_path_buf();
            let lister = self.lister.unwrap_or(list_subfolders);
            self.slots.insert(
                path.to_path_buf(),
                Slot::Loading(Pending::spawn(ctx, move || lister(&dir))),
            );
        }
        // Collect a finished read.
        if let Some(Slot::Loading(p)) = self.slots.get(path) {
            match p.poll() {
                Ok(Some(listing)) => {
                    self.slots.insert(path.to_path_buf(), Slot::Ready(listing));
                }
                Ok(None) => return None,
                // The worker died without an answer: report it instead of spinning forever.
                Err(_) => {
                    self.slots.insert(
                        path.to_path_buf(),
                        Slot::Ready(Listing {
                            error: Some("the folder could not be read".into()),
                            ..Listing::default()
                        }),
                    );
                }
            }
        }
        match self.slots.get(path) {
            Some(Slot::Ready(l)) => Some(l),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn lists_only_real_visible_subfolders_sorted_without_regard_to_case() {
        let t = tempfile::tempdir().unwrap();
        for d in [
            "beta",
            "Zed",
            "apple",
            "gamma",
            ".hidden",
            "$RECYCLE.BIN",
            "System Volume Information",
        ] {
            fs::create_dir(t.path().join(d)).unwrap();
        }
        fs::write(t.path().join("file.txt"), b"x").unwrap();
        let l = list_subfolders(t.path());
        assert_eq!(
            l.names,
            ["apple", "beta", "gamma", "Zed"],
            "case-insensitive, not byte order"
        );
        let real: Vec<String> = l
            .os_names
            .iter()
            .map(|o| o.to_string_lossy().into_owned())
            .collect();
        assert_eq!(real, l.names);
        assert!(!l.truncated && l.error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_listed_so_a_link_cannot_make_the_tree_infinite() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("real")).unwrap();
        std::os::unix::fs::symlink(t.path(), t.path().join("loop")).unwrap();
        std::os::unix::fs::symlink(t.path().join("real"), t.path().join("alias")).unwrap();
        assert_eq!(list_subfolders(t.path()).names, ["real"]);
    }

    #[test]
    fn a_huge_folder_is_capped_and_says_so_and_an_unreadable_one_reports_why() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..MAX_BROWSE_ENTRIES + 3 {
            fs::create_dir(t.path().join(format!("d{i:04}"))).unwrap();
        }
        let l = list_subfolders(t.path());
        assert_eq!(l.names.len(), MAX_BROWSE_ENTRIES);
        assert!(l.truncated);
        assert_eq!(l.names[0], "d0000", "the cap keeps the sorted prefix");
        let gone = list_subfolders(&t.path().join("missing"));
        assert!(gone.names.is_empty() && gone.error.is_some());
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(
            list_subfolders(&file).error.is_some(),
            "a file is not a folder"
        );
    }

    fn wait_for<'a>(b: &'a mut Browse, ctx: &egui::Context, p: &Path) -> &'a Listing {
        let start = std::time::Instant::now();
        // Polled by deadline, not a fixed sleep: the worker is a real thread.
        while b.listing(ctx, p).is_none() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "never listed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        b.listing(ctx, p).unwrap()
    }

    fn fake(dir: &Path) -> Listing {
        let name = format!("in-{}", dir.file_name().unwrap().to_string_lossy());
        Listing {
            os_names: vec![name.clone().into()],
            names: vec![name],
            ..Listing::default()
        }
    }

    #[test]
    fn a_listing_is_read_off_the_calling_thread_once_per_open_and_dropped_on_close() {
        fn slow(dir: &Path) -> Listing {
            std::thread::sleep(std::time::Duration::from_millis(150));
            fake(dir)
        }
        let ctx = egui::Context::default();
        let mut b = Browse::with_lister(slow);
        let p = Path::new("/x/photos");
        b.set_open(p, true);
        assert!(b.is_open(p));
        let t = std::time::Instant::now();
        assert!(
            b.listing(&ctx, p).is_none(),
            "still loading: the first call must not block"
        );
        assert!(t.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(wait_for(&mut b, &ctx, p).names, ["in-photos"]);
        // Closing forgets it; reopening reads again.
        b.set_open(p, false);
        assert!(!b.is_open(p));
        b.set_open(p, true);
        assert!(b.listing(&ctx, p).is_none(), "a reopened folder is re-read");
        assert_eq!(wait_for(&mut b, &ctx, p).names, ["in-photos"]);
    }

    #[test]
    fn reveal_once_opens_a_selected_folders_location_once_and_then_leaves_the_user_alone() {
        let mut b = Browse::default();
        let (drive, target) = (Path::new("/mnt/d"), Path::new("/mnt/d/2026/trip"));
        b.reveal_once(7, drive, target);
        assert!(b.is_open(Path::new("/mnt/d/2026")));
        // The user collapses it; the same selection must not re-open it every frame.
        b.set_open(Path::new("/mnt/d/2026"), false);
        b.reveal_once(7, drive, target);
        assert!(!b.is_open(Path::new("/mnt/d/2026")));
        // A different selection reveals its own path.
        b.reveal_once(8, drive, Path::new("/mnt/d/other/x"));
        assert!(b.is_open(Path::new("/mnt/d/other")));
    }

    #[test]
    fn reveal_opens_the_path_down_to_a_folder_but_not_the_folder_itself() {
        let mut b = Browse::default();
        let root = Path::new("/mnt/d");
        b.reveal(root, Path::new("/mnt/d/2026/trip/day1"));
        for open in ["/mnt/d", "/mnt/d/2026", "/mnt/d/2026/trip"] {
            assert!(b.is_open(Path::new(open)), "{open}");
        }
        assert!(!b.is_open(Path::new("/mnt/d/2026/trip/day1")));
        // A target outside the root changes nothing.
        let mut c = Browse::default();
        c.reveal(root, Path::new("/mnt/e/x"));
        assert!(!c.is_open(root) && !c.is_open(Path::new("/mnt/e")));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_folder_name_keeps_its_real_name_for_joining() {
        use std::os::unix::ffi::OsStrExt;
        let t = tempfile::tempdir().unwrap();
        let odd = std::ffi::OsStr::from_bytes(b"caf\xe9");
        fs::create_dir(t.path().join(odd)).unwrap();
        let l = list_subfolders(t.path());
        assert_eq!(l.names.len(), 1);
        assert!(l.names[0].contains('\u{FFFD}'), "the display name is lossy");
        assert!(
            t.path().join(&l.os_names[0]).is_dir(),
            "the real name still resolves"
        );
        assert!(
            !t.path().join(&l.names[0]).exists(),
            "the lossy one would not"
        );
    }

    #[test]
    fn closing_a_node_mid_read_keeps_that_one_read_so_reopening_does_not_spawn_another() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        static RELEASE: AtomicBool = AtomicBool::new(false);
        fn blocked(dir: &Path) -> Listing {
            CALLS.fetch_add(1, Ordering::SeqCst);
            while !RELEASE.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            fake(dir)
        }
        let ctx = egui::Context::default();
        let mut b = Browse::with_lister(blocked);
        let p = Path::new("/x/hung");
        for _ in 0..5 {
            b.set_open(p, true);
            assert!(b.listing(&ctx, p).is_none());
            b.set_open(p, false);
        }
        let t = std::time::Instant::now();
        while CALLS.load(Ordering::SeqCst) == 0 {
            assert!(t.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "one thread per path while it hangs"
        );
        RELEASE.store(true, Ordering::SeqCst);
        b.set_open(p, true);
        assert_eq!(wait_for(&mut b, &ctx, p).names, ["in-hung"]);
    }
}
