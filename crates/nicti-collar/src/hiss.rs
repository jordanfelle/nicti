//! Hiss, the kill switch's policy (ADR-0214 sections 4 and 5): per-module state that is persisted
//! and survives a restart, the bug-versus-violation distinction, and the file-backed safe-mode
//! flag read before any third-party module is allowed to run.
//!
//! The state machine is pure (time is passed in, nothing sleeps) and `Serialize`: the host saves
//! it after every transition, so a killed or quarantined module is not re-instantiated at the next
//! launch. There is **no silent auto-enable**: a module the registry has never seen, and one whose
//! update widened its authority, does not run until the user consents.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a module is currently allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Installed but never consented to (or consent revoked): does not run.
    NeedsConsent,
    Enabled,
    /// Soft: repeated bugs. One click re-enables it.
    AutoDisabled {
        since: u64,
        failures: u32,
    },
    /// The user pressed the kill switch. Persisted, so it is not re-instantiated until re-enabled.
    Killed {
        since: u64,
    },
    /// Hard: the module tried something it never declared. Only an explicit acknowledgement of
    /// what it attempted re-enables it, and no update clears it.
    Quarantined {
        since: u64,
        attempted: String,
    },
}

/// What went wrong in a call (ADR-0214 section 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// A trap, panic, fuel/epoch timeout or malformed output from an otherwise correctly scoped call.
    Bug(String),
    /// The module's own code reached for a capability it never declared, or exceeded a granted one.
    Violation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Bugs older than this many seconds no longer count.
    pub window_secs: u64,
    /// This many bugs inside the window auto-disable the module.
    pub max_bugs: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            window_secs: 600,
            max_bugs: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    status: Status,
    /// Unix seconds of recent bugs, oldest first.
    bugs: Vec<u64>,
    /// The user has consented to the module's *current* authority. Survives every soft state, so
    /// no kill, auto-disable or quarantine path can re-enable a module past a missing consent.
    /// Absent in an older state file reads false: fail closed.
    #[serde(default)]
    consented: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HissError {
    #[error("unknown module {0:?}")]
    Unknown(String),
    #[error("{0:?} is quarantined: acknowledge what it attempted to re-enable it")]
    QuarantineNeedsAcknowledgement(String),
    #[error("{0:?} needs the user's consent before it can run")]
    NeedsConsent(String),
    #[error("{0:?} is not quarantined")]
    NotQuarantined(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hiss {
    modules: BTreeMap<String, Record>,
    #[serde(skip)]
    policy: Policy,
}

impl Hiss {
    pub fn new(policy: Policy) -> Hiss {
        Hiss {
            modules: BTreeMap::new(),
            policy,
        }
    }

    pub fn status(&self, id: &str) -> Option<&Status> {
        self.modules.get(id).map(|r| &r.status)
    }

    /// Whether the host may instantiate `id` right now. Unknown modules may not run.
    pub fn may_run(&self, id: &str) -> bool {
        matches!(self.status(id), Some(Status::Enabled))
    }

    /// A module appeared (first install). It does not run until the user consents.
    pub fn install(&mut self, id: &str) {
        self.modules.entry(id.to_owned()).or_insert(Record {
            status: Status::NeedsConsent,
            bugs: Vec::new(),
            consented: false,
        });
    }

    /// The user accepted the consent screen. Never clears a quarantine, a kill or an auto-disable:
    /// those have their own way back.
    pub fn consent(&mut self, id: &str) -> Result<(), HissError> {
        let rec = self
            .modules
            .get_mut(id)
            .ok_or_else(|| HissError::Unknown(id.to_owned()))?;
        rec.consented = true;
        if rec.status == Status::NeedsConsent {
            rec.status = Status::Enabled;
        }
        Ok(())
    }

    /// An update was installed. If it widened the module's authority (`widened`, from
    /// `consent::widenings`) a running module is disabled until re-consent; a quarantine stays;
    /// an update that gains nothing changes nothing.
    pub fn on_update(&mut self, id: &str, widened: bool) {
        if let Some(rec) = self.modules.get_mut(id) {
            if widened {
                rec.consented = false;
                if rec.status == Status::Enabled {
                    rec.status = Status::NeedsConsent;
                }
            }
        }
    }

    /// Records a fault from a call and returns the module's new status.
    pub fn record_fault(&mut self, id: &str, fault: Fault, now: u64) -> Status {
        let window = self.policy.window_secs;
        let max = self.policy.max_bugs;
        let rec = self.modules.entry(id.to_owned()).or_insert(Record {
            status: Status::NeedsConsent,
            bugs: Vec::new(),
            consented: false,
        });
        match fault {
            Fault::Violation(attempted) => {
                // Hard and immediate; a second violation keeps the first record of what happened.
                if !matches!(rec.status, Status::Quarantined { .. }) {
                    rec.status = Status::Quarantined {
                        since: now,
                        attempted,
                    };
                }
            }
            Fault::Bug(_) => {
                rec.bugs.retain(|t| now.saturating_sub(*t) < window);
                rec.bugs.push(now);
                let n = u32::try_from(rec.bugs.len()).unwrap_or(u32::MAX);
                if rec.status == Status::Enabled && n >= max {
                    rec.status = Status::AutoDisabled {
                        since: now,
                        failures: n,
                    };
                }
            }
        }
        rec.status.clone()
    }

    /// The kill switch: every module that may run (or is merely auto-disabled) is stopped and the
    /// state persisted, so none is re-instantiated until explicitly re-enabled. A quarantine is kept.
    pub fn kill_all(&mut self, now: u64) -> Vec<String> {
        let mut killed = Vec::new();
        for (id, rec) in &mut self.modules {
            if matches!(rec.status, Status::Enabled | Status::AutoDisabled { .. }) {
                rec.status = Status::Killed { since: now };
                killed.push(id.clone());
            }
        }
        killed
    }

    /// Kills one module (a per-module Hiss, or the follow-up to a runaway call).
    pub fn kill(&mut self, id: &str, now: u64) -> Result<(), HissError> {
        let rec = self
            .modules
            .get_mut(id)
            .ok_or_else(|| HissError::Unknown(id.to_owned()))?;
        if matches!(rec.status, Status::Enabled | Status::AutoDisabled { .. }) {
            rec.status = Status::Killed { since: now };
        }
        Ok(())
    }

    /// One-click re-enable of a soft state (auto-disabled or killed). A quarantine refuses, and a
    /// module that still needs consent has to go through the consent screen.
    pub fn re_enable(&mut self, id: &str) -> Result<(), HissError> {
        let rec = self
            .modules
            .get_mut(id)
            .ok_or_else(|| HissError::Unknown(id.to_owned()))?;
        match rec.status {
            Status::Quarantined { .. } => {
                Err(HissError::QuarantineNeedsAcknowledgement(id.to_owned()))
            }
            Status::NeedsConsent => Err(HissError::NeedsConsent(id.to_owned())),
            _ if !rec.consented => {
                rec.status = Status::NeedsConsent;
                Err(HissError::NeedsConsent(id.to_owned()))
            }
            _ => {
                rec.status = Status::Enabled;
                rec.bugs.clear();
                Ok(())
            }
        }
    }

    /// The explicit way out of a quarantine. `attempted` must be exactly what the warning showed:
    /// re-enabling is acknowledging *that*, not clicking through a generic dialog.
    pub fn acknowledge_violation(&mut self, id: &str, attempted: &str) -> Result<(), HissError> {
        let rec = self
            .modules
            .get_mut(id)
            .ok_or_else(|| HissError::Unknown(id.to_owned()))?;
        match &rec.status {
            Status::Quarantined { attempted: a, .. } if a == attempted => {
                rec.bugs.clear();
                if rec.consented {
                    rec.status = Status::Enabled;
                    Ok(())
                } else {
                    rec.status = Status::NeedsConsent;
                    Err(HissError::NeedsConsent(id.to_owned()))
                }
            }
            Status::Quarantined { .. } => {
                Err(HissError::QuarantineNeedsAcknowledgement(id.to_owned()))
            }
            _ => Err(HissError::NotQuarantined(id.to_owned())),
        }
    }

    pub fn with_policy(mut self, policy: Policy) -> Hiss {
        self.policy = policy;
        self
    }

    /// Writes the state atomically (temp file, then rename), so a crash mid-write leaves the old
    /// state, never a truncated one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut name = path.as_os_str().to_owned();
        name.push(".hiss-tmp");
        let tmp = PathBuf::from(name);
        {
            let mut f = fs::File::create(&tmp)?;
            io::Write::write_all(&mut f, &json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// Loads saved state. A missing file is an empty registry; a file that exists but cannot be
    /// read or parsed is an **error**, not an empty registry: silently forgetting a quarantine
    /// would re-enable the module that earned it. The caller treats the error as safe mode.
    pub fn load(path: &Path, policy: Policy) -> io::Result<Hiss> {
        match fs::read(path) {
            Ok(bytes) => {
                let h: Hiss = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(h.with_policy(policy))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Hiss::new(policy)),
            Err(e) => Err(e),
        }
    }
}

/// The file-backed "all third-party modules disabled" flag (Obsidian's Restricted Mode is the
/// precedent): checked before anything third-party loads, so it works when a bad module has made the
/// normal UI unusable. The flag is a marker file, not a setting that itself needs a working UI.
#[derive(Debug, Clone)]
pub struct SafeMode {
    path: PathBuf,
}

impl SafeMode {
    pub fn new(path: impl Into<PathBuf>) -> SafeMode {
        SafeMode { path: path.into() }
    }

    /// On if the marker exists, **and also if it cannot be checked** (permissions, I/O error):
    /// failing open would let a module run precisely when the user asked for it not to.
    pub fn is_on(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Windows reports a path *through a file* as NotFound too, so absence only counts
                // as off when the nearest existing ancestor is a directory.
                let mut dir = self.path.parent();
                while let Some(d) = dir {
                    match fs::metadata(d) {
                        Ok(m) => return !m.is_dir(),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => dir = d.parent(),
                        Err(_) => return true,
                    }
                }
                false
            }
            Err(_) => true,
        }
    }

    pub fn set(&self, on: bool) -> io::Result<()> {
        if on {
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(&self.path, b"third-party modules disabled\n")
        } else {
            match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

/// The one pre-launch gate: whether any third-party module may run at all.
pub fn modules_allowed(safe_mode: &SafeMode) -> bool {
    !safe_mode.is_on()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bug() -> Fault {
        Fault::Bug("trap".into())
    }

    fn enabled(h: &mut Hiss, id: &str) {
        h.install(id);
        h.consent(id).unwrap();
    }

    #[test]
    fn no_soft_state_re_enables_a_module_past_missing_consent() {
        // kill an unconsented module, then re-enable
        let mut h = Hiss::default();
        h.install("v.m");
        h.kill("v.m", 1).unwrap();
        assert!(h.re_enable("v.m").is_err());
        assert!(!h.may_run("v.m"));
        // a widening update while killed
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        h.kill("v.m", 1).unwrap();
        h.on_update("v.m", true);
        assert_eq!(
            h.re_enable("v.m"),
            Err(HissError::NeedsConsent("v.m".into()))
        );
        assert!(!h.may_run("v.m"));
        h.consent("v.m").unwrap();
        h.re_enable("v.m").unwrap();
        assert!(h.may_run("v.m"));
        // a widening update while quarantined
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        h.record_fault("v.m", Fault::Violation("net".into()), 1);
        h.on_update("v.m", true);
        assert!(h.acknowledge_violation("v.m", "net").is_err());
        assert!(!h.may_run("v.m"));
    }

    #[test]
    fn save_with_a_tmp_named_state_file_keeps_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hiss.tmp");
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        h.save(&p).unwrap();
        assert!(Hiss::load(&p, Policy::default()).unwrap().may_run("v.m"));
    }

    #[test]
    fn a_new_module_does_not_run_until_the_user_consents() {
        let mut h = Hiss::default();
        assert!(!h.may_run("v.m"), "unknown");
        h.install("v.m");
        assert!(!h.may_run("v.m"), "installed is not enabled");
        assert_eq!(h.status("v.m"), Some(&Status::NeedsConsent));
        h.consent("v.m").unwrap();
        assert!(h.may_run("v.m"));
        assert_eq!(h.consent("nope"), Err(HissError::Unknown("nope".into())));
    }

    #[test]
    fn repeated_bugs_inside_the_window_auto_disable_and_one_click_re_enables() {
        let mut h = Hiss::new(Policy {
            window_secs: 100,
            max_bugs: 3,
        });
        enabled(&mut h, "v.m");
        assert_eq!(h.record_fault("v.m", bug(), 10), Status::Enabled);
        assert_eq!(h.record_fault("v.m", bug(), 20), Status::Enabled);
        assert!(h.may_run("v.m"));
        let s = h.record_fault("v.m", bug(), 30);
        assert_eq!(
            s,
            Status::AutoDisabled {
                since: 30,
                failures: 3
            }
        );
        assert!(!h.may_run("v.m"));
        h.re_enable("v.m").unwrap();
        assert!(h.may_run("v.m"));
        // The count restarts: it takes three fresh bugs again.
        h.record_fault("v.m", bug(), 40);
        h.record_fault("v.m", bug(), 50);
        assert!(h.may_run("v.m"));
    }

    #[test]
    fn bugs_older_than_the_window_do_not_count() {
        let mut h = Hiss::new(Policy {
            window_secs: 100,
            max_bugs: 3,
        });
        enabled(&mut h, "v.m");
        h.record_fault("v.m", bug(), 0);
        h.record_fault("v.m", bug(), 50);
        // The first two are stale by now: this is only the second bug in the window.
        assert_eq!(h.record_fault("v.m", bug(), 200), Status::Enabled);
        assert_eq!(h.record_fault("v.m", bug(), 210), Status::Enabled);
        assert!(matches!(
            h.record_fault("v.m", bug(), 220),
            Status::AutoDisabled { .. }
        ));
    }

    #[test]
    fn a_violation_quarantines_at_once_and_only_an_exact_acknowledgement_clears_it() {
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        let s = h.record_fault(
            "v.m",
            Fault::Violation("connect evil.example.com".into()),
            5,
        );
        assert_eq!(
            s,
            Status::Quarantined {
                since: 5,
                attempted: "connect evil.example.com".into()
            }
        );
        assert!(!h.may_run("v.m"));
        assert_eq!(
            h.re_enable("v.m"),
            Err(HissError::QuarantineNeedsAcknowledgement("v.m".into()))
        );
        assert_eq!(
            h.acknowledge_violation("v.m", "something else"),
            Err(HissError::QuarantineNeedsAcknowledgement("v.m".into()))
        );
        assert!(!h.may_run("v.m"));
        h.acknowledge_violation("v.m", "connect evil.example.com")
            .unwrap();
        assert!(h.may_run("v.m"));
        assert_eq!(
            h.acknowledge_violation("v.m", "x"),
            Err(HissError::NotQuarantined("v.m".into()))
        );
    }

    #[test]
    fn a_quarantine_survives_an_update_a_consent_a_kill_and_more_faults() {
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        h.record_fault("v.m", Fault::Violation("write /etc/x".into()), 1);
        h.on_update("v.m", false);
        h.on_update("v.m", true);
        h.consent("v.m").unwrap();
        h.kill("v.m", 9).unwrap();
        h.kill_all(10);
        h.record_fault("v.m", Fault::Violation("another".into()), 11);
        h.record_fault("v.m", bug(), 12);
        assert_eq!(
            h.status("v.m"),
            Some(&Status::Quarantined {
                since: 1,
                attempted: "write /etc/x".into()
            }),
            "still quarantined, and it still says what the first attempt was"
        );
    }

    #[test]
    fn an_update_that_widens_authority_disables_until_re_consent_and_one_that_does_not_changes_nothing(
    ) {
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        h.on_update("v.m", false);
        assert!(h.may_run("v.m"));
        h.on_update("v.m", true);
        assert!(!h.may_run("v.m"));
        assert_eq!(h.status("v.m"), Some(&Status::NeedsConsent));
        assert_eq!(
            h.re_enable("v.m"),
            Err(HissError::NeedsConsent("v.m".into()))
        );
        h.consent("v.m").unwrap();
        assert!(h.may_run("v.m"));
    }

    #[test]
    fn the_kill_switch_stops_everything_that_runs_and_keeps_it_stopped() {
        let mut h = Hiss::default();
        enabled(&mut h, "a.one");
        enabled(&mut h, "b.two");
        h.install("c.three"); // never consented
        h.record_fault("b.two", bug(), 1);
        let mut killed = h.kill_all(100);
        killed.sort();
        assert_eq!(killed, ["a.one", "b.two"]);
        assert!(!h.may_run("a.one") && !h.may_run("b.two"));
        assert_eq!(h.status("a.one"), Some(&Status::Killed { since: 100 }));
        assert_eq!(h.status("c.three"), Some(&Status::NeedsConsent));
        // Consent does not undo a kill; an explicit re-enable does.
        h.consent("a.one").unwrap();
        assert!(!h.may_run("a.one"));
        h.re_enable("a.one").unwrap();
        assert!(h.may_run("a.one"));
    }

    #[test]
    fn state_round_trips_through_disk_and_a_corrupt_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hiss.json");
        assert_eq!(
            Hiss::load(&path, Policy::default()).unwrap(),
            Hiss::default(),
            "missing = empty"
        );
        let mut h = Hiss::default();
        enabled(&mut h, "v.m");
        enabled(&mut h, "w.n");
        h.record_fault("w.n", Fault::Violation("net".into()), 7);
        h.kill("v.m", 8).unwrap();
        h.save(&path).unwrap();
        let back = Hiss::load(&path, Policy::default()).unwrap();
        assert_eq!(back, h);
        assert!(!back.may_run("v.m") && !back.may_run("w.n"));
        // No half-written temp file is left behind.
        assert!(!dir.path().join("hiss.tmp").exists());
        // A damaged file is an error (the caller enters safe mode), never an empty registry that
        // would forget the quarantine.
        fs::write(&path, b"{ not json").unwrap();
        assert!(Hiss::load(&path, Policy::default()).is_err());
        fs::write(&path, b"").unwrap();
        assert!(Hiss::load(&path, Policy::default()).is_err());
    }

    #[test]
    fn safe_mode_is_a_marker_file_read_before_any_module_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let flag = SafeMode::new(dir.path().join("cfg").join("safe-mode"));
        assert!(!flag.is_on());
        assert!(modules_allowed(&flag));
        flag.set(true).unwrap();
        assert!(flag.is_on());
        assert!(!modules_allowed(&flag));
        flag.set(true).unwrap(); // idempotent
        flag.set(false).unwrap();
        assert!(modules_allowed(&flag));
        flag.set(false).unwrap(); // idempotent
                                  // A marker that cannot be checked counts as ON: a path *through a file* is not NotFound.
        let blocked = dir.path().join("plain");
        fs::write(&blocked, b"x").unwrap();
        let unreadable = SafeMode::new(blocked.join("safe-mode"));
        assert!(
            unreadable.is_on(),
            "an I/O error other than 'absent' must not read as 'off'"
        );
    }

    #[test]
    fn a_fault_on_a_module_the_registry_never_saw_does_not_enable_it() {
        let mut h = Hiss::default();
        assert_eq!(h.record_fault("x.y", bug(), 1), Status::NeedsConsent);
        assert!(!h.may_run("x.y"));
    }
}
