//! Long-running operations (format, flash, wipe, verify…) run as *jobs* on their own thread.
//! The UI polls [`status`] for a [`Snapshot`]: phase, progress, speed, ETA and the log lines
//! it hasn't seen yet. [`cancel`] asks a job to stop at its next [`JobCtx::check`].
//!
//! Engine code only ever sees a [`JobCtx`]: it announces phases, reports progress, logs, and
//! calls `check()` (or `advance()`, which checks too) often enough that cancelling is prompt.
//! Tests and the CLI use [`JobCtx::detached`].

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::ForgeError;

/// What a job is doing right now. The UI shows them as steps of a timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Hashing,
    Unmounting,
    Wiping,
    Partitioning,
    Formatting,
    Writing,
    Extracting,
    Verifying,
    Customizing,
    Finishing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    /// Monotonic sequence number: pass the last one seen as `since` to [`status`].
    pub seq: u64,
    /// Milliseconds since the job started.
    pub ms: u64,
    pub level: Level,
    pub msg: String,
}

/// Everything the UI needs to draw a job.
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub id: u64,
    pub kind: String,
    pub state: State,
    pub phase: Phase,
    /// Phases announced so far, in order (the timeline).
    pub phases: Vec<Phase>,
    /// Units done / total in the current phase (bytes unless `unit` says otherwise).
    pub done: u64,
    pub total: u64,
    pub unit: &'static str,
    /// Smoothed throughput of the current phase, in units per second.
    pub rate: f64,
    pub eta_secs: Option<u64>,
    pub elapsed_secs: f64,
    pub logs: Vec<LogLine>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

const MAX_LOG: usize = 5000;

struct Inner {
    kind: String,
    started: Instant,
    state: State,
    phase: Phase,
    phases: Vec<Phase>,
    total: u64,
    unit: &'static str,
    phase_started: Instant,
    /// (time, done) samples for the rate estimate, newest last.
    samples: VecDeque<(Instant, u64)>,
    logs: VecDeque<LogLine>,
    next_seq: u64,
    result: Option<Value>,
    error: Option<String>,
}

struct Shared {
    id: u64,
    cancel: AtomicBool,
    done: AtomicU64,
    inner: Mutex<Inner>,
    /// Also print log lines to stderr (CLI, tests).
    echo: bool,
}

/// Handle given to engine code.
#[derive(Clone)]
pub struct JobCtx {
    s: Arc<Shared>,
}

impl JobCtx {
    fn new(id: u64, kind: &str, echo: bool) -> Self {
        let now = Instant::now();
        JobCtx {
            s: Arc::new(Shared {
                id,
                cancel: AtomicBool::new(false),
                done: AtomicU64::new(0),
                echo,
                inner: Mutex::new(Inner {
                    kind: kind.into(),
                    started: now,
                    state: State::Running,
                    phase: Phase::Preparing,
                    phases: vec![Phase::Preparing],
                    total: 0,
                    unit: "bytes",
                    phase_started: now,
                    samples: VecDeque::new(),
                    logs: VecDeque::new(),
                    next_seq: 1,
                    result: None,
                    error: None,
                }),
            }),
        }
    }

