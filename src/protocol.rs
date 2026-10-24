//! Argv assembly for the two Herdr CLI calls this plugin makes.
//!
//! Kept separate from execution so the command shape can be asserted without
//! spawning anything.
//!
//! Every value that reaches an argv element is either a compile-time constant
//! (`"pane"`, `"--source"`, …), a numeric `seq`, or something this module
//! sanitises. Herdr rejects a *whole* report for one malformed element, so the
//! sanitising belongs here rather than in a caller's comment.

use crate::resume;
use crate::state::AgentState;

/// Herdr's ceiling for one `--message` value. A block explanation is for a
/// human in the sidebar, so anything longer is noise (and eats into the argv
/// budget shared with the resume command).
pub const MAX_MESSAGE_BYTES: usize = 512;

/// Addressing for one report: which pane, and under which identity.
///
/// Construct through [`Target::new`], which enforces the invariants Herdr
/// imposes on `--source` and on empty identifiers.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pane_id: &'a str,
    agent_label: &'a str,
    source: &'a str,
}

impl<'a> Target<'a> {
    /// Build a target, or `None` when an identity field is unusable.
    ///
    /// `--source` must never start with `herdr:`: that namespace belongs to
    /// Herdr's own bundled integrations, and colliding with it would bind our
    /// reports to theirs. Empty or whitespace-only values are equally unusable,
    /// and control characters would corrupt the invocation.
    pub fn new(pane_id: &'a str, agent_label: &'a str, source: &'a str) -> Option<Self> {
        if !is_usable_identity(pane_id) || !is_usable_identity(agent_label) {
            return None;
        }
        if !is_usable_identity(source) || source.trim().starts_with("herdr:") {
            return None;
        }
        Some(Self {
            pane_id,
            agent_label,
            source,
        })
    }

    fn base(&self, subcommand: &str) -> Vec<String> {
        vec![
            "pane".to_string(),
            subcommand.to_string(),
            self.pane_id.to_string(),
            "--source".to_string(),
            self.source.to_string(),
            "--agent".to_string(),
            self.agent_label.to_string(),
        ]
    }
}

/// One argv element that Herdr will accept as an identity.
fn is_usable_identity(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// One `pane report-agent` invocation.
pub struct Report<'a> {
    pub state: AgentState,
    pub message: Option<&'a str>,
    pub seq: u64,
    pub session_id: Option<&'a str>,
    /// Resume command, appended after `--`.
    pub resume_argv: Option<&'a [String]>,
}

/// `herdr pane report-agent <PANE> --source … --agent … --state … [--message]
/// [--seq] [--agent-session-id] [-- <ARGV>…]`.
pub fn report_agent(target: &Target<'_>, report: &Report<'_>) -> Vec<String> {
    let mut argv = target.base("report-agent");
    argv.push("--state".to_string());
    argv.push(report.state.as_str().to_string());

    // `--message` explains a block; Herdr ignores it for other states, so only
    // send it when there is something the user should read.
    if report.state == AgentState::Blocked {
        if let Some(message) = report.message.and_then(sanitize_message) {
            argv.push("--message".to_string());
            argv.push(message);
        }
    }

    argv.push("--seq".to_string());
    argv.push(report.seq.to_string());

    // Same trim/empty guard as the message: the `RPI_SESSION_ID` fallback can
    // yield a padded or blank value, and `--agent-session-id ""` mis-associates
    // the pane with nothing.
    if let Some(session_id) = report.session_id.and_then(sanitize_session_id) {
        argv.push("--agent-session-id".to_string());
        argv.push(session_id);
    }

    // The `--` separator is mandatory: without it Herdr parses the resume
    // command as its own flags. One argument per element — never a joined shell
    // string, which Herdr rejects as `invalid_resume_argv`. Re-validated here
    // because the field is public: a caller bypassing `resume::build` must not
    // be able to smuggle in an element Herdr will reject.
    if let Some(resume_argv) = report.resume_argv.filter(|argv| resume::validate(argv)) {
        argv.push("--".to_string());
        argv.extend(resume_argv.iter().cloned());
    }
    argv
}

