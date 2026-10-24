//! Argv assembly for the two Herdr CLI calls this plugin makes.
//!
//! Kept separate from execution so the command shape can be asserted without
//! spawning anything.

use crate::state::AgentState;

/// Addressing for one report: which pane, and under which identity.
pub struct Target<'a> {
    pub pane_id: &'a str,
    pub agent_label: &'a str,
    pub source: &'a str,
}

impl<'a> Target<'a> {
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
        if let Some(message) = report.message.map(str::trim).filter(|m| !m.is_empty()) {
            argv.push("--message".to_string());
            argv.push(message.to_string());
        }
    }

    argv.push("--seq".to_string());
    argv.push(report.seq.to_string());

    if let Some(session_id) = report.session_id {
        argv.push("--agent-session-id".to_string());
        argv.push(session_id.to_string());
    }

    // The `--` separator is mandatory: without it Herdr parses the resume
    // command as its own flags. One argument per element — never a joined shell
    // string, which Herdr rejects as `invalid_resume_argv`.
    if let Some(resume_argv) = report.resume_argv.filter(|a| !a.is_empty()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentState;

    fn target() -> Target<'static> {
        Target {
            pane_id: "w18:p1",
            agent_label: "rpi",
            source: "rpi",
        }
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
        assert_eq!(argv[position(&argv, "--source") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--agent") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--state") + 1], "working");
        assert_eq!(argv[position(&argv, "--seq") + 1], "7");
        assert!(!argv.iter().any(|arg| arg == "--"), "no resume command");
    }

    #[test]
    fn the_source_never_uses_the_reserved_prefix() {
        let argv = report_agent(
            &target(),
            &Report {
                state: AgentState::Idle,
                message: None,
                seq: 1,
                session_id: None,
                resume_argv: None,
            },
        );
        let source = &argv[position(&argv, "--source") + 1];
        assert!(!source.starts_with("herdr:"));
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
                message: Some("   "),
                seq: 3,
                session_id: None,
                resume_argv: None,
            },
        );
        assert!(!argv.iter().any(|arg| arg == "--message"));
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
        assert_eq!(argv[position(&argv, "--source") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--agent") + 1], "rpi");
        assert_eq!(argv[position(&argv, "--seq") + 1], "11");
        assert!(
            !argv.iter().any(|arg| arg == "--state"),
            "no state on release"
        );
    }
}
