//! `InstallModelsJob`: the explicit, user-initiated download of AI-removal models (ADR-0218) as a
//! Pounce CPU-lane job, one artifact per chunk.
//!
//! Nothing here runs unless the UI submits it after the user agrees to the source and size it
//! showed. The UI reads live byte progress from [`InstallHandle::bytes`] (a `Pounce` progress
//! readout only updates between chunks, i.e. once per artifact -- too coarse for a 200 MB file),
//! and cancels through [`InstallHandle::cancel`], which the download loop polls per chunk.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};
use nicti_stalk::models::{Artifact, Downloader, InstallError, ModelStore, Status};

use crate::job::Slot;

/// The UI's side of an in-flight install.
#[derive(Clone)]
pub struct InstallHandle {
    /// Bytes downloaded so far across all artifacts (finished ones count in full).
    pub bytes: Arc<AtomicU64>,
    /// Total bytes this install will download (artifacts already installed are excluded). Shared
    /// because a repair only learns which files are bad once its job starts hashing them.
    pub total: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
    /// Resolved exactly once, with `Ok` or a message.
    pub result: Slot<Result<(), String>>,
}

pub struct InstallModelsJob {
    store: ModelStore,
    downloader: Arc<dyn Downloader + Send + Sync>,
    pending: Vec<&'static Artifact>,
    next: usize,
    finished_bytes: u64,
    bytes: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    total: Arc<AtomicU64>,
    /// Repair mode: on the first step, hash what is installed, delete what fails, and download only
    /// those (see [`InstallModelsJob::new_repair`]).
    repair: bool,
    checked: bool,
    result: Slot<Result<(), String>>,
    label: String,
}

