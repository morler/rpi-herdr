//! # rpi-herdr
//!
//! Reports rpi's agent state, session resume command and pane release to
//! [Herdr](https://herdr.dev) so a pane shows `rpi` with a live
//! `idle`/`working`/`blocked` state, notifications fire on completion or block,
//! and a Herdr server restart can reopen the same rpi session.
//!
//! Everything is conditional on Herdr's own environment contract
//! (`HERDR_ENV=1` plus `HERDR_PANE_ID`/`HERDR_BIN_PATH`). Outside Herdr this
//! plugin registers **no** handler and returns success, so it has no cost and
//! produces no noise.
//!
//! ## Event → state mapping
//!
//! | Event | Input | Result |
//! | --- | --- | --- |
//! | `SessionStart` | `SessionReady` | unchanged |
//! | `BeforeAgentStart` | `PromptStarted` | `working` |
//! | `AgentStart` | `RunStarted` | `working` |
//! | `AgentEnd` | `RunEnded` | `idle` |
//! | `UiPromptStart` | `PromptShown` | `blocked` |
//! | `UiPromptEnd` | `PromptDismissed` | `working` |
//! | `SessionShutdown` | — | release |
//!
//! Handlers never spawn a process and never touch the network: they overwrite a
//! one-slot mailbox and return, and a single background thread performs the
//! `herdr` call with a short deadline. The one exception is the drain before a
//! final `idle`+release on the one-shot path, which is bounded and happens once
//! per process.
//!
//! ## Configuration
//!
//! - `RPI_HERDR=0` — stay completely silent inside Herdr.
//! - `RPI_HERDR_RESUME=0` — report state but never send a resume command.
//! - `RPI_HERDR_AGENT=<label>` — override the agent label (default `rpi`).

// `export_plugin!` generates `pub extern "C" fn rpi_plugin_register(api: *const
// PluginApi)`; the macro owns the safety contract (null check, version check,
// then a guarded deref). The lint cannot see through the expansion.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod env;
mod protocol;
mod reporter;
mod resume;
mod state;

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use rpi_plugin_sdk::{
    EventTag, PluginApi, RuntimeActionId, StbString, StbStringRef, EVENT_HANDLER_CONTINUE,
};

use crate::reporter::{
    ContextSource, ProcessRunner, Reporter, REPORT_FLUSH_TIMEOUT, SHUTDOWN_FLUSH_TIMEOUT,
};
use crate::resume::ExecContext;
use crate::state::Input;

/// The live reporter for this loaded library instance. A `/reload` builds a new
/// cdylib with its own statics, so this never aliases a previous instance.
static STATE: Mutex<Option<Arc<Reporter>>> = Mutex::new(None);

fn current() -> Option<Arc<Reporter>> {
    STATE.lock().ok().and_then(|guard| guard.clone())
}

// ---------------------------------------------------------------------------
// Host bridge for `GetExecutionContext`
// ---------------------------------------------------------------------------

/// Records whatever the host hands back, so the worker can build a resume
/// command that reproduces the current model, provider and thinking level.
struct HostContext {
    runtime_action: rpi_plugin_sdk::RuntimeActionFn,
    /// Opaque action-bridge handle from the vtable. Valid only for the lifetime
    /// of this loaded library; never cached across registrations, never logged,
    /// never exposed to the model.
    user_data: *mut c_void,
    /// The host's deallocator for strings it produced for us.
    free_string: rpi_plugin_sdk::FreeStringFn,
}

// SAFETY: both the vtable's action fn and `user_data` are process-wide handles
// the host documents as usable from any plugin thread (worker threads are the
// intended callers). The host owns their lifetime and keeps them valid for as
// long as it may call plugin code.
unsafe impl Send for HostContext {}
unsafe impl Sync for HostContext {}

