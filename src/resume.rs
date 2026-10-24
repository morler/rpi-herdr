//! Construction and validation of the resume command handed to Herdr.
//!
//! Herdr reopens a pane after a server restart by running the argv they stored
//! from `pane report-agent … -- <ARGV>…`. The rules below come from the
//! protocol and from local probing against Herdr 0.9.5; violating **any** of
//! them makes Herdr reject the whole report (`invalid_resume_argv`), so the
//! caller drops the argv and still reports the state.
//!
//! 1. `argv[0]` is a bare command name on `PATH` — no path separators.
//! 2. No argument may contain a single quote or a control character.
//! 3. At most [`MAX_ARGS`] arguments.
//! 4. At most [`MAX_ARGV_BYTES`] bytes in total (UTF-8, separators included).
//! 5. A session id is mandatory: without one there is nothing to resume.
//!
//! Optional flags only improve fidelity after a restart — the correctness of a
//! resume depends on the session id alone, so anything questionable is dropped
//! instead of failing the whole command.

/// Command name Herdr must be able to find on the user's `PATH`.
pub const COMMAND: &str = "rpi";

/// Herdr's documented argument-count ceiling.
pub const MAX_ARGS: usize = 64;

/// Herdr's documented total-argv length ceiling.
pub const MAX_ARGV_BYTES: usize = 8 * 1024;

/// The id rpi uses for a session that was never persisted (`--no-session`).
/// Resuming it would open a brand-new session, so it is not a session id.
const EPHEMERAL_SESSION_ID: &str = "ephemeral";

/// Safe live execution metadata, as returned by `GetExecutionContext`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecContext {
    pub session_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
}

impl ExecContext {
    /// Parse `GetExecutionContext`'s result. Unknown/missing fields stay `None`
    /// and a real error reply (`{"error": <non-null>}`) yields an empty
    /// context.
    pub fn from_json(value: &serde_json::Value) -> Self {
        if value.get("error").is_some_and(|error| !error.is_null()) {
            return Self::default();
        }
        let model = value.get("model");
        let mut context = Self {
            session_id: string_field(value, "sessionId"),
            provider: model
                .and_then(|m| string_field(m, "provider"))
                .or_else(|| string_field(value, "provider")),
            model: model.and_then(|m| string_field(m, "id")).or_else(|| {
                model
                    .and_then(|m| string_field(m, "model"))
                    .or_else(|| string_field(value, "model"))
            }),
            thinking_level: string_field(value, "thinkingLevel")
                .or_else(|| string_field(value, "thinking")),
        };
        context.dedupe_model_qualified_provider();
        context
    }

    /// rpi accepts `--model provider:id`; some hosts report the provider inside
    /// the model id. Keeping both would duplicate it, so drop the separate flag.
    fn dedupe_model_qualified_provider(&mut self) {
        if let (Some(provider), Some(model)) = (&self.provider, &self.model) {
            if model
                .strip_prefix(provider.as_str())
                .is_some_and(|rest| rest.starts_with(':'))
            {
                self.provider = None;
            }
        }
    }
}

/// A session id we can actually reopen: non-blank, not the ephemeral
/// placeholder, and safe to hand to Herdr as one argv element. Anything else
/// means "this candidate cannot be used", which lets the caller fall back to
/// the next one instead of dropping the resume entirely.
fn usable_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && !id.eq_ignore_ascii_case(EPHEMERAL_SESSION_ID) && is_safe_arg(id))
        .then_some(id)
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Build the resume argv: `rpi --session-id <id> [--provider p] [--model m]
/// [--thinking l]`.
///
/// The id candidates are tried in order (the event's id, then the host's), so an
/// unsafe explicit id degrades to a usable fallback instead of losing the
/// command.
///
/// Returns `None` when no candidate is usable, or when the resulting command
/// cannot satisfy Herdr's constraints.
pub fn build(session_id: Option<&str>, exec: Option<&ExecContext>) -> Option<Vec<String>> {
    let session_id = session_id
        .and_then(usable_id)
        .or_else(|| {
            exec.and_then(|c| c.session_id.as_deref())
                .and_then(usable_id)
        })?
        .to_string();

    let mut argv = vec![
        COMMAND.to_string(),
        "--session-id".to_string(),
        session_id.clone(),
    ];

    if let Some(exec) = exec {
        // Optional flags are best-effort: a value that is blank or violates
        // Herdr's argument rules is skipped individually. If the survivors still
        // break a limit, the fallback below drops all of them at once.
        for (flag, value) in [
            ("--provider", exec.provider.as_deref()),
            ("--model", exec.model.as_deref()),
            ("--thinking", exec.thinking_level.as_deref()),
        ] {
            if let Some(value) = value
                .map(str::trim)
                .filter(|v| !v.is_empty() && is_safe_arg(v))
            {
                argv.push(flag.to_string());
                argv.push(value.to_string());
            }
        }
    }

    if validate(&argv) {
        Some(argv)
    } else {
        // The extras broke a constraint; retry with the session id only.
        let minimal = vec![COMMAND.to_string(), "--session-id".to_string(), session_id];
        validate(&minimal).then_some(minimal)
    }
}

