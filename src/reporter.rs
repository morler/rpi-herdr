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
use crate::state::{AgentState, Input};

/// How long one `herdr` call may take before it is killed and forgotten.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound for `finish_shutdown`. Two seconds of exec plus scheduling slack.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound for pushing the final release out while still inside an event
/// handler. Comfortably below the 15 s `SessionShutdown` budget.
pub const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

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
                Err(_) => return,
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

/// Owning handle for the reporting thread. Dropping it signals shutdown.
pub struct Reporter {
    slot: Arc<(Mutex<Option<Request>>, Condvar)>,
    stop: Arc<AtomicBool>,
    /// True while the worker is inside `runner.run(..)` for one request.
    busy: Arc<AtomicBool>,
    /// Whether the host dispatched `SessionStart`, i.e. this is a long-lived
    /// session (TUI/REPL/rpc) rather than a one-shot run.
    session_started: AtomicBool,
    /// Set once a release was published, so `SessionShutdown` followed by the
    /// lifecycle hook does not spawn a second `herdr` for the same outcome.
    /// Cleared as soon as a new state is reported.
    released: AtomicBool,
    last_state: Mutex<Option<AgentState>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Reporter {
    /// Start the reporting thread. `env` supplies the pane identity, `runner`
    /// performs the calls and `context` provides resume metadata.
    pub fn start(
        env: HerdrEnv,
        runner: Arc<dyn Runner>,
        context: Arc<dyn ContextSource>,
    ) -> Arc<Self> {
        let slot = Arc::new((Mutex::new(None::<Request>), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let seq = Arc::new(SeqGen::default());

        let thread = {
            let slot = Arc::clone(&slot);
            let stop = Arc::clone(&stop);
            let busy = Arc::clone(&busy);
            let seq = Arc::clone(&seq);
            std::thread::Builder::new()
                .name("rpi-herdr".to_string())
                // A failure to spawn a thread must not take the agent down; the
                // plugin then simply reports nothing.
                .spawn(move || worker_loop(env, runner, context, slot, stop, busy, seq))
                .ok()
        };

        Arc::new(Self {
            slot,
            stop,
            busy,
            session_started: AtomicBool::new(false),
            released: AtomicBool::new(false),
            last_state: Mutex::new(None),
            thread: Mutex::new(thread),
        })
    }

    /// Feed one input to the state machine and publish the resulting state.
    ///
    /// Never blocks on I/O: the handler path stays in the microsecond range so
    /// event-log budgets (10 s for `BeforeAgentStart`) are never approached.
    pub fn submit(&self, input: Input, session_id: Option<String>) {
        if input == Input::SessionReady {
            self.session_started.store(true, Ordering::Release);
        }
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        let (state, changed) = {
            let mut last = self.last_state.lock().unwrap_or_else(|e| e.into_inner());
            let current = last.unwrap_or(AgentState::Idle);
            let next = current.next(input);
            let changed = *last != Some(next);
            *last = Some(next);
            (next, changed)
        };
        // Report only real transitions, so a chatty event stream cannot make the
        // pane flicker between identical states.
        if !changed {
            return;
        }
        self.released.store(false, Ordering::Release);
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
            let mut last = self.last_state.lock().unwrap_or_else(|e| e.into_inner());
            *last = None;
        }
        // Idempotent: Herdr accepts repeated releases, but a second spawn buys
        // nothing. A later state report clears the flag and re-claims the pane.
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.publish(Request::Release);
    }

    /// Overwrite whatever is queued. A late older request can never clobber a
    /// newer one because the mailbox holds exactly one entry.
    fn publish(&self, request: Request) {
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

    /// Whether the host ever announced a session. `false` after a run means a
    /// one-shot mode (`-p`, `--mode json`) that dispatches no `SessionStart` and
    /// no `SessionShutdown` — the agent is about to exit, so the report must be
    /// released proactively.
    pub fn is_one_shot_run(&self) -> bool {
        !self.session_started.load(Ordering::Acquire)
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
                .map(|guard| guard.is_none())
                .unwrap_or(false);
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

        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(handle) = handle else {
            return true;
        };
        let until = Instant::now() + SHUTDOWN_TIMEOUT;
        while !handle.is_finished() {
            if Instant::now() >= until {
                // Still running: the host must keep the library loaded rather
                // than unload code that is about to execute.
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.join().is_ok()
    }
}

/// Milliseconds since the Unix epoch, clamped to `0` before it.
pub fn millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The reporting loop. The only place that spawns `herdr`.
fn worker_loop(
    env: HerdrEnv,
    runner: Arc<dyn Runner>,
    context: Arc<dyn ContextSource>,
    slot: Arc<(Mutex<Option<Request>>, Condvar)>,
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    seq: Arc<SeqGen>,
) {
    let (lock, cv) = &*slot;
    let mut exec_context: Option<ExecContext> = None;
    let mut need_refresh = true;

    loop {
        let request = {
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(request) = guard.take() {
                    break Some(request);
                }
                if stop.load(Ordering::Acquire) {
                    break None;
                }
                let (next, _) = cv
                    .wait_timeout(guard, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner());
                guard = next;
            }
        };

        let Some(request) = request else {
            return; // stopping and nothing left to send
        };
        busy.store(true, Ordering::Release);

        match request {
            Request::State {
                state,
                message,
                session_id,
            } => {
                // Refresh host metadata once per user prompt rather than per
                // event: the call is a blocking host round-trip, so keep it off
                // the hot path and off the agent's thread (this is ours).
                if need_refresh {
                    need_refresh = false;
                    exec_context = context.refresh();
                }
                let session_id = session_id.or_else(|| {
                    exec_context
                        .as_ref()
                        .and_then(|c| c.session_id.clone())
                        .or_else(|| std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).ok())
                });
                let resume_argv = if env.resume_enabled {
                    resume::build(session_id.as_deref(), exec_context.as_ref())
                } else {
                    None
                };
                let argv = protocol::report_agent(
                    &Target {
                        pane_id: &env.pane_id,
                        agent_label: &env.agent_label,
                        source: crate::env::SOURCE_ID,
                    },
                    &Report {
                        state,
                        message: message.as_deref(),
                        seq: seq.next(),
                        session_id: session_id.as_deref(),
                        resume_argv: resume_argv.as_deref(),
                    },
                );
                runner.run(&argv, EXEC_TIMEOUT);
            }
            Request::Release => {
                let argv = protocol::release_agent(
                    &Target {
                        pane_id: &env.pane_id,
                        agent_label: &env.agent_label,
                        source: crate::env::SOURCE_ID,
                    },
                    seq.next(),
                );
                runner.run(&argv, EXEC_TIMEOUT);
            }
        }
        busy.store(false, Ordering::Release);
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
        Reporter::start(test_env(), runner, context)
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
}
