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
    /// and an `{"error": …}` reply yields an empty context.
    pub fn from_json(value: &serde_json::Value) -> Self {
        if value.get("error").is_some() {
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

/// A session id we can actually reopen: non-blank and not the ephemeral
/// placeholder. Anything else means "this session cannot be resumed".
fn usable_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && !id.eq_ignore_ascii_case(EPHEMERAL_SESSION_ID)).then_some(id)
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
/// Returns `None` when there is no usable session id, or when the candidate
/// command cannot satisfy Herdr's constraints.
pub fn build(session_id: Option<&str>, exec: Option<&ExecContext>) -> Option<Vec<String>> {
    let session_id = session_id
        .and_then(usable_id)
        .or_else(|| {
            exec.and_then(|c| c.session_id.as_deref())
                .and_then(usable_id)
        })
        .map(str::to_string);
    let session_id = session_id?;

    let mut argv = vec![
        COMMAND.to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
    ];

    if let Some(exec) = exec {
        // Optional flags are all-or-nothing individually: an unusable value is
        // skipped rather than poisoning the whole resume command.
        if let Some(provider) = &exec.provider {
            argv.push("--provider".to_string());
            argv.push(provider.clone());
        }
        if let Some(model) = &exec.model {
            argv.push("--model".to_string());
            argv.push(model.clone());
        }
        if let Some(level) = &exec.thinking_level {
            argv.push("--thinking".to_string());
            argv.push(level.clone());
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

/// Check every Herdr resume constraint. Exposed so tests and the reporter agree
/// on exactly one definition of "acceptable".
pub fn validate(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.is_empty() || program.contains(['/', '\\']) {
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

/// No single quotes (they would trip a shell-quoting restore path) and no
/// control characters (newlines, NULs, tabs) in any argument.
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
    fn a_quote_in_an_optional_value_drops_the_whole_argv() {
        let poisoned = ExecContext {
            model: Some("bad'model".into()),
            ..context()
        };
        let built = build(Some("01abc"), Some(&poisoned)).expect("minimal argv survives");
        assert_eq!(built, argv(&["rpi", "--session-id", "01abc"]));
    }

    #[test]
    fn control_characters_are_rejected_but_fall_back_to_the_minimum() {
        for value in ["line\nbreak", "tab\there", "nul\0byte", "cr\rreturn"] {
            let poisoned = ExecContext {
                provider: Some(value.into()),
                ..context()
            };
            let built = build(Some("01abc"), Some(&poisoned)).expect("minimal argv");
            assert_eq!(built.len(), 3, "{value:?} must not reach Herdr");
        }
    }

    #[test]
    fn a_poisoned_session_id_drops_the_command_entirely() {
        assert!(build(Some("bad'id"), None).is_none());
        assert!(build(Some("bad\nid"), None).is_none());
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