/// Check Herdr's generic argv-shape constraints: a bare program name, no single
/// quote or control character in any element, at most [`MAX_ARGS`] arguments and
/// at most [`MAX_ARGV_BYTES`] bytes in total.
///
/// Rule 5 ("a session id is mandatory") is **not** enforced here — it is a
/// property of a *resume command*, not of an argv shape, and it lives in
/// [`build`]. Exposed so tests and the reporter agree on one definition of
/// "Herdr will accept this argv".
pub fn validate(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    // A bare command name: no path separator, and no whitespace either — an
    // element like `"rpi --session-id abc"` is Herdr's documented
    // `invalid_resume_argv` failure (a whole shell line passed as one argument).
    if program.is_empty()
        || program.contains(['/', '\\'])
        || program.chars().any(char::is_whitespace)
    {
        return false;
    }
    if argv.len() > MAX_ARGS {
        return false;
    }
    if argv.iter().any(|arg| !is_safe_arg(arg)) {
        return false;
    }
    total_len(argv) <= MAX_ARGV_BYTES
}

/// No single quotes and no control characters (newlines, NULs, tabs) in any
/// argument.
///
/// Herdr stores the argv as an array and re-executes it without a shell, so the
/// other shell metacharacters (`"`, `` ` ``, `$`, `;`, `\`) are *data* and need
/// no filtering. The single quote is the one exception Herdr itself rejects,
/// which is why it is banned on this side too.
#[cfg_attr(test, allow(dead_code))]
fn is_safe_arg(arg: &str) -> bool {
    !arg.contains('\'') && !arg.chars().any(char::is_control)
}

