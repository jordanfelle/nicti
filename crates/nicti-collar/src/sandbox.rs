//! A wasmtime sandbox that a kill interrupts mid-execution (ADR-0214 section 4).
//!
//! One `Engine` **per module** (the ADR's "simplest" isolation: `Engine::increment_epoch` is a
//! counter shared by every `Store` on that engine, so killing one module must not share an engine
//! with another). Every call runs with fuel, an epoch deadline, a wall-clock epoch budget enforced
//! by a watchdog, and a memory cap, so a runaway loop is stopped whether the user pressed the kill
//! switch or the module simply never returns. The outcome is classified for [`crate::hiss`]:
//!
//! - `Returned`: it finished.
//! - `Killed`: the user's kill switch (a deliberate stop, not a fault).
//! - `Bug`: a trap, fuel or budget exhaustion: a correctly scoped call that went wrong.
//! - `Violation`: the module imports something it was not given. No imports are provided here
//!   (WASI linking is a follow-up), so any import is a reach for authority it never declared.
//!
//! Limits of "immediate" that ADR-0214 states and this does not remove: epoch checks run in guest
//! code only (a blocked *host* call is bounded by that call's own timeout), and a very large
//! `memory.copy`/`memory.fill` is a native loop without a per-byte check; the memory cap bounds it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, Trap};

use crate::hiss::Fault;
use crate::manifest::Budget;

/// What a call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Returned(i64),
    Killed,
    Bug(String),
    Violation(String),
}

impl Outcome {
    /// The fault [`crate::hiss::Hiss::record_fault`] should be told about, if any. A normal return
    /// and a deliberate kill are not faults.
    pub fn fault(&self) -> Option<Fault> {
        match self {
            Outcome::Returned(_) | Outcome::Killed => None,
            Outcome::Bug(why) => Some(Fault::Bug(why.clone())),
            Outcome::Violation(what) => Some(Fault::Violation(what.clone())),
        }
    }
}

/// A module's sandbox: its own engine and its budget.
pub struct Sandbox {
    engine: Engine,
    budget: Budget,
    killed: Arc<AtomicBool>,
}

/// A handle the kill switch holds; cloneable and usable from any thread.
#[derive(Clone)]
pub struct Killer {
    engine: Engine,
    killed: Arc<AtomicBool>,
}

impl Killer {
    /// Interrupts the module's running call (if any) and keeps its future calls from starting.
    pub fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        self.engine.increment_epoch();
    }
}

struct Data {
    limits: StoreLimits,
}

