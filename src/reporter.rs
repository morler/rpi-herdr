//! The reporting executor: a single background thread that sends the latest
//! state to Herdr and drops everything older.
//!
//! The protocol's hard requirement — "never slow the agent down" — shapes the
//! whole design:
//!
//! - Event handlers only publish into a one-slot, overwriting mailbox and
//!   return. No process spawning, no locks held across I/O, no sleeping.
//! - Exactly one thread spawns `herdr`, so a burst of transitions collapses to
//!   the newest one instead of queueing a backlog.
//! - Every invocation has a deadline and failures are ignored, because Herdr
//!   explicitly permits dropping reports.
//! - `seq` is timestamp-derived and monotonic, so it keeps increasing across
//!   sessions, restarts and `/reload` (a fresh cdylib starts a new static).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::env::HerdrEnv;
use crate::protocol::{self, Report, Target};
use crate::resume::{self, ExecContext};
use crate::state::{AgentState, Input, Machine};

/// How long one `herdr` call may take before it is killed and forgotten.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound for `finish_shutdown`. Two seconds of exec plus scheduling slack.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound for pushing the final release out while still inside an event
/// handler. Comfortably below the 15 s `SessionShutdown` budget.
pub const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// How often the current state is re-sent even though it did not change.
///
/// Herdr drops a pane's agent authority on its own — its process detection
/// settles, an unrelated report on the same pane releases the slot, the server
/// reloads its config, `/reload` swaps this library out — and a plugin that
/// only speaks on transitions then stays invisible until the *next* transition,
/// which can be minutes. A cheap repeat keeps the pane honest; ten seconds is
/// well inside the window a user would notice, and one `herdr` call per beat is
/// noise next to a running agent.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// One pending report. Later submissions replace earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    State {
        state: AgentState,
        message: Option<String>,
        session_id: Option<String>,
    },
    Release,
}

impl Request {
    /// A coalescing key: two requests with the same key are interchangeable, so
    /// submitting the second is a no-op. `Release` is always distinct.
    fn same_intent(&self, other: &Request) -> bool {
        match (self, other) {
            (Request::Release, Request::Release) => true,
            (
                Request::State {
                    state: a,
                    message: am,
                    session_id: asid,
                },
                Request::State {
                    state: b,
                    message: bm,
                    session_id: bsid,
                },
            ) => a == b && am == bm && asid == bsid,
            _ => false,
        }
    }
}

/// Runs one argv to completion (or to the deadline). Injectable so the coalescing
/// semantics can be tested without a Herdr binary or a host.
pub trait Runner: Send + Sync + 'static {
    fn run(&self, argv: &[String], deadline: Duration);
}

/// The real runner: spawn `HERDR_BIN_PATH` with all three stdio streams nulled.
///
/// Inheriting stdio is not an option — the TUI owns the tty and `herdr` writes
/// JSON to stdout on success, which would corrupt the screen.
#[derive(Debug)]
pub struct ProcessRunner {
    bin_path: PathBuf,
}

impl ProcessRunner {
    pub fn new(bin_path: PathBuf) -> Self {
        Self { bin_path }
    }
}

impl Runner for ProcessRunner {
    fn run(&self, argv: &[String], deadline: Duration) {
        let mut command = std::process::Command::new(&self.bin_path);
        command
            .args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        // A console window flashing on every report would be unusable on
        // Windows; this keeps the child invisible.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let Ok(mut child) = command.spawn() else {
            return; // spawn failure: ignored, per the protocol
        };

        let until = Instant::now() + deadline;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return, // any exit code, including failure
                Ok(None) => {}
                Err(_) => {
                    // `try_wait` failed, so the child's fate is unknown: reap it
                    // instead of leaving a live process with detached stdio (and
                    // a zombie on Unix) that no timeout applies to any more.
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Reads live execution metadata from the host when a run starts.
pub trait ContextSource: Send + Sync + 'static {
    /// `None` means "the host could not answer"; callers then fall back to a
    /// command with just `--session-id`.
    fn refresh(&self) -> Option<ExecContext>;
}

/// State shared between the handler side and the worker thread.
///
/// One lock guards the machine and the cached session id together: a heartbeat
/// must see exactly the state a concurrent transition just declared, not a
/// half-updated pair.
struct Shared {
    /// Set once the host proves an interactive TUI is running, either by
    /// dispatching `BeforeTuiStart` or because the host's argv says so. Until
    /// then nothing is claimed: a `-p`/`--mode json` run has no PTY Herdr could
    /// display, and claiming the pane there would steal the slot from the TUI
    /// that is already using it — its eventual release is what used to blank a
    /// *working* agent out of the sidebar.
    interactive: AtomicBool,
    /// Set once a release was published, so `SessionShutdown` followed by the
    /// lifecycle hook does not spawn a second `herdr` for the same outcome.
    /// Cleared as soon as a new state is reported.
    released: AtomicBool,
    /// Whether `SessionStart` was ever dispatched to *this* instance.
    ///
    /// `/reload` loads a new library and sends it `SessionShutdown` without ever
    /// sending `SessionStart`, so this is what tells a real exit apart from that
    /// fake one. Guarding on it is what keeps a reload from blanking a running
    /// agent out of the sidebar.
    saw_session_start: AtomicBool,
    /// The declared state, plus whether a run is in flight.
    machine: Mutex<Machine>,
    /// The session id from the newest event, so a heartbeat can repeat it
    /// instead of re-deriving it from the host on every beat.
    session_id: Mutex<Option<String>>,
}

/// Owning handle for the reporting thread. Dropping it signals shutdown.
pub struct Reporter {
    slot: Arc<(Mutex<Option<Request>>, Condvar)>,
    stop: Arc<AtomicBool>,
    /// True while the worker is inside `runner.run(..)` for one request.
    busy: Arc<AtomicBool>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Sticky record of a `shutdown()` that could not join the worker. Once set,
    /// every later `shutdown()` keeps reporting failure: the host must not unload
    /// a library whose thread may still run plugin code.
    join_failed: AtomicBool,
}

impl Reporter {
    /// Start the reporting thread. `env` supplies the pane identity, `runner`
    /// performs the calls and `context` provides resume metadata.
    pub fn start(
        env: HerdrEnv,
        runner: Arc<dyn Runner>,
        context: Arc<dyn ContextSource>,
    ) -> Arc<Self> {
        Self::start_with_heartbeat(env, runner, context, HEARTBEAT_INTERVAL)
    }

