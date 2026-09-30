//! The marking writer (#32): one thread that owns every catalog read and write the culling UI
//! makes, and the undo ring.
//!
//! Why a thread, not a direct call from the key handler: the catalog is one `Mutex<Connection>`,
//! and an import running in the background holds it in bursts. A keypress that waited on it would
//! blow the < 50 ms culling budget (`docs/benchmarks.md`) exactly when the user is culling while
//! an import finishes. So the UI updates its own cache first and advances immediately; this
//! thread does the write behind it, strictly in the order the keys were pressed.
//!
//! One thread also gives ordering for free: an undo can only run after the writes it reverses,
//! and a read can only see the state after every write queued before it.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use nicti_lair::{AssetMeta, CatalogStore};

use super::keys::{apply_all, CullAction};
use super::undo::{Entry, UndoRing};

/// The catalog operations the worker needs. A trait so tests can substitute a store they control
/// (block it, fail it); the real one is [`CatalogMeta`].
pub trait MetaStore: Send + Sync {
    fn get_meta(&self, ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, String>;
    fn set_meta(&self, items: &[(i64, AssetMeta)]) -> Result<(), String>;
}

/// [`MetaStore`] over the real catalog.
pub struct CatalogMeta(pub Arc<dyn CatalogStore + Send + Sync>);

impl MetaStore for CatalogMeta {
    fn get_meta(&self, ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, String> {
        self.0.get_meta(ids).map_err(|e| e.to_string())
    }
    fn set_meta(&self, items: &[(i64, AssetMeta)]) -> Result<(), String> {
        self.0.set_meta(items).map_err(|e| e.to_string())
    }
}

enum Cmd {
    Apply {
        ids: Vec<i64>,
        action: CullAction,
    },
    Undo,
    Redo,
    Read(Vec<i64>),
    Forget(Vec<i64>),
    /// Replies once every command queued before it has been handled -- the writer is strictly
    /// ordered, so this is a deterministic "everything so far is done" (tests wait on it instead
    /// of sleeping).
    #[cfg_attr(not(test), allow(dead_code))]
    Barrier,
}

/// What the worker tells the UI.
#[derive(Debug)]
pub enum Reply {
    /// A write finished. `ids` is every photo the command targeted (for the UI's pending
    /// bookkeeping, empty for undo/redo); `changed` holds the authoritative new markers of the
    /// photos that actually changed. `counted` is `true` for an `Apply`.
    Done {
        ids: Vec<i64>,
        changed: Vec<(i64, AssetMeta)>,
        counted: bool,
    },
    Loaded(HashMap<i64, AssetMeta>),
    /// Answer to [`Cmd::Barrier`].
    #[cfg_attr(not(test), allow(dead_code))]
    Barrier,
    /// A command failed and changed nothing. `ids` are the photos whose cached markers can no
    /// longer be trusted (the UI re-reads them).
    Failed {
        ids: Vec<i64>,
        message: String,
        counted: bool,
    },
}

pub struct MetaWorker {
    tx: Option<Sender<Cmd>>,
    rx: Receiver<Reply>,
    thread: Option<JoinHandle<()>>,
}

impl MetaWorker {
    /// `wake` is called after every reply so an idle UI repaints and picks it up.
    pub fn spawn(store: Arc<dyn MetaStore>, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, cmd_rx) = channel::<Cmd>();
        let (reply_tx, rx) = channel::<Reply>();
        let thread = std::thread::Builder::new()
            .name("nicti-cull-writer".into())
            .spawn(move || {
                let mut ring = UndoRing::default();
                while let Ok(cmd) = cmd_rx.recv() {
                    let reply = handle(&*store, &mut ring, cmd);
                    if let Some(reply) = reply {
                        if reply_tx.send(reply).is_err() {
                            return;
                        }
                        wake();
                    }
                }
            })
            .expect("spawn the marking writer thread");
        MetaWorker {
            tx: Some(tx),
            rx,
            thread: Some(thread),
        }
    }