impl Sandbox {
    pub fn new(budget: Budget) -> Result<Sandbox, String> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        Ok(Sandbox {
            engine,
            budget,
            killed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn killer(&self) -> Killer {
        Killer {
            engine: self.engine.clone(),
            killed: Arc::clone(&self.killed),
        }
    }

    /// Whether the kill switch has been pressed for this module.
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// Runs the exported no-argument function `export` (returning `i32` or `i64`) of the module in
    /// `wasm` (binary, or WAT text). Never panics on a hostile module; every failure is an `Outcome`.
    pub fn run(&self, wasm: &[u8], export: &str) -> Outcome {
        if self.is_killed() {
            return Outcome::Killed;
        }
        let module = match Module::new(&self.engine, wasm) {
            Ok(m) => m,
            Err(e) => return Outcome::Bug(format!("not a valid module: {e}")),
        };
        // No imports are provided: anything the module asks for is authority it never declared.
        if let Some(import) = module.imports().next() {
            return Outcome::Violation(format!(
                "imports {}::{} (no imports are provided to a module)",
                import.module(),
                import.name()
            ));
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.budget.effective_memory_mb() as usize * 1024 * 1024)
            .instances(1)
            .tables(8)
            .memories(2)
            .trap_on_grow_failure(false)
            .build();
        let mut store = Store::new(&self.engine, Data { limits });
        store.limiter(|d| &mut d.limits);
        if store.set_fuel(self.budget.effective_fuel()).is_err() {
            return Outcome::Bug("could not set the fuel budget".into());
        }
        // The deadline is one epoch from *now*; a kill that landed before this line is caught by the
        // flag check just below, one that lands after it by the epoch.
        store.set_epoch_deadline(1);
        if self.is_killed() {
            return Outcome::Killed;
        }

        // Watchdog: the wall-clock budget. It bumps the epoch once if the call is still running.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let engine = self.engine.clone();
        let budget = Duration::from_millis(self.budget.effective_epoch_budget_ms());
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_w = Arc::clone(&timed_out);
        let watchdog = thread::spawn(move || {
            if done_rx.recv_timeout(budget).is_err() {
                timed_out_w.store(true, Ordering::SeqCst);
                engine.increment_epoch();
            }
        });

        let result = (|| -> wasmtime::Result<i64> {
            let instance = Instance::new(&mut store, &module, &[])?;
            if let Ok(f) = instance.get_typed_func::<(), i64>(&mut store, export) {
                return f.call(&mut store, ());
            }
            let f = instance.get_typed_func::<(), i32>(&mut store, export)?;
            Ok(i64::from(f.call(&mut store, ())?))
        })();
        let _ = done_tx.send(());
        let _ = watchdog.join();

        match result {
            Ok(v) => Outcome::Returned(v),
            Err(e) => match e.downcast_ref::<Trap>() {
                Some(Trap::Interrupt) if self.is_killed() => Outcome::Killed,
                Some(Trap::Interrupt) if timed_out.load(Ordering::SeqCst) => Outcome::Bug(format!(
                    "exceeded its {} ms budget",
                    self.budget.effective_epoch_budget_ms()
                )),
                Some(Trap::OutOfFuel) => Outcome::Bug("exhausted its fuel budget".into()),
                Some(trap) => Outcome::Bug(format!("trapped: {trap}")),
                None => Outcome::Bug(format!("call failed: {e}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const SPIN: &str =
        r#"(module (func (export "run") (result i32) (loop $l (br $l)) (i32.const 0)))"#;
    const ADD: &str =
        r#"(module (func (export "run") (result i32) (i32.add (i32.const 40) (i32.const 2))))"#;
    const WIDE: &str = r#"(module (func (export "run") (result i64) (i64.const 4294967296)))"#;

    fn budget(fuel: u64, ms: u64, mb: u32) -> Budget {
        Budget {
            fuel_limit: Some(fuel),
            epoch_budget_ms: Some(ms),
            memory_mb_max: Some(mb),
        }
    }

    /// Runs `f` on a thread and waits for it for at most `deadline`; a hang fails the test instead
    /// of hanging the suite.
    fn within<T: Send + 'static>(deadline: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(deadline)
            .expect("the call did not finish in time")
    }

    #[test]
    fn outcomes_feed_hiss_end_to_end() {
        use crate::hiss::{Hiss, Policy, Status};
        let mut hiss = Hiss::new(Policy {
            window_secs: 1000,
            max_bugs: 3,
        });
        hiss.install("v.m");
        hiss.consent("v.m").unwrap();
        let sb = Sandbox::new(Budget::default()).unwrap();
        let trap = r#"(module (func (export "run") (result i32) unreachable))"#;

        // A good call and a deliberate kill are not faults.
        assert_eq!(sb.run(ADD.as_bytes(), "run").fault(), None);
        // Three bugs in the window auto-disable.
        for t in 0..3 {
            let fault = sb
                .run(trap.as_bytes(), "run")
                .fault()
                .expect("a trap is a bug");
            hiss.record_fault("v.m", fault, t);
        }
        assert!(matches!(
            hiss.status("v.m"),
            Some(Status::AutoDisabled { failures: 3, .. })
        ));
        hiss.re_enable("v.m").unwrap();
        // One violation quarantines, naming what it reached for.
        let reach = r#"(module (import "env" "steal" (func)) (func (export "run") (result i32) (i32.const 0)))"#;
        let fault = sb
            .run(reach.as_bytes(), "run")
            .fault()
            .expect("an import is a violation");
        hiss.record_fault("v.m", fault, 10);
        assert!(
            matches!(hiss.status("v.m"), Some(Status::Quarantined { attempted, .. }) if attempted.contains("env::steal"))
        );
        assert!(!hiss.may_run("v.m"));
    }

    #[test]
    fn a_well_behaved_module_returns_its_value() {
        let sb = Sandbox::new(Budget::default()).unwrap();
        assert_eq!(sb.run(ADD.as_bytes(), "run"), Outcome::Returned(42));
        assert_eq!(
            sb.run(WIDE.as_bytes(), "run"),
            Outcome::Returned(4_294_967_296)
        );
    }

    #[test]
    fn the_kill_switch_stops_a_tight_loop_that_never_calls_the_host() {
        // The ADR's claim: epoch checks sit at loop back-edges, so a loop with no host-call boundary
        // is interrupted. Generous fuel and budget: only the kill can end this call.
        let sb = Arc::new(Sandbox::new(budget(MAX, 60_000, 64)).unwrap());
        let killer = sb.killer();
        let started = Instant::now();
        let worker = {
            let sb = Arc::clone(&sb);
            thread::spawn(move || sb.run(SPIN.as_bytes(), "run"))
        };
        thread::sleep(Duration::from_millis(100));
        killer.kill();
        let outcome = within(Duration::from_secs(30), move || worker.join().unwrap());
        assert_eq!(outcome, Outcome::Killed);
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(sb.is_killed());
    }

    const MAX: u64 = crate::manifest::MAX_FUEL;

    #[test]
    fn a_kill_that_lands_before_the_call_starts_is_not_lost() {
        let sb = Sandbox::new(budget(MAX, 60_000, 64)).unwrap();
        sb.killer().kill();
        let out = within(Duration::from_secs(10), move || {
            sb.run(SPIN.as_bytes(), "run")
        });
        assert_eq!(
            out,
            Outcome::Killed,
            "a killed module's later calls never start"
        );
    }

    #[test]
    fn a_module_that_never_returns_is_stopped_by_its_budget_and_that_is_a_bug_not_a_kill() {
        let sb = Sandbox::new(budget(MAX, 200, 64)).unwrap();
        let out = within(Duration::from_secs(30), move || {
            sb.run(SPIN.as_bytes(), "run")
        });
        assert!(
            matches!(&out, Outcome::Bug(m) if m.contains("200 ms")),
            "{out:?}"
        );
    }

    #[test]
    fn running_out_of_fuel_is_a_bug() {
        let sb = Sandbox::new(budget(10_000, 60_000, 64)).unwrap();
        let out = within(Duration::from_secs(30), move || {
            sb.run(SPIN.as_bytes(), "run")
        });
        assert_eq!(out, Outcome::Bug("exhausted its fuel budget".into()));
    }

    #[test]
    fn a_trap_and_a_bad_export_and_garbage_are_bugs_never_panics() {
        let sb = Sandbox::new(Budget::default()).unwrap();
        let trap = r#"(module (func (export "run") (result i32) unreachable))"#;
        assert!(matches!(sb.run(trap.as_bytes(), "run"), Outcome::Bug(m) if m.contains("trapped")));
        let div = r#"(module (func (export "run") (result i32) (i32.div_s (i32.const 1) (i32.const 0))))"#;
        assert!(matches!(sb.run(div.as_bytes(), "run"), Outcome::Bug(_)));
        assert!(matches!(sb.run(ADD.as_bytes(), "missing"), Outcome::Bug(_)));
        assert!(
            matches!(sb.run(b"\x00asm garbage", "run"), Outcome::Bug(m) if m.contains("not a valid module"))
        );
        assert!(matches!(sb.run(b"", "run"), Outcome::Bug(_)));
    }

    #[test]
    fn a_module_that_asks_for_an_import_it_was_never_given_is_a_violation() {
        let sb = Sandbox::new(Budget::default()).unwrap();
        for wat in [
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (func (export "run") (result i32) (i32.const 0)))"#,
            r#"(module (import "env" "steal" (func)) (func (export "run") (result i32) (i32.const 0)))"#,
        ] {
            let out = sb.run(wat.as_bytes(), "run");
            assert!(
                matches!(&out, Outcome::Violation(m) if m.contains("imports")),
                "{out:?}"
            );
        }
        let Outcome::Violation(m) = sb.run(
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)) (func (export "run") (result i32) (i32.const 0)))"#.as_bytes(),
            "run",
        ) else {
            panic!()
        };
        assert!(
            m.contains("wasi_snapshot_preview1::fd_write"),
            "names exactly what was reached for: {m}"
        );
    }

    #[test]
    fn memory_is_capped_by_the_budget() {
        // 1 MiB cap; the module starts with 1 page (64 KiB) and tries to grow by 100 pages (6.25 MiB).
        let grow = r#"(module (memory 1) (func (export "run") (result i32) (memory.grow (i32.const 100))))"#;
        let sb = Sandbox::new(budget(MAX, 60_000, 1)).unwrap();
        assert_eq!(
            sb.run(grow.as_bytes(), "run"),
            Outcome::Returned(-1),
            "grow refused, no crash"
        );
        let ok =
            r#"(module (memory 1) (func (export "run") (result i32) (memory.grow (i32.const 2))))"#;
        assert_eq!(
            sb.run(ok.as_bytes(), "run"),
            Outcome::Returned(1),
            "growth within the cap works"
        );
        // A module whose *initial* memory exceeds the cap does not instantiate.
        let big = r#"(module (memory 100) (func (export "run") (result i32) (i32.const 1)))"#;
        assert!(matches!(sb.run(big.as_bytes(), "run"), Outcome::Bug(_)));
    }

    #[test]
    fn killing_one_module_never_interrupts_another() {
        let a = Arc::new(Sandbox::new(budget(MAX, 60_000, 64)).unwrap());
        let b = Arc::new(Sandbox::new(budget(MAX, 400, 64)).unwrap());
        let (ka, kb) = (a.killer(), b.killer());
        let ta = {
            let a = Arc::clone(&a);
            thread::spawn(move || a.run(SPIN.as_bytes(), "run"))
        };
        let tb = {
            let b = Arc::clone(&b);
            thread::spawn(move || b.run(SPIN.as_bytes(), "run"))
        };
        thread::sleep(Duration::from_millis(100));
        ka.kill();
        let out_a = within(Duration::from_secs(30), move || ta.join().unwrap());
        assert_eq!(out_a, Outcome::Killed);
        // B was never killed: it ends on its own 400 ms budget, as a bug, not as a kill.
        let out_b = within(Duration::from_secs(30), move || tb.join().unwrap());
        assert!(
            matches!(&out_b, Outcome::Bug(m) if m.contains("400 ms")),
            "{out_b:?}"
        );
        assert!(!b.is_killed());
        let _ = kb;
    }

    #[test]
    fn killing_after_a_call_finished_changes_nothing_about_that_call_but_blocks_the_next() {
        let sb = Sandbox::new(Budget::default()).unwrap();
        assert_eq!(sb.run(ADD.as_bytes(), "run"), Outcome::Returned(42));
        sb.killer().kill();
        assert_eq!(sb.run(ADD.as_bytes(), "run"), Outcome::Killed);
    }
}