    /// Start and, when this process drives a TUI, claim the pane immediately.
    ///
    /// The claim cannot wait for an event: `/reload` loads this library
    /// mid-session without re-dispatching `BeforeTuiStart` or `SessionStart`, so
    /// an event-gated plugin stays invisible until the next turn — which for an
    /// idle agent can be hours.
    pub fn start_in_current_mode(
        env: HerdrEnv,
        runner: Arc<dyn Runner>,
        context: Arc<dyn ContextSource>,
    ) -> Arc<Self> {
        let reporter = Self::start(env, runner, context);
        if crate::env::is_interactive_process() {
            reporter.enter_interactive();
        }
        reporter
    }

    /// [`Self::start`] with an explicit heartbeat period, so tests do not have to
    /// wait ten real seconds for the second beat.
    fn start_with_heartbeat(
        env: HerdrEnv,
        runner: Arc<dyn Runner>,
        context: Arc<dyn ContextSource>,
        heartbeat: Duration,
    ) -> Arc<Self> {
        let slot = Arc::new((Mutex::new(None::<Request>), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(SeqGen::default());
        let shared = Arc::new(Shared {
            interactive: AtomicBool::new(false),
            released: AtomicBool::new(false),
            saw_session_start: AtomicBool::new(false),
            machine: Mutex::new(Machine::default()),
            session_id: Mutex::new(None),
        });

        let thread = {
            let worker = Worker {
                env,
                runner,
                context,
                slot: Arc::clone(&slot),
                stop: Arc::clone(&stop),
                busy: Arc::clone(&busy),
                seq: Arc::clone(&seq),
                shared: Arc::clone(&shared),
                heartbeat,
                exec_context: None,
                refreshed_for_run: false,
            };
            std::thread::Builder::new()
                .name("rpi-herdr".to_string())
                // A failure to spawn a thread must not take the agent down; the
                // plugin then simply reports nothing.
                .spawn(move || worker_loop(worker))
                .ok()
        };

        Arc::new(Self {
            slot,
            stop,
            busy,
            shared,
            thread: Mutex::new(thread),
            join_failed: AtomicBool::new(false),
        })
    }

    /// Enter interactive mode: from here on the pane is claimed and kept alive by
    /// the heartbeat.
    ///
    /// Idempotent and cheap to repeat: start-up calls it once from the argv check,
    /// `BeforeTuiStart` calls it again. The state machine is untouched and the
    /// mailbox is a single slot, so the second call collapses into the first when
    /// the first report has not left yet.
    pub fn enter_interactive(&self) {
        self.shared.interactive.store(true, Ordering::Release);
        // Claim immediately instead of waiting for the first prompt: between
        // start-up and the first keystroke the pane otherwise carries no agent at
        // all, which is the "rpi is running but not in the list" report.
        self.publish_current();
    }

    /// Record that `SessionStart` reached *this* instance.
    ///
    /// This is what separates a real shutdown from the one `/reload` fakes: the
    /// host dispatches a `SessionShutdown` to the freshly loaded library without
    /// ever sending it a `SessionStart`, so a release from an instance that never
    /// saw the start would blank a session that is in fact still running.
    pub fn note_session_start(&self) {
        self.shared.saw_session_start.store(true, Ordering::Release);
    }

    /// Queue the current declared state, bypassing the "did it change" filter.
    /// Used for the initial claim and every heartbeat.
    fn publish_current(&self) {
        if self.stop.load(Ordering::Acquire)
            || !self.shared.interactive.load(Ordering::Acquire)
            || self.shared.released.load(Ordering::Acquire)
        {
            return;
        }
        let (state, session_id) = {
            let machine = self
                .shared
                .machine
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session_id = self
                .shared
                .session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            (machine.state(), session_id)
        };
        self.shared.released.store(false, Ordering::Release);
        self.publish(Request::State {
            state,
            message: None,
            session_id,
        });
    }

    /// Feed one input to the state machine and publish the resulting state.
    ///
    /// Never blocks on I/O: the handler path stays in the microsecond range so
    /// event-log budgets (10 s for `BeforeAgentStart`) are never approached.
    pub fn submit(&self, input: Input, session_id: Option<String>) {
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        let interactive = self.shared.interactive.load(Ordering::Acquire);
        let (state, changed) = {
            let mut machine = self
                .shared
                .machine
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let next = machine.next(input);
            let changed = next.state() != machine.state();
            *machine = next;
            if session_id.is_some() {
                *self
                    .shared
                    .session_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = session_id.clone();
            }
            (next.state(), changed)
        };
        // Nothing is claimed outside a TUI, so a transition there is news for
        // nobody: a one-shot run would only fight the TUI for the same slot.
        if !interactive {
            return;
        }
        // Report only real transitions, so a chatty event stream cannot make the
        // pane flicker between identical states.
        if !changed {
            return;
        }
        self.shared.released.store(false, Ordering::Release);
        self.publish(Request::State {
            state,
            message: None,
            session_id,
        });
    }

    /// Ask Herdr to forget this agent. Idempotent and always sends, so a release
    /// cannot be swallowed by the "unchanged" filter.
    pub fn submit_release(&self) {
        {
            let mut machine = self
                .shared
                .machine
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *machine = Machine::default();
        }
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        // A non-interactive run never claimed the pane, so it has nothing to
        // release — and releasing anyway is what used to wipe the parent TUI's
        // ownership out of the sidebar.
        if !self.shared.interactive.load(Ordering::Acquire) {
            return;
        }
        // A `/reload` dispatches `SessionShutdown` to the freshly loaded library
        // even though the session continues. That instance never saw
        // `SessionStart`, so its release would blank a *running* agent — the
        // exact "rpi is working but not in the list" symptom. Real exits are
        // unaffected: every ordinary session gets `SessionStart` first.
        if !self.shared.saw_session_start.load(Ordering::Acquire) {
            return;
        }
        // Idempotent: Herdr accepts repeated releases, but a second spawn buys
        // nothing. A later state report clears the flag and re-claims the pane.
        if self.shared.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.publish(Request::Release);
    }

    /// Overwrite whatever is queued. A late older request can never clobber a
    /// newer one because the mailbox holds exactly one entry.
    fn publish(&self, request: Request) {
        if self.stop.load(Ordering::Acquire) {
            return; // a stopping reporter never starts new work
        }
        let (lock, cv) = &*self.slot;
        let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|queued| queued.same_intent(&request))
        {
            return; // nothing new to say
        }
        *slot = Some(request);
        drop(slot);
        cv.notify_one();
    }

    /// Mark this reporter interactive *without* the initial claim, so a test can
    /// count the calls a transition produces on its own.
    #[cfg(test)]
    pub(crate) fn mark_interactive_silently(&self) {
        self.shared.interactive.store(true, Ordering::Release);
        self.shared.saw_session_start.store(true, Ordering::Release);
    }

    /// Exactly what a `/reload`ed instance looks like: interactive, but it never
    /// received a `SessionStart`.
    #[cfg(test)]
    fn interactive_without_session_start(&self) {
        self.shared.interactive.store(true, Ordering::Release);
    }

    /// Block (up to `timeout`) until nothing is queued and no call is in
    /// flight. Used at session shutdown, where the host may exit the process
    /// immediately after dispatching `SessionShutdown`: without this, the
    /// release request could be lost. Bounded, so a wedged `herdr` cannot hang
    /// the exit.
    pub fn flush(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            let queue_empty = self
                .slot
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none();
            if queue_empty && !self.busy.load(Ordering::Acquire) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stop reporting and wait for the thread. Returns `true` when the thread is
    /// known to be gone, which is what lets the host unload this library safely.
    ///
    /// A request that is already queued is still delivered: `begin_shutdown`
    /// publishes the release and this call must not drop it. Only requests
    /// published *after* this point are ignored, which is exactly what keeps a
    /// stopping plugin from starting new work.
    pub fn shutdown(&self) -> bool {
        self.stop.store(true, Ordering::Release);
        let (_, cv) = &*self.slot;
        cv.notify_all();

        // A previous attempt could not join the worker. That fact is sticky: the
        // thread may still execute plugin code, so the host must keep this
        // library loaded no matter how often it asks again.
        if self.join_failed.load(Ordering::Acquire) {
            return false;
        }

        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(handle) = handle else {
            return true;
        };
        let until = Instant::now() + SHUTDOWN_TIMEOUT;
        while !handle.is_finished() {
            if Instant::now() >= until {
                // Still running: hand the handle back so a retry can still join
                // it, and record the failure so every later call keeps saying
                // "not unloaded".
                self.join_failed.store(true, Ordering::Release);
                *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match handle.join() {
            Ok(()) => true,
            // A panicked worker is gone (its stack unwound), so unloading is
            // safe; report success so the host is not stuck forever.
            Err(_) => true,
        }
    }
}

/// Milliseconds since the Unix epoch, clamped to `0` before it.
pub fn millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Everything the worker thread owns. Bundled so the loop keeps a readable
/// signature and the per-run caches live in one place.
struct Worker {
    env: HerdrEnv,
    runner: Arc<dyn Runner>,
    context: Arc<dyn ContextSource>,
    slot: Arc<(Mutex<Option<Request>>, Condvar)>,
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    seq: Arc<SeqGen>,
    shared: Arc<Shared>,
    heartbeat: Duration,
    /// Host metadata, refreshed once per run.
    exec_context: Option<ExecContext>,
    refreshed_for_run: bool,
}

/// The reporting loop. The only place that spawns `herdr`.
fn worker_loop(mut worker: Worker) {
    // Own clones: the loop needs `&mut worker`, so it cannot also borrow its
    // mailbox through `worker`.
    let slot = Arc::clone(&worker.slot);
    let (lock, cv) = &*slot;
    let mut last_sent = Instant::now();

    // Validated once: the env is immutable for the worker's lifetime, and `None`
    // means the identity Herdr would reject anyway (nothing to report). The
    // target itself is rebuilt per call, so it can borrow `worker` immutably
    // while the loop holds it mutably.
    if Target::new(
        &worker.env.pane_id,
        &worker.env.agent_label,
        crate::env::SOURCE_ID,
    )
    .is_none()
    {
        return;
    }

    loop {
        let (request, idle) = {
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(request) = guard.take() {
                    // Claim the in-flight flag while the lock is still held, so
                    // `flush` can never observe an empty mailbox with no call
                    // running.
                    worker.busy.store(true, Ordering::Release);
                    break (Some(request), false);
                }
                if worker.stop.load(Ordering::Acquire) {
                    break (None, false);
                }
                // Wake on the beat as well as on new work, so a state that has
                // not changed is still re-asserted.
                let remaining = worker.heartbeat.saturating_sub(last_sent.elapsed());
                let (next, _) = cv
                    .wait_timeout(guard, remaining.min(Duration::from_millis(250)))
                    .unwrap_or_else(|e| e.into_inner());
                guard = next;
                if guard.is_none()
                    && !worker.stop.load(Ordering::Acquire)
                    && last_sent.elapsed() >= worker.heartbeat
                {
                    break (None, true);
                }
            }
        };

        let request = match request {
            Some(request) => request,
            None if idle => {
                // The beat: re-send the state that is already known. Nothing is
                // claimed while the run is headless or already released, and
                // `beat_request` checks both.
                worker.busy.store(true, Ordering::Release);
                last_sent = Instant::now();
                if let Some(request) = beat_request(&worker.shared) {
                    worker.send(&request);
                }
                worker.busy.store(false, Ordering::Release);
                continue;
            }
            None => return, // stopping and nothing left to send
        };
        last_sent = Instant::now();
        worker.send(&request);
        worker.busy.store(false, Ordering::Release);
    }
}

/// The request a heartbeat repeats, or `None` when this run must stay invisible.
///
/// A release does *not* stop the beat. `/reload` dispatches a `SessionShutdown`
/// to the freshly loaded instance even though the session keeps running, so a
/// beat gated on "already released" would leave the pane permanently unclaimed
/// — the exact symptom of an idle agent missing from Herdr's list. A genuine
/// shutdown is unaffected: `begin_shutdown` stops the reporter, and `publish`
/// refuses work once `stop` is set, so no beat can follow it.
fn beat_request(shared: &Shared) -> Option<Request> {
    if !shared.interactive.load(Ordering::Acquire) {
        return None;
    }
    // A state report re-claims the pane, so the next release must be allowed to
    // fire again instead of being swallowed as a duplicate.
    shared.released.store(false, Ordering::Release);
    let state = shared
        .machine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state();
    let session_id = shared
        .session_id
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Some(Request::State {
        state,
        message: None,
        session_id,
    })
}

impl Worker {
    /// Execute one request. Split out of the loop so the beat and the event path go
    /// through exactly the same argv construction.
    fn send(&mut self, request: &Request) {
        // Immutable borrows of `self` are fine here: `Target` only lives for the
        // duration of the call.
        let Some(target) = Target::new(
            &self.env.pane_id,
            &self.env.agent_label,
            crate::env::SOURCE_ID,
        ) else {
            return;
        };
        let target = &target;
        match request {
            Request::State {
                state,
                message,
                session_id,
            } => {
                // Refresh host metadata on every transition into `Working` (i.e.
                // once per user prompt) rather than once per reporter lifetime:
                // the first report usually happens before a session id, provider
                // or model exists, so a one-shot refresh would keep a stale
                // resume command for the rest of the session. The call is a
                // blocking host round-trip, but this thread is ours, not the
                // agent's.
                if *state == AgentState::Working && !self.refreshed_for_run {
                    self.refreshed_for_run = true;
                    self.exec_context = self.context.refresh();
                } else if *state != AgentState::Working {
                    self.refreshed_for_run = false;
                }
                let session_id = session_id.clone().or_else(|| {
                    self.exec_context
                        .as_ref()
                        .and_then(|c| c.session_id.clone())
                        .or_else(|| std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).ok())
                });
                let resume_argv = if self.env.resume_enabled {
                    resume::build(session_id.as_deref(), self.exec_context.as_ref())
                } else {
                    None
                };
                let argv = protocol::report_agent(
                    target,
                    &Report {
                        state: *state,
                        message: message.as_deref(),
                        seq: self.seq.next(),
                        session_id: session_id.as_deref(),
                        resume_argv: resume_argv.as_deref(),
                    },
                );
                self.runner.run(&argv, EXEC_TIMEOUT);
            }
            Request::Release => {
                let argv = protocol::release_agent(target, self.seq.next());
                self.runner.run(&argv, EXEC_TIMEOUT);
            }
        }
    }
}

/// Timestamp-derived sequence numbers, monotonic even without a shared counter.
///
/// A plain counter starting at 1 would reset when `/reload` loads a fresh
/// cdylib, and Herdr drops any report whose `seq` is not above the last accepted
/// one. Milliseconds since the epoch survive that, and `fetch_max` also keeps
/// the value moving forward if the clock steps backwards.
#[derive(Debug, Default)]
pub struct SeqGen {
    last: AtomicU64,
}

impl SeqGen {
    /// Always `>= max(previous + 1, now)`: strictly increasing, and timestamp
    /// shaped on the very first call so a reloaded instance still outranks the
    /// reports the previous one sent.
    pub fn next(&self) -> u64 {
        let now = millis_now().max(1);
        let mut previous = self.last.load(Ordering::Acquire);
        loop {
            let candidate = previous.saturating_add(1).max(now);
            match self.last.compare_exchange_weak(
                previous,
                candidate,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return candidate,
                Err(actual) => previous = actual,
            }
        }
    }
}

#[cfg(test)]
mod test_support {
    use std::sync::mpsc::{self, Receiver, Sender};

    use super::*;

    /// A runner that records every argv instead of executing it.
    #[derive(Debug)]
    pub struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
        /// Signals each recorded call so tests can wait deterministically.
        signal: Mutex<Sender<()>>,
        receiver: Mutex<Receiver<()>>,
    }

    impl RecordingRunner {
        pub fn new() -> Arc<Self> {
            let (tx, rx) = mpsc::channel();
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                signal: Mutex::new(tx),
                receiver: Mutex::new(rx),
            })
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// Wait for the next recorded call; `false` means the timeout elapsed.
        pub fn wait_for_call(&self, timeout: Duration) -> bool {
            self.receiver
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(timeout)
                .is_ok()
        }
    }