impl InstallModelsJob {
    /// Plans an install of `artifacts` (skipping any already in `store`).
    pub fn new(
        store: ModelStore,
        downloader: Arc<dyn Downloader + Send + Sync>,
        artifacts: &[&'static Artifact],
    ) -> (Self, InstallHandle) {
        let pending: Vec<&'static Artifact> = artifacts
            .iter()
            .copied()
            .filter(|a| store.status(a) != Status::Installed)
            .collect();
        Self::build(store, downloader, pending, false)
    }

    /// Like [`Self::new`], but for when an install exists yet a load reported it corrupt.
    /// [`ModelStore::status`] only compares sizes, so a same-size bad file counts as installed and
    /// a plain install would skip it forever. This hashes every installed artifact on the job's
    /// thread, deletes the ones that fail, and downloads just those.
    pub fn new_repair(
        store: ModelStore,
        downloader: Arc<dyn Downloader + Send + Sync>,
        artifacts: &[&'static Artifact],
    ) -> (Self, InstallHandle) {
        Self::build(store, downloader, artifacts.to_vec(), true)
    }

    fn build(
        store: ModelStore,
        downloader: Arc<dyn Downloader + Send + Sync>,
        pending: Vec<&'static Artifact>,
        repair: bool,
    ) -> (Self, InstallHandle) {
        let total = Arc::new(AtomicU64::new(
            pending.iter().map(|a| a.download_size).sum(),
        ));
        let bytes = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result: Slot<Result<(), String>> = Arc::new(Mutex::new(None));
        let handle = InstallHandle {
            bytes: Arc::clone(&bytes),
            total: Arc::clone(&total),
            cancel: Arc::clone(&cancel),
            result: Arc::clone(&result),
        };
        let job = Self {
            store,
            downloader,
            pending,
            next: 0,
            finished_bytes: 0,
            bytes,
            cancel,
            total,
            repair,
            checked: false,
            result,
            label: if repair {
                "Repair AI removal models".to_owned()
            } else {
                "Download AI removal models".to_owned()
            },
        };
        (job, handle)
    }

    fn resolve(&self, r: Result<(), String>) {
        *self.result.lock().unwrap() = Some(r);
    }
}

impl ChunkedJob for InstallModelsJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Foreground,
            kind: JobKind::Download,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: self.bytes.load(Ordering::Relaxed),
            total: Some(self.total.load(Ordering::Relaxed)),
        }
    }

    /// Installs one artifact. A failure (network, checksum, cancel) resolves the result slot and
    /// ends the job with `Ok(Done)`: returning `Err` would drop the job without ever touching the
    /// slot, leaving the UI waiting on a download that already stopped.
    fn step(&mut self) -> Result<Step, JobError> {
        if self.repair && !self.checked {
            self.checked = true;
            let store = &self.store;
            let mut failed: Option<String> = None;
            self.pending.retain(|a| {
                if store.status(a) == Status::Installed && matches!(store.verify(a), Ok(true)) {
                    return false; // intact: nothing to fetch
                }
                if let Err(e) = store.remove(a) {
                    failed.get_or_insert(format!("Couldn't remove the bad {}: {e}", a.label));
                }
                true
            });
            self.total.store(
                self.pending.iter().map(|a| a.download_size).sum(),
                Ordering::Relaxed,
            );
            if let Some(msg) = failed {
                self.resolve(Err(msg));
                return Ok(Step::Done);
            }
        }
        let Some(&artifact) = self.pending.get(self.next) else {
            self.resolve(Ok(()));
            return Ok(Step::Done);
        };
        let base = self.finished_bytes;
        let counter = Arc::clone(&self.bytes);
        let outcome = self.store.install(
            artifact,
            self.downloader.as_ref(),
            &mut |p| counter.store(base + p.downloaded, Ordering::Relaxed),
            &self.cancel,
        );
        match outcome {
            Ok(_) => {
                self.finished_bytes += artifact.download_size;
                self.bytes.store(self.finished_bytes, Ordering::Relaxed);
                self.next += 1;
                if self.next >= self.pending.len() {
                    self.resolve(Ok(()));
                    Ok(Step::Done)
                } else {
                    Ok(Step::Yield)
                }
            }
            Err(InstallError::Cancelled) => {
                self.resolve(Err("Download cancelled.".to_owned()));
                Ok(Step::Done)
            }
            Err(e) => {
                self.resolve(Err(format!("Couldn't download {}: {e}", artifact.label)));
                Ok(Step::Done)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_stalk::models::{DownloadError, Payload};
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    fn sha(bytes: &[u8]) -> &'static str {
        let mut s = String::new();
        for b in Sha256::digest(bytes) {
            let _ = write!(s, "{b:02x}");
        }
        Box::leak(s.into_boxed_str())
    }

    fn artifact(id: &'static str, body: &[u8]) -> &'static Artifact {
        Box::leak(Box::new(Artifact {
            id,
            label: id,
            file_name: "m.bin",
            url: Box::leak(format!("https://example.invalid/{id}").into_boxed_str()),
            download_size: body.len() as u64,
            download_sha256: sha(body),
            installed_size: body.len() as u64,
            installed_sha256: sha(body),
            license: "test",
            payload: Payload::File,
        }))
    }

    /// Serves `bodies[url suffix]`; unknown URLs fail like a 404.
    struct Fake(Vec<(&'static str, Vec<u8>)>);

    impl Downloader for Fake {
        fn fetch(
            &self,
            url: &str,
            _max: u64,
            on_chunk: &mut dyn FnMut(&[u8]) -> bool,
        ) -> Result<(), DownloadError> {
            let (_, body) = self
                .0
                .iter()
                .find(|(id, _)| url.ends_with(id))
                .ok_or_else(|| DownloadError::Network("404".into()))?;
            for piece in body.chunks(5) {
                if !on_chunk(piece) {
                    break;
                }
            }
            Ok(())
        }
    }

    fn temp_store(name: &str) -> ModelStore {
        let dir = std::env::temp_dir().join(format!("nicti-groom-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ModelStore::new(dir)
    }

    fn resolved(h: &InstallHandle) -> Option<Result<(), String>> {
        h.result.lock().unwrap().clone()
    }

    #[test]
    fn installs_every_artifact_one_chunk_each_and_tracks_bytes() {
        let (a_body, b_body) = (b"first model bytes".to_vec(), b"second".to_vec());
        let (a, b) = (artifact("a", &a_body), artifact("b", &b_body));
        let store = temp_store("multi");
        let dl = Arc::new(Fake(vec![("a", a_body.clone()), ("b", b_body.clone())]));
        let (mut job, handle) = InstallModelsJob::new(store.clone(), dl, &[a, b]);
        assert_eq!(
            handle.total.load(Ordering::Relaxed),
            (a_body.len() + b_body.len()) as u64
        );

        assert_eq!(job.step().unwrap(), Step::Yield, "more artifacts remain");
        assert_eq!(handle.bytes.load(Ordering::Relaxed), a_body.len() as u64);
        assert_eq!(resolved(&handle), None, "not done until the last artifact");
        assert_eq!(
            job.progress().total,
            Some(handle.total.load(Ordering::Relaxed))
        );

        assert_eq!(job.step().unwrap(), Step::Done);
        assert_eq!(
            handle.bytes.load(Ordering::Relaxed),
            handle.total.load(Ordering::Relaxed)
        );
        assert_eq!(resolved(&handle), Some(Ok(())));
        assert_eq!(store.status(a), Status::Installed);
        assert_eq!(store.status(b), Status::Installed);
    }

    #[test]
    fn already_installed_artifacts_are_skipped_and_excluded_from_the_total() {
        let (a_body, b_body) = (b"aaaa".to_vec(), b"bbbbbbbb".to_vec());
        let (a, b) = (artifact("a2", &a_body), artifact("b2", &b_body));
        let store = temp_store("skip");
        let dl = Arc::new(Fake(vec![("a2", a_body), ("b2", b_body.clone())]));
        let (mut first, _) = InstallModelsJob::new(store.clone(), dl.clone(), &[a]);
        assert_eq!(first.step().unwrap(), Step::Done);

        let (_job, handle) = InstallModelsJob::new(store, dl, &[a, b]);
        assert_eq!(
            handle.total.load(Ordering::Relaxed),
            b_body.len() as u64,
            "a is already installed"
        );
    }

    #[test]
    fn nothing_pending_finishes_immediately() {
        let store = temp_store("empty");
        let (mut job, handle) = InstallModelsJob::new(store, Arc::new(Fake(vec![])), &[]);
        assert_eq!(handle.total.load(Ordering::Relaxed), 0);
        assert_eq!(job.step().unwrap(), Step::Done);
        assert_eq!(resolved(&handle), Some(Ok(())));
    }

    #[test]
    fn a_download_failure_resolves_the_slot_and_ends_the_job_cleanly() {
        let body = b"never served".to_vec();
        let a = artifact("missing", &body);
        let store = temp_store("fail");
        let (mut job, handle) = InstallModelsJob::new(store.clone(), Arc::new(Fake(vec![])), &[a]);
        // step() must not return Err: the scheduler would drop the job without resolving the slot.
        assert_eq!(job.step().unwrap(), Step::Done);
        let msg = resolved(&handle).expect("slot resolved").unwrap_err();
        assert!(msg.contains("missing") && msg.contains("404"), "{msg}");
        assert_eq!(store.status(a), Status::NotInstalled);
    }

    #[test]
    fn cancelling_stops_the_install_and_reports_it() {
        let body = b"a body long enough to be cut off".to_vec();
        let a = artifact("cancel", &body);
        let store = temp_store("cancel");
        let (mut job, handle) =
            InstallModelsJob::new(store.clone(), Arc::new(Fake(vec![("cancel", body)])), &[a]);
        handle.cancel.store(true, Ordering::Relaxed);
        assert_eq!(job.step().unwrap(), Step::Done);
        assert_eq!(
            resolved(&handle),
            Some(Err("Download cancelled.".to_owned()))
        );
        assert_eq!(store.status(a), Status::NotInstalled);
    }

    #[test]
    fn a_corrupt_download_is_rejected_and_reported() {
        let good = b"the real model".to_vec();
        let a = artifact("corrupt", &good);
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        let store = temp_store("corrupt");
        let (mut job, handle) =
            InstallModelsJob::new(store.clone(), Arc::new(Fake(vec![("corrupt", bad)])), &[a]);
        assert_eq!(job.step().unwrap(), Step::Done);
        assert!(resolved(&handle).unwrap().is_err());
        assert_eq!(store.status(a), Status::NotInstalled);
    }

    #[test]
    fn repair_replaces_a_corrupt_file_of_the_right_size_that_a_plain_install_would_skip() {
        let good = b"the real model bytes".to_vec();
        let a = artifact("repair", &good);
        let store = temp_store("repair");
        let dl = Arc::new(Fake(vec![("repair", good.clone())]));
        let (mut first, _) = InstallModelsJob::new(store.clone(), dl.clone(), &[a]);
        assert_eq!(first.step().unwrap(), Step::Done);

        // Same length, different bytes: `status` is fooled.
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        std::fs::write(store.path(a), &bad).unwrap();
        assert_eq!(store.status(a), Status::Installed);
        assert!(!store.verify(a).unwrap());

        // A plain install has nothing to do...
        let (plain, plain_handle) = InstallModelsJob::new(store.clone(), dl.clone(), &[a]);
        assert_eq!(plain_handle.total.load(Ordering::Relaxed), 0);
        drop(plain);
        // ...but a repair fixes it, downloading exactly the bad file.
        let (mut repair, handle) = InstallModelsJob::new_repair(store.clone(), dl, &[a]);
        assert_eq!(repair.step().unwrap(), Step::Done);
        assert_eq!(resolved(&handle), Some(Ok(())));
        assert!(store.verify(a).unwrap());
        assert_eq!(std::fs::read(store.path(a)).unwrap(), good);
    }

    #[test]
    fn repair_leaves_intact_files_alone_and_downloads_nothing() {
        let (a_body, b_body) = (b"aaaaaa".to_vec(), b"bbbbbbbbbb".to_vec());
        let (a, b) = (artifact("ra", &a_body), artifact("rb", &b_body));
        let store = temp_store("repair-intact");
        let dl = Arc::new(Fake(vec![("ra", a_body), ("rb", b_body)]));
        let (mut install, _) = InstallModelsJob::new(store.clone(), dl.clone(), &[a, b]);
        while install.step().unwrap() == Step::Yield {}

        // A downloader that fails every request: any fetch during repair would surface as an error.
        let (mut repair, handle) =
            InstallModelsJob::new_repair(store.clone(), Arc::new(Fake(vec![])), &[a, b]);
        assert_eq!(repair.step().unwrap(), Step::Done);
        assert_eq!(resolved(&handle), Some(Ok(())));
        assert_eq!(
            handle.total.load(Ordering::Relaxed),
            0,
            "nothing needed re-downloading"
        );
    }

    #[test]
    fn repair_fetches_only_the_bad_one_of_two() {
        let (a_body, b_body) = (b"aaaaaa".to_vec(), b"bbbbbbbbbb".to_vec());
        let (a, b) = (artifact("pa", &a_body), artifact("pb", &b_body));
        let store = temp_store("repair-one");
        let dl = Arc::new(Fake(vec![("pa", a_body.clone()), ("pb", b_body.clone())]));
        let (mut install, _) = InstallModelsJob::new(store.clone(), dl.clone(), &[a, b]);
        while install.step().unwrap() == Step::Yield {}
        let mut bad = b_body.clone();
        bad[3] ^= 1;
        std::fs::write(store.path(b), bad).unwrap();

        let (mut repair, handle) = InstallModelsJob::new_repair(store.clone(), dl, &[a, b]);
        while repair.step().unwrap() == Step::Yield {}
        assert_eq!(resolved(&handle), Some(Ok(())));
        assert_eq!(handle.total.load(Ordering::Relaxed), b_body.len() as u64);
        assert!(store.verify(a).unwrap() && store.verify(b).unwrap());
    }

    #[test]
    fn spec_is_a_foreground_cpu_download() {
        let (job, _) = InstallModelsJob::new(temp_store("spec"), Arc::new(Fake(vec![])), &[]);
        let spec = job.spec();
        assert_eq!(spec.kind, JobKind::Download);
        assert_eq!(spec.lane, Lane::Cpu);
        assert_eq!(spec.priority, Priority::Foreground);
    }
}