    /// A context not registered anywhere (tests, CLI). Logs go to stderr when `echo`.
    pub fn detached(echo: bool) -> Self {
        JobCtx::new(0, "detached", echo)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.s.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enter `phase`, whose work amounts to `total` bytes (0 = unknown / indeterminate).
    pub fn phase(&self, phase: Phase, total: u64) {
        self.phase_units(phase, total, "bytes");
    }

    /// Like [`JobCtx::phase`] with another unit ("files", "passes", "steps"…).
    pub fn phase_units(&self, phase: Phase, total: u64, unit: &'static str) {
        self.s.done.store(0, Ordering::Relaxed);
        let mut i = self.lock();
        i.phase = phase;
        if i.phases.last() != Some(&phase) {
            i.phases.push(phase);
        }
        i.total = total;
        i.unit = unit;
        i.phase_started = Instant::now();
        i.samples.clear();
    }

    /// The total of the current phase became known (or changed).
    pub fn set_total(&self, total: u64) {
        self.lock().total = total;
    }

    /// `n` more units done. Fails with [`ForgeError::Cancelled`] once a cancel was requested.
    pub fn advance(&self, n: u64) -> Result<(), ForgeError> {
        self.s.done.fetch_add(n, Ordering::Relaxed);
        self.check()
    }

    pub fn set_done(&self, n: u64) {
        self.s.done.store(n, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.s.done.load(Ordering::Relaxed)
    }

    /// Err(Cancelled) if the job was asked to stop. Call it at least every few MB of I/O.
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.s.cancel.load(Ordering::Relaxed) {
            return Err(ForgeError::Cancelled);
        }
        Ok(())
    }

    pub fn is_cancelled(&self) -> bool {
        self.s.cancel.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.s.cancel.store(true, Ordering::Relaxed);
    }

    pub fn log(&self, level: Level, msg: impl Into<String>) {
        let msg = msg.into();
        if self.s.echo {
            eprintln!("[{level:?}] {msg}");
        }
        let mut i = self.lock();
        let line = LogLine { seq: i.next_seq, ms: i.started.elapsed().as_millis() as u64, level, msg };
        i.next_seq += 1;
        i.logs.push_back(line);
        while i.logs.len() > MAX_LOG {
            i.logs.pop_front();
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.log(Level::Info, msg)
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.log(Level::Warn, msg)
    }

    fn finish(&self, r: Result<Value, ForgeError>) {
        let mut i = self.lock();
        match r {
            Ok(v) => {
                i.state = State::Succeeded;
                i.result = Some(v);
            }
            Err(ForgeError::Cancelled) => i.state = State::Cancelled,
            Err(e) => {
                i.state = State::Failed;
                i.error = Some(e.to_string());
            }
        }
    }

    /// Snapshot with the log lines after `since`.
    pub fn snapshot(&self, since: u64) -> Snapshot {
        let done = self.done();
        let mut i = self.lock();
        let now = Instant::now();
        i.samples.push_back((now, done));
        // Rate over the last ~5 s of samples (the UI polls a few times per second).
        while i.samples.len() > 2 && now.duration_since(i.samples[0].0) > Duration::from_secs(5) {
            i.samples.pop_front();
        }
        let rate = match (i.samples.front(), i.samples.back()) {
            (Some(&(t0, d0)), Some(&(t1, d1))) if t1 > t0 && d1 >= d0 => {
                (d1 - d0) as f64 / t1.duration_since(t0).as_secs_f64()
            }
            _ => 0.0,
        };
        let rate = if rate == 0.0 && done > 0 {
            done as f64 / i.phase_started.elapsed().as_secs_f64().max(0.001)
        } else {
            rate
        };
        let eta_secs = (i.total > done && rate > 0.0 && i.state == State::Running).then(|| ((i.total - done) as f64 / rate).ceil() as u64);
        Snapshot {
            id: self.s.id,
            kind: i.kind.clone(),
            state: i.state,
            phase: i.phase,
            phases: i.phases.clone(),
            done,
            total: i.total,
            unit: i.unit,
            rate,
            eta_secs,
            elapsed_secs: i.started.elapsed().as_secs_f64(),
            logs: i.logs.iter().filter(|l| l.seq > since).cloned().collect(),
            result: i.result.clone(),
            error: i.error.clone(),
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static JOBS: once_cell::sync::Lazy<Mutex<HashMap<u64, JobCtx>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// Run `f` on a new thread as job `kind`. Returns its id at once.
pub fn spawn<F>(kind: &str, f: F) -> u64
where
    F: FnOnce(&JobCtx) -> Result<Value, ForgeError> + Send + 'static,
{
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctx = JobCtx::new(id, kind, false);
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).insert(id, ctx.clone());
    std::thread::Builder::new()
        .name(format!("lumendisk-job-{id}-{kind}"))
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx)))
                .unwrap_or_else(|p| {
                    let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    Err(ForgeError::Io(format!("internal error: {msg}")))
                });
            if let Err(e) = &r {
                if !matches!(e, ForgeError::Cancelled) {
                    ctx.log(Level::Error, e.to_string());
                }
            }
            ctx.finish(r);
        })
        .expect("spawn job thread");
    id
}

pub fn status(id: u64, since: u64) -> Option<Snapshot> {
    let ctx = JOBS.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()?;
    Some(ctx.snapshot(since))
}

pub fn cancel(id: u64) -> bool {
    match JOBS.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
        Some(c) => {
            c.cancel();
            true
        }
        None => false,
    }
}

/// Forget a finished job (the UI calls it once it has shown the result).
pub fn forget(id: u64) -> bool {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    match jobs.get(&id) {
        Some(c) if c.lock().state != State::Running => jobs.remove(&id).is_some(),
        _ => false,
    }
}

/// Ids of the jobs still running (at most one write to a given disk at a time is enforced by
/// `target::claim`, not here).
pub fn running() -> Vec<u64> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, c)| c.lock().state == State::Running).map(|(id, _)| *id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(id: u64) -> Snapshot {
        for _ in 0..500 {
            let s = status(id, 0).unwrap();
            if s.state != State::Running {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("job {id} did not finish");
    }

    #[test]
    fn job_reports_phases_progress_logs_and_result() {
        let id = spawn("test", |ctx| {
            ctx.phase(Phase::Writing, 1000);
            for _ in 0..10 {
                ctx.advance(100)?;
            }
            ctx.info("written");
            ctx.phase_units(Phase::Verifying, 3, "files");
            ctx.advance(3)?;
            Ok(serde_json::json!({"bytes": 1000}))
        });
        let s = wait(id);
        assert_eq!(s.state, State::Succeeded);
        assert_eq!(s.phases, vec![Phase::Preparing, Phase::Writing, Phase::Verifying]);
        assert_eq!((s.done, s.total, s.unit), (3, 3, "files"));
        assert_eq!(s.result.unwrap()["bytes"], 1000);
        assert_eq!(s.logs.len(), 1);
        let later = status(id, s.logs[0].seq).unwrap();
        assert!(later.logs.is_empty(), "`since` filters lines already seen");
        assert!(forget(id));
        assert!(status(id, 0).is_none());
    }

    #[test]
    fn cancel_stops_at_the_next_check_and_failures_and_panics_are_reported() {
        let id = spawn("cancel", |ctx| {
            ctx.phase(Phase::Wiping, 0);
            loop {
                ctx.advance(1)?;
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(cancel(id));
        assert_eq!(wait(id).state, State::Cancelled);

        let id = spawn("fail", |_| Err(ForgeError::Invalid("bad input".into())));
        let s = wait(id);
        assert_eq!(s.state, State::Failed);
        assert!(s.error.unwrap().contains("bad input"));

        let id = spawn("panic", |_| panic!("boom"));
        let s = wait(id);
        assert_eq!(s.state, State::Failed);
        assert!(s.error.unwrap().contains("boom"));
    }
}