/// UTF-8 byte count plus one separator byte per argument, which stays below
/// Herdr's limit rather than straddling it.
fn total_len(argv: &[String]) -> usize {
    argv.iter().map(|arg| arg.len() + 1).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn context() -> ExecContext {
        ExecContext {
            session_id: Some("abc".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-5".into()),
            thinking_level: Some("high".into()),
        }
    }

    #[test]
    fn resumes_with_the_exact_session_id() {
        let built = build(Some("01abc"), None).expect("session id is enough");
        assert_eq!(built, argv(&["rpi", "--session-id", "01abc"]));
        assert!(validate(&built));
    }

    #[test]
    fn optional_flags_keep_the_restored_session_equivalent() {
        let built = build(Some("01abc"), Some(&context())).expect("full context");
        assert_eq!(
            built,
            argv(&[
                "rpi",
                "--session-id",
                "01abc",
                "--provider",
                "anthropic",
                "--model",
                "claude-sonnet-5",
                "--thinking",
                "high",
            ])
        );
    }

    #[test]
    fn never_uses_fuzzy_session_flags() {
        let built = build(Some("01abc"), Some(&context())).unwrap();
        for flag in ["--session", "-c", "--resume", "--continue"] {
            assert!(!built.iter().any(|arg| arg == flag), "{flag} is ambiguous");
        }
    }

    #[test]
    fn no_session_id_means_no_resume_command() {
        assert!(build(None, None).is_none());
        assert!(build(Some(""), None).is_none());
        assert!(build(Some("   "), None).is_none());
        let empty = ExecContext::default();
        assert!(build(None, Some(&empty)).is_none());
    }

    #[test]
    fn the_ephemeral_placeholder_is_not_a_resumable_session() {
        // `--no-session` stores the literal id `ephemeral`; resuming it would
        // silently start an unrelated conversation.
        assert!(build(Some("ephemeral"), None).is_none());
        assert!(build(Some("EPHEMERAL"), None).is_none());
        let context = ExecContext {
            session_id: Some("ephemeral".into()),
            ..ExecContext::default()
        };
        assert!(build(None, Some(&context)).is_none());
    }

    #[test]
    fn a_real_id_wins_over_the_ephemeral_placeholder() {
        let context = ExecContext {
            session_id: Some("ephemeral".into()),
            ..ExecContext::default()
        };
        let built = build(Some("01abc"), Some(&context)).expect("real id");
        assert_eq!(built[2], "01abc");
    }

    #[test]
    fn exec_context_supplies_a_fallback_session_id() {
        let built = build(None, Some(&context())).expect("context carries the id");
        assert_eq!(built[2], "abc");
    }

    #[test]
    fn a_quote_in_an_optional_value_drops_only_that_flag() {
        let poisoned = ExecContext {
            model: Some("bad'model".into()),
            ..context()
        };
        let built = build(Some("01abc"), Some(&poisoned)).expect("optional flags survive");
        assert_eq!(
            built,
            argv(&[
                "rpi",
                "--session-id",
                "01abc",
                "--provider",
                "anthropic",
                "--thinking",
                "high",
            ])
        );
    }

    #[test]
    fn a_blank_optional_value_is_skipped() {
        let spaced = ExecContext {
            provider: Some("   ".into()),
            model: Some("  claude-sonnet-5  ".into()),
            ..context()
        };
        let built = build(Some("01abc"), Some(&spaced)).expect("minimal argv");
        assert_eq!(
            built,
            argv(&[
                "rpi",
                "--session-id",
                "01abc",
                "--model",
                "claude-sonnet-5",
                "--thinking",
                "high",
            ])
        );
    }

    #[test]
    fn an_unsafe_explicit_id_falls_back_to_the_context_id() {
        // Dropping the whole command here would make a poisoned event id lose a
        // perfectly resumable session.
        let built = build(Some("bad'id"), Some(&context())).expect("context id is usable");
        assert_eq!(built[2], "abc");
        assert!(validate(&built));
    }

    #[test]
    fn a_control_character_in_an_explicit_id_also_falls_back() {
        let built = build(Some("bad\nid"), Some(&context())).expect("context id is usable");
        assert_eq!(built[2], "abc");
    }

    #[test]
    fn control_characters_are_dropped_without_poisoning_the_rest() {
        for value in ["line\nbreak", "tab\there", "nul\0byte", "cr\rreturn"] {
            let poisoned = ExecContext {
                provider: Some(value.into()),
                ..context()
            };
            let built = build(Some("01abc"), Some(&poisoned)).expect("minimal argv");
            assert!(
                !built.iter().any(|arg| arg == value),
                "{value:?} must not reach Herdr"
            );
            assert!(!built.iter().any(|arg| arg == "--provider"), "{built:?}");
            // The other optional values are still intact.
            assert!(built.iter().any(|arg| arg == "--model"), "{built:?}");
        }
    }

    #[test]
    fn a_poisoned_session_id_alone_drops_the_command_and_says_so() {
        // No usable fallback: the command is dropped, but `validate` remains a
        // pure argv-shape check (it does not require a session id).
        assert!(build(Some("bad'id"), None).is_none());
        assert!(build(Some("bad\nid"), None).is_none());
        assert!(
            validate(&argv(&["rpi"])),
            "validate checks argv shape, not resumability"
        );
    }

    #[test]
    fn a_whole_shell_line_as_one_argument_is_rejected() {
        // Herdr's documented failure: the resume command must be an argv array,
        // never a joined string.
        assert!(!validate(&argv(&["rpi --session-id abc"])));
        assert!(!validate(&argv(&["rpi --session-id", "abc"])));
        assert!(validate(&argv(&["rpi", "--session-id", "abc"])));
    }

    #[test]
    fn argv_zero_must_be_a_bare_command_name() {
        assert!(!validate(&argv(&["/usr/bin/rpi", "--session-id", "a"])));
        assert!(!validate(&argv(&["C:\\rpi.exe", "--session-id", "a"])));
        assert!(!validate(&argv(&["", "--session-id", "a"])));
        assert!(!validate(&argv(&["/usr/bin/herdr"])), "path is not a name");
        assert!(validate(&argv(&["rpi"])));
    }

    #[test]
    fn argument_count_limit_is_inclusive_at_64() {
        let mut at_limit = vec!["rpi".to_string()];
        at_limit.extend(std::iter::repeat_n("x".to_string(), MAX_ARGS - 1));
        assert_eq!(at_limit.len(), MAX_ARGS);
        assert!(validate(&at_limit));

        let mut over = at_limit.clone();
        over.push("x".to_string());
        assert!(!validate(&over));
    }

    #[test]
    fn total_length_limit_is_inclusive_at_8_kib() {
        // Two args: "rpi" plus a filler that makes the summed length land
        // exactly on the ceiling.
        let filler_len = MAX_ARGV_BYTES - ("rpi".len() + 1) - 1;
        let at_limit = argv(&["rpi", &"y".repeat(filler_len)]);
        assert_eq!(total_len(&at_limit), MAX_ARGV_BYTES);
        assert!(validate(&at_limit));

        let mut over = at_limit.clone();
        over.push("y".to_string());
        assert!(!validate(&over));
    }

    #[test]
    fn empty_argv_is_invalid() {
        assert!(!validate(&[]));
    }

    #[test]
    fn exec_context_parses_the_host_shape() {
        let raw: serde_json::Value = serde_json::json!({
            "model": {"id": "claude-sonnet-5", "provider": "anthropic", "api": "anthropic"},
            "thinkingLevel": "medium",
            "tools": ["bash"],
            "cwd": "/tmp",
            "sessionId": "01abc",
            "childAgent": false
        });
        let context = ExecContext::from_json(&raw);
        assert_eq!(
            context,
            ExecContext {
                session_id: Some("01abc".into()),
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-5".into()),
                thinking_level: Some("medium".into()),
            }
        );
    }

    #[test]
    fn exec_context_survives_errors_and_missing_fields() {
        assert_eq!(
            ExecContext::from_json(&serde_json::json!({"error": "no session"})),
            ExecContext::default()
        );
        assert_eq!(
            ExecContext::from_json(&serde_json::json!({"error": {"code": "x"}})),
            ExecContext::default()
        );
        // A null `error` is a success envelope: keep the payload.
        let ok = ExecContext::from_json(&serde_json::json!({
            "error": null,
            "sessionId": "01abc",
            "model": {"id": "claude-sonnet-5"}
        }));
        assert_eq!(ok.session_id.as_deref(), Some("01abc"));
        assert_eq!(ok.model.as_deref(), Some("claude-sonnet-5"));

        assert_eq!(
            ExecContext::from_json(&serde_json::json!({})),
            ExecContext::default()
        );
        // Flat shape as a fallback for hosts that do not nest the model.
        let flat = ExecContext::from_json(&serde_json::json!({
            "model": "claude-sonnet-5",
            "provider": "anthropic",
            "thinking": "low"
        }));
        assert_eq!(flat.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(flat.provider.as_deref(), Some("anthropic"));
        assert_eq!(flat.thinking_level.as_deref(), Some("low"));
    }

    #[test]
    fn a_provider_qualified_model_id_drops_the_duplicate_flag() {
        let context = ExecContext::from_json(&serde_json::json!({
            "model": {"id": "anthropic:claude-sonnet-5", "provider": "anthropic"},
            "sessionId": "01abc"
        }));
        assert_eq!(context.provider, None);
        let built = build(None, Some(&context)).unwrap();
        assert_eq!(
            built,
            argv(&[
                "rpi",
                "--session-id",
                "01abc",
                "--model",
                "anthropic:claude-sonnet-5"
            ])
        );
    }
}
