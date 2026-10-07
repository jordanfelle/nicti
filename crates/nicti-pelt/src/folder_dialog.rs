//! Native "Browse..." folder dialog for the typed-path fields (#342): Import/Sync/Open, Move
//! destination and Export destination.
//!
//! The dialog runs on its own thread so the UI keeps repainting while it is open (a modal
//! dialog on the UI thread would stall egui and, on some platforms, the whole window).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// One in-flight (or idle) folder dialog. Each path field owns its own, so two dialogs never race
/// for the same result.
#[derive(Default)]
pub struct FolderPicker {
    pending: Option<Receiver<Option<PathBuf>>>,
}

impl FolderPicker {
    /// True while a dialog is open; callers disable the Browse button so it can't be stacked.
    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// Opens the dialog, starting at `start` when it is an existing directory. No-op if one is
    /// already open.
    pub fn open(&mut self, ctx: &egui::Context, title: &str, start: &str) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let title = title.to_owned();
        let start = start_dir(start);
        let spawned = std::thread::Builder::new()
            .name("folder-dialog".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new().set_title(title);
                if let Some(dir) = start {
                    dialog = dialog.set_directory(dir);
                }
                let _ = tx.send(dialog.pick_folder());
                ctx.request_repaint();
            });
        // A failed spawn just leaves the typed path field as the fallback.
        if spawned.is_ok() {
            self.pending = Some(rx);
        }
    }

    /// The folder the user chose, once, on the frame the dialog closes. `None` while it is still
    /// open, and also after a cancel.
    pub fn poll(&mut self) -> Option<PathBuf> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(choice) => {
                self.pending = None;
                choice
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                None
            }
        }
    }
}

/// The directory a dialog should open in: the typed text if it is an existing directory, else
/// its nearest existing parent, else the platform default (`None`).
fn start_dir(typed: &str) -> Option<PathBuf> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    Path::new(typed)
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.is_dir())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_blank_text_has_no_start_dir() {
        assert_eq!(start_dir(""), None);
        assert_eq!(start_dir("   "), None);
    }

    #[test]
    fn existing_dir_is_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            start_dir(dir.path().to_str().unwrap()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn missing_leaf_falls_back_to_nearest_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let typed = dir.path().join("not").join("yet");
        assert_eq!(
            start_dir(typed.to_str().unwrap()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn poll_without_a_dialog_is_none() {
        assert_eq!(FolderPicker::default().poll(), None);
    }

    #[test]
    fn poll_yields_the_choice_once_then_goes_idle() {
        let (tx, rx) = mpsc::channel();
        let mut picker = FolderPicker { pending: Some(rx) };
        assert!(picker.is_open());
        assert_eq!(picker.poll(), None); // still open
        tx.send(Some(PathBuf::from("/x"))).unwrap();
        assert_eq!(picker.poll(), Some(PathBuf::from("/x")));
        assert!(!picker.is_open());
    }

    #[test]
    fn cancel_closes_without_a_path() {
        let (tx, rx) = mpsc::channel();
        let mut picker = FolderPicker { pending: Some(rx) };
        tx.send(None).unwrap();
        assert_eq!(picker.poll(), None);
        assert!(!picker.is_open());
    }
}