    impl Runner for RecordingRunner {
        fn run(&self, argv: &[String], _deadline: Duration) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(argv.to_vec());
            let _ = self
                .signal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send(());
        }
    }

    /// A context source that always fails: the resume command degrades to the
    /// session id alone.
    pub struct NoContext;

    impl ContextSource for NoContext {
        fn refresh(&self) -> Option<ExecContext> {
            None
        }
    }

    /// A fixed context, so tests can assert the resume command shape.
    pub struct FixedContext(pub Option<ExecContext>);

    impl ContextSource for FixedContext {
        fn refresh(&self) -> Option<ExecContext> {
            self.0.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FixedContext, NoContext, RecordingRunner};
    use super::*;
    use crate::state::AgentState;

    fn test_env() -> HerdrEnv {
        HerdrEnv {
            pane_id: "w18:p1".to_string(),
            bin_path: PathBuf::from("/usr/bin/herdr"),
            agent_label: "rpi".to_string(),
            resume_enabled: true,
        }
    }

    fn state_of(argv: &[String]) -> Option<String> {
        argv.iter()
            .position(|a| a == "--state")
            .and_then(|i| argv.get(i + 1).cloned())
    }

    fn reporter(runner: Arc<RecordingRunner>, context: Arc<dyn ContextSource>) -> Arc<Reporter> {
        reporter_with_heartbeat(runner, context, Duration::from_secs(3600))
    }