    pub fn apply(&self, ids: Vec<i64>, action: CullAction) {
        self.send(Cmd::Apply { ids, action });
    }
    pub fn undo(&self) {
        self.send(Cmd::Undo);
    }
    pub fn redo(&self) {
        self.send(Cmd::Redo);
    }
    pub fn read(&self, ids: Vec<i64>) {
        self.send(Cmd::Read(ids));
    }
    pub fn forget(&self, ids: Vec<i64>) {
        self.send(Cmd::Forget(ids));
    }
    #[cfg(test)]
    pub fn barrier(&self) {
        self.send(Cmd::Barrier);
    }
    /// Whether the writer thread has stopped (a panic in the store, e.g. a poisoned catalog
    /// mutex). Nothing it was asked to do afterwards happens, so the UI must say so.
    pub fn is_dead(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    pub fn try_recv(&self) -> Option<Reply> {
        self.rx.try_recv().ok()
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            // A closed channel means the thread died (a panic in the store); nothing useful to do
            // from a key handler, and the UI keeps working from its cache.
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for MetaWorker {
    fn drop(&mut self) {
        // Closing the command channel lets the thread finish its queue, then exit; joining makes
        // sure a queued mark is written before the app (or a test) goes away.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn handle(store: &dyn MetaStore, ring: &mut UndoRing, cmd: Cmd) -> Option<Reply> {
    match cmd {
        Cmd::Read(ids) => Some(match store.get_meta(&ids) {
            Ok(map) => Reply::Loaded(map),
            // Nothing is pending for a read, but the ids must be reported: the UI marked them
            // "requested" and would otherwise never ask for them again.
            Err(message) => Reply::Failed {
                ids,
                message,
                counted: false,
            },
        }),
        Cmd::Forget(ids) => {
            ring.forget(&ids);
            None
        }
        Cmd::Barrier => Some(Reply::Barrier),
        Cmd::Apply { ids, action } => Some(match apply(store, ring, &ids, action) {
            Ok(changed) => Reply::Done {
                ids,
                changed,
                counted: true,
            },
            Err(message) => Reply::Failed {
                ids,
                message,
                counted: true,
            },
        }),
        Cmd::Undo => undo(store, ring),
        Cmd::Redo => redo(store, ring),
    }
}

/// Reverses the latest action: writes each photo's *before* markers. `None` with nothing to undo.
fn undo(store: &dyn MetaStore, ring: &mut UndoRing) -> Option<Reply> {
    let entry = ring.begin_undo()?;
    Some(match store.set_meta(&entry.before) {
        Ok(()) => {
            let changed = entry.before.clone();
            ring.commit_undo(entry);
            Reply::Done {
                ids: vec![],
                changed,
                counted: false,
            }
        }
        Err(message) => {
            let ids = entry.before.iter().map(|(id, _)| *id).collect();
            ring.abort_undo(entry);
            Reply::Failed {
                ids,
                message,
                counted: false,
            }
        }
    })
}

/// Re-applies the latest undone action: writes each photo's *after* markers.
fn redo(store: &dyn MetaStore, ring: &mut UndoRing) -> Option<Reply> {
    let entry = ring.begin_redo()?;
    Some(match store.set_meta(&entry.after) {
        Ok(()) => {
            let changed = entry.after.clone();
            ring.commit_redo(entry);
            Reply::Done {
                ids: vec![],
                changed,
                counted: false,
            }
        }
        Err(message) => {
            let ids = entry.after.iter().map(|(id, _)| *id).collect();
            ring.abort_redo(entry);
            Reply::Failed {
                ids,
                message,
                counted: false,
            }
        }
    })
}

/// Reads the targets' current markers, computes the action's effect, writes the photos that
/// change, and records the undo entry. Returns the authoritative new markers of the changed ones.
fn apply(
    store: &dyn MetaStore,
    ring: &mut UndoRing,
    ids: &[i64],
    action: CullAction,
) -> Result<Vec<(i64, AssetMeta)>, String> {
    let current = store.get_meta(ids)?;
    // Photos deleted since the key was pressed have no row; they are simply skipped.
    let before: Vec<(i64, AssetMeta)> = ids
        .iter()
        .filter_map(|id| current.get(id).map(|m| (*id, m.clone())))
        .collect();
    let metas: Vec<AssetMeta> = before.iter().map(|(_, m)| m.clone()).collect();
    let after = apply_all(action, &metas);

    let mut entry = Entry {
        before: Vec::new(),
        after: Vec::new(),
    };
    for ((id, was), now) in before.into_iter().zip(after) {
        if was != now {
            entry.before.push((id, was));
            entry.after.push((id, now));
        }
    }
    if entry.after.is_empty() {
        return Ok(Vec::new());
    }
    store.set_meta(&entry.after)?;
    let changed = entry.after.clone();
    ring.push(entry);
    Ok(changed)
}