impl ContextSource for HostContext {
    fn refresh(&self) -> Option<ExecContext> {
        let args = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = (self.runtime_action)(
            RuntimeActionId::GetExecutionContext as u32,
            args,
            &mut out as *mut StbString,
            self.user_data,
        );
        // SAFETY: `out` is either untouched (empty → no-op) or a live
        // allocation produced by the host, which is exactly what the host's
        // `free_string` reclaims. The text is copied into an owned `String`
        // *before* the allocation is released, so nothing below can read freed
        // host memory regardless of how `to_string_lossy` is implemented. Freed
        // exactly once, and `free_with` itself no-ops on an empty string.
        let text: String = unsafe { out.to_string_lossy() };
        unsafe { out.free_with(Some(self.free_string)) };

        if rc != 0 || text.is_empty() {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let context = ExecContext::from_json(&value);
        // Only worth caching when it can actually change the resume command.
        (context.session_id.is_some()
            || context.model.is_some()
            || context.provider.is_some()
            || context.thinking_level.is_some())
        .then_some(context)
    }
}

/// Fallback when the host has no action bridge: state is still reported, but the
/// resume command degrades to `rpi --session-id <id>`.
struct NoHost;

impl ContextSource for NoHost {
    fn refresh(&self) -> Option<ExecContext> {
        None
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

rpi_plugin_sdk::event_handler! {
    fn on_event(event, _data) {
        let Some(reporter) = current() else {
            return EVENT_HANDLER_CONTINUE;
        };
        match event.tag() {
            EventTag::SessionStart => reporter.submit(Input::SessionReady, None),
            EventTag::BeforeAgentStart => {
                // `{"prompt","imageCount","systemPrompt","sessionId","host"}`.
                // Only `sessionId` matters here. Deserialise into a two-field
                // struct instead of a full `serde_json::Value`: the payload
                // carries the prompt and the whole system prompt (hundreds of
                // KB), and every turn would otherwise allocate all of it just
                // to read one id. Unknown fields are skipped without allocation.
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct BeforeAgentStart {
                    #[serde(default)]
                    session_id: Option<String>,
                }
                let session_id = event
                    .data_text()
                    .and_then(|text| serde_json::from_str::<BeforeAgentStart>(&text).ok())
                    .and_then(|payload| payload.session_id)
                    .filter(|id| !id.trim().is_empty());
                reporter.submit(Input::PromptStarted, session_id);
            }
            EventTag::AgentStart => reporter.submit(Input::RunStarted, None),
            EventTag::AgentEnd => {
                reporter.submit(Input::RunEnded, None);
                // `-p` and `--mode json` dispatch neither `SessionStart` nor
                // `SessionShutdown`, and the host's own cleanup thread does not
                // outlive the process. Release here so the pane is not left
                // showing a dead agent (Herdr's own fallback is also slower).
                if reporter.is_one_shot_run() {
                    // Let the `idle` report land first: publishing the release
                    // into the same slot would overwrite it, and the final
                    // state is what a user watching the sidebar expects to see.
                    // The intermediate drain is short by design; the long budget
                    // is reserved for the release below.
                    reporter.flush(REPORT_FLUSH_TIMEOUT);
                    reporter.submit_release();
                    reporter.flush(SHUTDOWN_FLUSH_TIMEOUT);
                }
            }
            // The host dispatches these two as *empty* events, so the payload
            // accessors must not be touched here (`EventView::data_text` lists
            // them for historical reasons and would read an inactive union arm).
            EventTag::UiPromptStart => reporter.submit(Input::PromptShown, None),
            EventTag::UiPromptEnd => reporter.submit(Input::PromptDismissed, None),
            EventTag::SessionShutdown => {
                // Some modes dispatch this as the last thing they do and then
                // let the process exit (headless rpc, `-p`). Wait for the
                // release to actually reach Herdr: bounded well inside this
                // tag's 15 s budget, and harmless when the reporter is idle.
                reporter.submit_release();
                reporter.flush(SHUTDOWN_FLUSH_TIMEOUT);
            }
            _ => {}
        }
        EVENT_HANDLER_CONTINUE
    }
}

// ---------------------------------------------------------------------------
// Shutdown hooks
// ---------------------------------------------------------------------------

extern "C" fn begin_shutdown(_user_data: *mut c_void) {
    rpi_plugin_sdk::guard(|| {
        if let Some(reporter) = current() {
            // A real exit: tell Herdr to forget this agent now, and make sure
            // the request actually leaves before the host can unload us.
            reporter.submit_release();
            reporter.flush(SHUTDOWN_FLUSH_TIMEOUT);
        }
    })
    .ok();
}

extern "C" fn finish_shutdown(_user_data: *mut c_void) -> i32 {
    rpi_plugin_sdk::guard_or(-1, || {
        let Some(reporter) = current() else {
            return 0;
        };
        if reporter.shutdown() {
            // Safe to unload: no thread can run plugin code again.
            if let Ok(mut guard) = STATE.lock() {
                *guard = None;
            }
            0
        } else {
            // The thread is still alive; a nonzero return makes the host keep
            // this library loaded instead of unmapping executing code.
            -1
        }
    })
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Event tags this plugin observes. Deliberately small: an integration that
/// needs no model interaction should not add per-turn FFI work.
const TAGS: [EventTag; 7] = [
    EventTag::SessionStart,
    EventTag::SessionShutdown,
    EventTag::BeforeAgentStart,
    EventTag::AgentStart,
    EventTag::AgentEnd,
    EventTag::UiPromptStart,
    EventTag::UiPromptEnd,
];

rpi_plugin_sdk::export_plugin!(|api: &PluginApi| {
    // Outside Herdr (or with `RPI_HERDR=0`) there is nothing to do. Registering
    // zero handlers and returning 0 is the documented "loaded, idle" outcome;
    // a nonzero return would be reported as a refused plugin.
    let Some(env) = env::detect() else {
        return 0;
    };

    if let Some(declare) = api.declare {
        let rc = declare(StbStringRef::from_str(
            r#"{"priority":60,"platforms":["linux","macos","windows"]}"#,
        ));
        if rc != 0 {
            return rc;
        }
    }

    let Some(register_event_handler) = api.register_event_handler else {
        return 1; // host too old for the ABI surface this plugin needs
    };
    // The reporter owns a background thread that executes plugin code, so a
    // lifecycle hook is not optional: without `finish_shutdown` nothing ever
    // joins that thread and the host may unmap it mid-run. Registering it before
    // the thread exists means a failure here cannot strand a live worker.
    let Some(register_lifecycle) = api.register_plugin_lifecycle else {
        return 1;
    };
    let rc = register_lifecycle(begin_shutdown, finish_shutdown, std::ptr::null_mut());
    if rc != 0 {
        return rc;
    }

    let context: Arc<dyn ContextSource> = if api.user_data.is_null() {
        Arc::new(NoHost)
    } else {
        Arc::new(HostContext {
            runtime_action: api.runtime_action,
            user_data: api.user_data,
            free_string: api.free_string,
        })
    };

    let runner = Arc::new(ProcessRunner::new(env.bin_path.clone()));
    let reporter = Reporter::start(env, runner, context);

    // Publish the reporter *before* the handlers exist. The lifecycle hook is
    // already registered, so every later failure path leaves the host with a
    // way to join this thread: `finish_shutdown` reads `STATE` and returns -1
    // when it cannot, which is what keeps a half-registered plugin from being
    // unmapped while its worker runs.
    match STATE.lock() {
        Ok(mut guard) => *guard = Some(Arc::clone(&reporter)),
        Err(_) => {
            // Poisoned before we could publish: nobody else can reach the
            // reporter, so shutting it down here is the only option.
            if !reporter.shutdown() {
                eprintln!("rpi-herdr: worker still running after a poisoned state lock");
            }
            return 1;
        }
    }

    for tag in TAGS {
        // Abandon a partially registered plugin: reporting while some
        // transitions are missing would show Herdr a state that never happened.
        // The reporter stays in `STATE` so the lifecycle hook can still join it.
        let rc = register_event_handler(tag, on_event, std::ptr::null_mut());
        if rc != 0 {
            return rc;
        }
    }
    0
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// Serialises every test that reads or writes the process-global [`STATE`].
    ///
    /// The default harness runs tests in parallel, so a test that installs a
    /// reporter would otherwise overlap `handlers_are_no_ops_before_registration`
    /// and both would be flaky. Poisoning is ignored: a panicking test leaves no
    /// invariant behind that the others depend on beyond the lock itself.
    static STATE_GUARD: Mutex<()> = Mutex::new(());

    fn state_guard() -> MutexGuard<'static, ()> {
        STATE_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_observed_tags_are_the_ones_the_state_machine_needs() {
        // Guards against an accidental edit that would silently drop a
        // transition (for example losing `AgentEnd` and never returning to idle).
        assert_eq!(TAGS.len(), 7);
        for required in [
            EventTag::SessionStart,
            EventTag::SessionShutdown,
            EventTag::BeforeAgentStart,
            EventTag::AgentStart,
            EventTag::AgentEnd,
            EventTag::UiPromptStart,
            EventTag::UiPromptEnd,
        ] {
            assert!(TAGS.contains(&required), "{required:?} must be observed");
        }
    }

    #[test]
    fn the_agent_label_never_shadows_a_bundled_herdr_integration() {
        assert_eq!(env::DEFAULT_AGENT_LABEL, "rpi");
        assert!(!env::SOURCE_ID.starts_with("herdr:"));
    }

    #[test]
    fn finish_shutdown_reports_failure_without_a_reporter() {
        let _guard = state_guard();
        // No reporter registered: there is no thread to join, so unloading is
        // safe and the hook must not claim failure.
        assert_eq!(finish_shutdown(std::ptr::null_mut()), 0);
    }

    #[test]
    fn begin_and_finish_shutdown_are_safe_without_state() {
        let _guard = state_guard();
        begin_shutdown(std::ptr::null_mut());
        assert_eq!(finish_shutdown(std::ptr::null_mut()), 0);
    }

    #[test]
    fn a_freshly_registered_reporter_is_reachable_from_handlers() {
        let _guard = state_guard();
        struct Noop;
        impl crate::reporter::Runner for Noop {
            fn run(&self, _argv: &[String], _deadline: std::time::Duration) {}
        }
        impl crate::reporter::ContextSource for Noop {
            fn refresh(&self) -> Option<ExecContext> {
                None
            }
        }

        let env = env::HerdrEnv {
            pane_id: "w18:p1".into(),
            bin_path: std::path::PathBuf::from("/usr/bin/herdr"),
            agent_label: "rpi".into(),
            resume_enabled: true,
        };
        let reporter = Reporter::start(env, Arc::new(Noop), Arc::new(Noop));
        {
            let mut guard = STATE.lock().unwrap();
            *guard = Some(Arc::clone(&reporter));
        }
        assert!(current().is_some());
        // Handlers must tolerate the event payload being absent.
        assert!(
            on_event(
                rpi_plugin_sdk::StablePluginEvent::empty(EventTag::AgentStart),
                std::ptr::null_mut()
            ) == EVENT_HANDLER_CONTINUE
        );
        assert!(reporter.shutdown());
        let mut guard = STATE.lock().unwrap();
        *guard = None;
    }

    #[test]
    fn handlers_are_no_ops_before_registration() {
        let _guard = state_guard();
        let event = rpi_plugin_sdk::StablePluginEvent::empty(EventTag::AgentStart);
        assert_eq!(
            on_event(event, std::ptr::null_mut()),
            EVENT_HANDLER_CONTINUE
        );
    }

    #[test]
    fn finish_shutdown_keeps_reporting_failure_after_a_failed_join() {
        let _guard = state_guard();
        // A worker that ignores `stop` (wedged inside `runner.run`) must never
        // be reported as "gone": the host would unmap code that is still
        // executing. The verdict is sticky, so a retry cannot flip it either.
        struct Wedged;
        impl crate::reporter::Runner for Wedged {
            fn run(&self, _argv: &[String], _deadline: std::time::Duration) {
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
        impl crate::reporter::ContextSource for Wedged {
            fn refresh(&self) -> Option<ExecContext> {
                None
            }
        }

        let env = env::HerdrEnv {
            pane_id: "w18:p1".into(),
            bin_path: std::path::PathBuf::from("/usr/bin/herdr"),
            agent_label: "rpi".into(),
            resume_enabled: true,
        };
        let reporter = Reporter::start(env, Arc::new(Wedged), Arc::new(Wedged));
        reporter.submit(Input::RunStarted, None);
        // Give the worker time to pick the request up and enter `run`.
        std::thread::sleep(std::time::Duration::from_millis(150));

        assert!(!reporter.shutdown(), "a live worker is not 'unloaded'");
        assert!(
            !reporter.shutdown(),
            "a failed join must stay failed on retry"
        );
    }
}