    /// A reporter in interactive mode with a heartbeat far in the future, so
    /// call-count assertions only see transition-driven reports.
    fn reporter_with_heartbeat(
        runner: Arc<RecordingRunner>,
        context: Arc<dyn ContextSource>,
        heartbeat: Duration,
    ) -> Arc<Reporter> {
        let reporter = Reporter::start_with_heartbeat(test_env(), runner, context, heartbeat);
        reporter.mark_interactive_silently();
        reporter
    }

    #[test]
    fn a_real_transition_is_reported_once() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(state_of(&calls[0]).as_deref(), Some("working"));
        assert!(reporter.shutdown());
    }

    #[test]
    fn identical_state_is_not_re_reported() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        for _ in 0..5 {
            reporter.submit(Input::RunStarted, None);
        }
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(runner.calls().len(), 1, "only the first change counts");
        assert!(reporter.shutdown());
    }

    #[test]
    fn idle_working_idle_reports_three_monotonic_steps() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        // Wait for each report before the next transition: a pending report is
        // deliberately replaced, so three distinct calls require three turns.
        for input in [Input::RunStarted, Input::RunEnded, Input::RunStarted] {
            reporter.submit(input, None);
            assert!(runner.wait_for_call(Duration::from_secs(2)), "{input:?}");
        }
        assert!(reporter.shutdown());

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        let states: Vec<_> = calls.iter().filter_map(|a| state_of(a)).collect();
        assert_eq!(states, ["working", "idle", "working"]);

        let seqs: Vec<u64> = calls
            .iter()
            .map(|argv| {
                let i = argv.iter().position(|a| a == "--seq").expect("--seq");
                argv[i + 1].parse().expect("numeric seq")
            })
            .collect();
        assert!(
            seqs[0] < seqs[1] && seqs[1] < seqs[2],
            "{seqs:?} must increase"
        );
    }

    #[test]
    fn a_burst_collapses_to_the_newest_state() {
        // A blocked runner guarantees the whole burst is queued before the
        // worker drains anything, so the coalescing behaviour is observable.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));

        struct Gated {
            gate: Arc<(Mutex<bool>, Condvar)>,
            calls: Arc<Mutex<Vec<Vec<String>>>>,
        }
        impl Runner for Gated {
            fn run(&self, argv: &[String], _deadline: Duration) {
                let (lock, cv) = &*self.gate;
                let mut open = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*open {
                    open = cv.wait(open).unwrap_or_else(|e| e.into_inner());
                }
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(argv.to_vec());
            }
        }

        let runner = Arc::new(Gated {
            gate: Arc::clone(&gate),
            calls: Arc::clone(&calls),
        });
        let reporter = Reporter::start(test_env(), runner, Arc::new(NoContext));

        // Queue: working, idle, blocked, idle — only the last should survive.
        reporter.submit(Input::PromptStarted, None);
        reporter.submit(Input::RunEnded, None);
        reporter.submit(Input::PromptShown, None);
        reporter.submit(Input::RunEnded, None);

        // Let the worker take at most one request, then release the gate.
        std::thread::sleep(Duration::from_millis(100));
        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(150));

        let recorded = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            recorded.len() <= 2,
            "a 4-deep burst must not replay 4 calls: {recorded:?}"
        );
        if let Some(last) = recorded.last() {
            assert_eq!(state_of(last).as_deref(), Some("idle"), "newest wins");
        }
        assert!(reporter.shutdown());
    }

    #[test]
    fn a_reload_instance_never_releases_the_pane_it_did_not_claim() {
        // `/reload` loads a fresh library and dispatches `SessionShutdown` to it
        // *without* a `SessionStart`, even though the session keeps running. That
        // release is what blanked a working agent out of the sidebar: the pane is
        // owned by `source=rpi`, and a release clears the whole slot.
        let runner = RecordingRunner::new();
        let reporter = Reporter::start_with_heartbeat(
            test_env(),
            Arc::clone(&runner) as Arc<dyn Runner>,
            Arc::new(NoContext),
            Duration::from_secs(3600),
        );
        // Exactly what a reloaded instance sees: no `SessionStart`, but the
        // lifecycle hook and `SessionShutdown` still arrive.
        reporter.interactive_without_session_start();
        reporter.submit_release();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            runner.calls().is_empty(),
            "a reloaded instance must not release, got {:?}",
            runner.calls()
        );
        assert!(reporter.shutdown());
    }

    #[test]
    fn release_clears_the_agent_and_stops_the_thread() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        reporter.submit_release();
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        let calls = runner.calls();
        assert_eq!(calls[0][1], "release-agent");
        assert!(reporter.shutdown());
    }

    #[test]
    fn release_is_idempotent_and_a_later_state_reclaims_the_pane() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));

        reporter.submit_release();
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        // A second release (SessionShutdown + the lifecycle hook) must not
        // spawn `herdr` again.
        reporter.submit_release();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(runner.calls().len(), 1, "duplicate release suppressed");

        // Reporting a state again re-claims the pane, so a later release works.
        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        reporter.submit_release();
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let kinds: Vec<_> = runner.calls().iter().map(|argv| argv[1].clone()).collect();
        assert_eq!(kinds, ["release-agent", "report-agent", "release-agent"]);
    }

    #[test]
    fn the_resume_command_is_attached_once_a_session_is_known() {
        let runner = RecordingRunner::new();
        let context = FixedContext(Some(ExecContext {
            session_id: Some("ignored-when-event-provides-one".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-5".into()),
            thinking_level: Some("high".into()),
        }));
        let reporter = reporter(Arc::clone(&runner), Arc::new(context));
        reporter.submit(Input::PromptStarted, Some("01abc".into()));
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let argv = &runner.calls()[0];
        let separator = argv.iter().position(|a| a == "--").expect("separator");
        assert_eq!(argv[separator + 1], "rpi");
        assert_eq!(argv[separator + 2], "--session-id");
        assert_eq!(argv[separator + 3], "01abc");
        // The event's session id wins over the cached one.
        assert!(argv
            .iter()
            .position(|a| a == "--agent-session-id")
            .is_some());
    }

    #[test]
    fn without_a_session_id_the_state_is_still_reported() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        // Ensure no ambient session id leaks in from the test environment.
        let restore = std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).ok();
        // SAFETY: test-only mutation of the process environment.
        unsafe { std::env::remove_var(rpi_plugin_sdk::SESSION_ID_ENV) };

        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let argv = &runner.calls()[0];
        assert_eq!(state_of(argv).as_deref(), Some("working"));
        assert!(!argv.iter().any(|a| a == "--"), "no resume command");
        assert!(!argv.iter().any(|a| a == "--agent-session-id"));

        if let Some(value) = restore {
            // SAFETY: restoring the previous value in the same test process.
            unsafe { std::env::set_var(rpi_plugin_sdk::SESSION_ID_ENV, value) };
        }
    }

    #[test]
    fn the_resume_switch_suppresses_the_command() {
        let runner = RecordingRunner::new();
        let env = HerdrEnv {
            resume_enabled: false,
            ..test_env()
        };
        let reporter = Reporter::start(
            env,
            Arc::clone(&runner) as Arc<dyn Runner>,
            Arc::new(NoContext),
        );
        reporter.mark_interactive_silently();
        reporter.submit(Input::PromptStarted, Some("01abc".into()));
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let argv = &runner.calls()[0];
        assert_eq!(state_of(argv).as_deref(), Some("working"));
        assert!(!argv.iter().any(|a| a == "--"), "RPI_HERDR_RESUME=0");
        assert!(
            argv.iter().any(|a| a == "--agent-session-id"),
            "id still sent"
        );
    }

    #[test]
    fn shutdown_still_delivers_a_queued_release() {
        // `begin_shutdown` publishes the release and `finish_shutdown` joins
        // right after; dropping the queue there would leave Herdr showing a
        // ghost agent until its own fallback kicks in.
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        reporter.submit_release();
        assert!(reporter.shutdown());
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "the queued release must be sent: {calls:?}");
        assert_eq!(calls[0][1], "release-agent");
    }

    #[test]
    fn shutdown_without_pending_work_is_prompt() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        let started = Instant::now();
        assert!(reporter.shutdown());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn submission_after_shutdown_is_ignored() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        assert!(reporter.shutdown());
        reporter.submit(Input::RunStarted, None);
        reporter.submit_release();
        std::thread::sleep(Duration::from_millis(100));
        assert!(runner.calls().is_empty(), "a stopped reporter is inert");
    }

    #[test]
    fn seq_is_monotonic_and_survives_a_clock_that_does_not_move() {
        let seq = SeqGen::default();
        let first = seq.next();
        let second = seq.next();
        assert!(second > first, "{second} must exceed {first}");
        // A timestamp-derived seq is far above a small counter, which is what
        // keeps it increasing across `/reload`.
        assert!(
            first > 1_600_000_000,
            "seq must look like a timestamp: {first}"
        );
    }

    #[test]
    fn a_dialog_returns_the_pane_to_working() {
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        reporter.submit(Input::PromptStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        reporter.submit(Input::PromptShown, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        reporter.submit(Input::PromptDismissed, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let states: Vec<_> = runner.calls().iter().filter_map(|a| state_of(a)).collect();
        assert_eq!(states, ["working", "blocked", "working"]);
        assert_eq!(AgentState::Blocked.as_str(), "blocked", "wire label sanity");
    }

    #[test]
    fn a_dialog_outside_a_run_returns_the_pane_to_idle() {
        // `UiPromptStart` can fire for an extension/startup prompt, i.e. with no
        // run in flight. Reporting `working` after the dismissal would strand the
        // pane until the next prompt.
        let runner = RecordingRunner::new();
        let reporter = reporter(Arc::clone(&runner), Arc::new(NoContext));
        reporter.submit(Input::PromptShown, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        reporter.submit(Input::PromptDismissed, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());

        let states: Vec<_> = runner.calls().iter().filter_map(|a| state_of(a)).collect();
        assert_eq!(states, ["blocked", "idle"]);
    }

    #[test]
    fn host_metadata_is_refreshed_again_on_the_next_run() {
        // A one-shot refresh would keep the first (usually empty) context for the
        // rest of the session, so the resume command never learns the model.
        #[derive(Default)]
        struct Counting {
            calls: std::sync::atomic::AtomicUsize,
        }
        impl ContextSource for Counting {
            fn refresh(&self) -> Option<ExecContext> {
                let n = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
                Some(ExecContext {
                    session_id: Some(format!("session-{n}")),
                    ..ExecContext::default()
                })
            }
        }

        let runner = RecordingRunner::new();
        let context = Arc::new(Counting::default());
        let reporter = reporter(Arc::clone(&runner), context.clone());
        for input in [Input::RunStarted, Input::RunEnded, Input::RunStarted] {
            reporter.submit(input, None);
            assert!(runner.wait_for_call(Duration::from_secs(2)), "{input:?}");
        }
        assert!(reporter.shutdown());

        assert_eq!(
            context.calls.load(Ordering::Acquire),
            2,
            "one refresh per run, not one per reporter"
        );
        // The idle report in between reuses the cached id rather than refreshing;
        // the third call proves the second run picked up fresh metadata.
        let rebuilt: Vec<_> = runner
            .calls()
            .iter()
            .filter_map(|argv| {
                let i = argv.iter().position(|a| a == "--agent-session-id")?;
                argv.get(i + 1).cloned()
            })
            .collect();
        assert_eq!(rebuilt, ["session-1", "session-1", "session-2"]);
    }

    #[test]
    fn the_timeouts_respect_their_budgets() {
        // A flush sits inside an event handler, so it must stay well below the
        // 15 s `SessionShutdown` budget, and one bounded exec must fit inside it.
        assert!(EXEC_TIMEOUT <= SHUTDOWN_FLUSH_TIMEOUT);
        assert!(SHUTDOWN_TIMEOUT >= SHUTDOWN_FLUSH_TIMEOUT);
    }

    #[test]
    fn a_headless_run_never_claims_the_pane() {
        // The bug this guards: `rpi -p` inherits `HERDR_*` from the TUI that
        // started it and used to claim the same (pane, source, agent) slot, then
        // release it on exit — wiping the parent out of Herdr's agent list
        // while it was still working.
        let runner = RecordingRunner::new();
        let reporter = Reporter::start(
            test_env(),
            Arc::clone(&runner) as Arc<dyn Runner>,
            Arc::new(NoContext),
        );
        for input in [
            Input::PromptStarted,
            Input::RunStarted,
            Input::RunEnded,
            Input::PromptShown,
            Input::PromptDismissed,
        ] {
            reporter.submit(input, Some("01abc".into()));
        }
        reporter.submit_release();
        reporter.flush(Duration::from_millis(200));
        assert!(
            runner.calls().is_empty(),
            "a headless run must be invisible to Herdr, got {:?}",
            runner.calls()
        );
        assert!(reporter.shutdown());
    }

    #[test]
    fn entering_interactive_claims_the_pane_before_any_prompt() {
        // Between TUI startup and the first keystroke there is no transition to
        // report, so the claim has to happen on entry — otherwise the pane sits
        // in Herdr with no agent at all until the user types something.
        let runner = RecordingRunner::new();
        let reporter = Reporter::start(
            test_env(),
            Arc::clone(&runner) as Arc<dyn Runner>,
            Arc::new(NoContext),
        );
        reporter.enter_interactive();
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        let calls = runner.calls();
        assert_eq!(state_of(&calls[0]).as_deref(), Some("idle"));
        assert!(reporter.shutdown());
    }

    #[test]
    fn the_heartbeat_re_asserts_an_unchanged_state() {
        // Herdr can drop the pane's authority on its own (settling process
        // detection, a config reload, `/reload` swapping this library). Without a
        // repeat the pane would stay invisible until the next real transition.
        let runner = RecordingRunner::new();
        let reporter = reporter_with_heartbeat(
            Arc::clone(&runner),
            Arc::new(NoContext),
            Duration::from_millis(80),
        );
        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)), "first report");
        assert!(
            runner.wait_for_call(Duration::from_secs(2)),
            "the beat must repeat the same state"
        );
        let calls = runner.calls();
        assert_eq!(state_of(&calls[0]).as_deref(), Some("working"));
        assert_eq!(state_of(&calls[1]).as_deref(), Some("working"));
        // Sequence numbers still move forward, or Herdr drops the repeat.
        assert!(calls[1].iter().any(|a| a == "--seq"));
        assert_ne!(calls[0], calls[1]);
        assert!(reporter.shutdown());
    }

    #[test]
    fn the_heartbeat_reclaims_the_pane_after_a_release() {
        // `/reload` sends `SessionShutdown` to the freshly loaded instance while
        // the session keeps running. If the beat honoured that release the pane
        // would stay unclaimed forever and an idle agent would vanish from
        // Herdr's list — the bug this whole change is about.
        let runner = RecordingRunner::new();
        let reporter = reporter_with_heartbeat(
            Arc::clone(&runner),
            Arc::new(NoContext),
            Duration::from_millis(60),
        );
        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)), "claim");
        reporter.submit_release();
        assert!(runner.wait_for_call(Duration::from_secs(2)), "release");
        // The very next beat must claim the pane again.
        assert!(
            runner.wait_for_call(Duration::from_secs(2)),
            "the beat must re-claim after a release, got {:?}",
            runner.calls()
        );
        let calls = runner.calls();
        let last = calls.last().expect("a beat");
        assert!(
            last.iter().any(|a| a == "--state"),
            "the beat re-claims with a state report, got {last:?}"
        );
        assert!(reporter.shutdown());
    }

    #[test]
    fn a_stopped_reporter_never_beats() {
        // The real end of a session: `begin_shutdown` stops the reporter, so no
        // beat may resurrect the claim after the host has moved on.
        let runner = RecordingRunner::new();
        let reporter = reporter_with_heartbeat(
            Arc::clone(&runner),
            Arc::new(NoContext),
            Duration::from_millis(50),
        );
        reporter.submit(Input::RunStarted, None);
        assert!(runner.wait_for_call(Duration::from_secs(2)));
        assert!(reporter.shutdown());
        let after_stop = runner.calls().len();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            runner.calls().len(),
            after_stop,
            "a stopped reporter must stay silent, got {:?}",
            runner.calls()
        );
    }
}