/// `herdr pane release-agent <PANE> --source … --agent … --seq …`.
pub fn release_agent(target: &Target<'_>, seq: u64) -> Vec<String> {
    let mut argv = target.base("release-agent");
    argv.push("--seq".to_string());
    argv.push(seq.to_string());
    argv
}

/// Normalise a session id to one argv element, or `None` when there is nothing
/// worth sending.
fn sanitize_session_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) || value.contains('\'') {
        return None;
    }
    Some(value.to_string())
}

/// Collapse a block explanation into a single Herdr-acceptable argv element.
///
/// Newlines and tabs are control characters Herdr rejects outright, and a
/// single quote trips its quoting rules, so all three are normalised away
/// instead of dropping the message. The result is truncated on a character
/// boundary to [`MAX_MESSAGE_BYTES`].
fn sanitize_message(value: &str) -> Option<String> {
    let collapsed = value
        .split_whitespace()
        .map(|word| word.replace('\'', ""))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut truncated = collapsed;
    if truncated.len() > MAX_MESSAGE_BYTES {
        let mut cut = MAX_MESSAGE_BYTES;
        while cut > 0 && !truncated.is_char_boundary(cut) {
            cut -= 1;
        }
        truncated.truncate(cut);
        truncated = truncated.trim_end().to_string();
    }
    (!truncated.is_empty()).then_some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env;
    use crate::state::AgentState;

    fn target() -> Target<'static> {
        Target::new("w18:p1", "rpi", env::SOURCE_ID).expect("valid target")
    }

    fn position(argv: &[String], needle: &str) -> usize {
        argv.iter()
            .position(|arg| arg == needle)
            .unwrap_or_else(|| panic!("{needle} missing from {argv:?}"))
    }

    #[test]
    fn report_carries_the_stable_source_and_agent_label() {
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Working,
                message: None,
                seq: 7,
                session_id: None,
                resume_argv: None,
            },
        );
        assert_eq!(argv[0], "pane");
        assert_eq!(argv[1], "report-agent");
        assert_eq!(argv[2], "w18:p1");
        assert_eq!(argv[position(&argv, "--source") + 1], env::SOURCE_ID);
        assert_eq!(argv[position(&argv, "--agent") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--state") + 1], "working");
        assert_eq!(argv[position(&argv, "--seq") + 1], "7");
        assert!(!argv.iter().any(|arg| arg == "--"), "no resume command");
    }

    #[test]
    fn the_source_never_uses_the_reserved_prefix() {
        // Guards the production constant, not a fixture.
        assert!(!env::SOURCE_ID.starts_with("herdr:"));
        assert!(Target::new("w18:p1", "rpi", env::SOURCE_ID).is_some());
    }

    #[test]
    fn a_colliding_or_blank_identity_cannot_be_constructed() {
        for (pane, agent, source) in [
            ("", "rpi", "rpi"),
            ("   ", "rpi", "rpi"),
            ("w18:p1", "", "rpi"),
            ("w18:p1", "rpi", ""),
            ("w18:p1", "rpi", "herdr:pi"),
            ("w18:p1", "rpi", " herdr:pi"),
            ("w1\n8:p1", "rpi", "rpi"),
            ("w18:p1", "rp\ti", "rpi"),
        ] {
            assert!(
                Target::new(pane, agent, source).is_none(),
                "pane={pane:?} agent={agent:?} source={source:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_message_is_only_sent_while_blocked() {
        let blocked = report_agent(
            &target(),
            &Report {
                state: AgentState::Blocked,
                message: Some("Approve bash?"),
                seq: 3,
                session_id: None,
                resume_argv: None,
            },
        );
        assert_eq!(
            blocked[position(&blocked, "--message") + 1],
            "Approve bash?"
        );

        for state in [AgentState::Idle, AgentState::Working] {
            let argv = report_agent(
                &target(),
                &Report {
                    state,
                    message: Some("ignored"),
                    seq: 3,
                    session_id: None,
                    resume_argv: None,
                },
            );
            assert!(!argv.iter().any(|arg| arg == "--message"), "{state:?}");
        }
    }

    #[test]
    fn a_blank_message_is_not_sent() {
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Blocked,
                message: Some("   \n\t "),
                seq: 3,
                session_id: None,
                resume_argv: None,
            },
        );
        assert!(!argv.iter().any(|arg| arg == "--message"));
    }

    #[test]
    fn a_multiline_or_quoted_message_stays_one_safe_argv_element() {
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Blocked,
                message: Some("Don't run\nthis\tsh -c 'rm -rf /'"),
                seq: 3,
                session_id: None,
                resume_argv: None,
            },
        );
        let message = &argv[position(&argv, "--message") + 1];
        assert!(!message.contains('\''), "{message:?}");
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(!message.contains('\n'), "{message:?}");
        assert!(message.starts_with("Dont run this"), "{message:?}");
        assert!(resume::validate(std::slice::from_ref(message)) || !message.is_empty());
    }

    #[test]
    fn an_overlong_message_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Blocked,
                message: Some(&long),
                seq: 3,
                session_id: None,
                resume_argv: None,
            },
        );
        let message = &argv[position(&argv, "--message") + 1];
        assert!(message.len() <= MAX_MESSAGE_BYTES, "{}", message.len());
        assert!(message.chars().all(|c| c == 'é'), "{message:?}");
    }

    #[test]
    fn a_blank_session_id_is_not_sent() {
        for value in ["", "   ", "\n"] {
            let argv = report_agent(
                &target(),
                &Report {
                    state: AgentState::Idle,
                    message: None,
                    seq: 1,
                    session_id: Some(value),
                    resume_argv: None,
                },
            );
            assert!(
                !argv.iter().any(|arg| arg == "--agent-session-id"),
                "{value:?} must be filtered"
            );
        }
    }

    #[test]
    fn a_padded_session_id_is_trimmed() {
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Idle,
                message: None,
                seq: 1,
                session_id: Some("  01abc  "),
                resume_argv: None,
            },
        );
        assert_eq!(argv[position(&argv, "--agent-session-id") + 1], "01abc");
    }

    #[test]
    fn the_session_id_and_resume_command_follow_the_separator() {
        let resume = vec![
            "rpi".to_string(),
            "--session-id".to_string(),
            "01abc".to_string(),
        ];
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Idle,
                message: None,
                seq: 9,
                session_id: Some("01abc"),
                resume_argv: Some(&resume),
            },
        );
        assert_eq!(argv[position(&argv, "--agent-session-id") + 1], "01abc");

        let separator = position(&argv, "--");
        assert_eq!(&argv[separator + 1..], resume.as_slice());
        // Herdr rejects a single joined string; each element must be its own
        // argv entry.
        assert_eq!(argv[separator + 1], "rpi");
        assert_eq!(argv[separator + 2], "--session-id");
    }

    #[test]
    fn an_invalid_resume_command_is_dropped_rather_than_rejected_by_herdr() {
        // A public field: nothing stops a caller from skipping `resume::build`.
        let bad = vec!["rpi --session-id 01abc".to_string()];
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Idle,
                message: None,
                seq: 2,
                session_id: Some("01abc"),
                resume_argv: Some(&bad),
            },
        );
        assert!(!argv.iter().any(|arg| arg == "--"), "{argv:?}");
        // The state report itself still goes out.
        assert_eq!(state_of(&argv).as_deref(), Some("idle"));
    }

    #[test]
    fn an_empty_resume_command_omits_the_separator() {
        let empty: Vec<String> = Vec::new();
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Idle,
                message: None,
                seq: 2,
                session_id: Some("abc"),
                resume_argv: Some(&empty),
            },
        );
        assert!(!argv.iter().any(|arg| arg == "--"));
    }

    #[test]
    fn release_names_the_pane_source_agent_and_seq() {
        let argv = release_agent(&target(), 11);
        assert_eq!(argv[0], "pane");
        assert_eq!(argv[1], "release-agent");
        assert_eq!(argv[2], "w18:p1");
        assert_eq!(argv[position(&argv, "--source") + 1], env::SOURCE_ID);
        assert_eq!(argv[position(&argv, "--agent") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--seq") + 1], "11");
        assert!(
            !argv.iter().any(|arg| arg == "--state"),
            "no state on release"
        );
    }

    fn state_of(argv: &[String]) -> Option<String> {
        argv.iter()
            .position(|arg| arg == "--state")
            .and_then(|i| argv.get(i + 1).cloned())
    }
}
