//! Herdr environment detection and plugin configuration.
//!
//! The Herdr integration protocol requires that an agent reports **only** while
//! it runs inside a Herdr pane, and stays completely silent otherwise. Every
//! condition below must hold for us to do anything observable:
//!
//! - `HERDR_ENV` is exactly `1` (any other value means "not in Herdr"),
//! - `HERDR_PANE_ID` is present and non-empty,
//! - `HERDR_BIN_PATH` is present, non-empty and points at an existing file,
//! - `RPI_HERDR` is not explicitly disabling the integration.
//!
//! `HERDR_SOCKET_PATH` is deliberately ignored: v1 talks to the CLI, which the
//! protocol documents as the portable path (Windows included).

use std::path::{Path, PathBuf};

/// Agent label Herdr shows in its sidebar and `herdr agent list`.
pub const DEFAULT_AGENT_LABEL: &str = "rpi";

/// `--source` value identifying this integration. Stable, unique, and never
/// prefixed with `herdr:` (that prefix belongs to Herdr's own bundled
/// integrations).
pub const SOURCE_ID: &str = "rpi";

/// Resolved Herdr context for this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrEnv {
    /// Pane this agent occupies (`HERDR_PANE_ID`).
    pub pane_id: String,
    /// Herdr CLI that manages this pane (`HERDR_BIN_PATH`).
    pub bin_path: PathBuf,
    /// Label Herdr displays for this agent (`--agent`).
    pub agent_label: String,
    /// Whether the resume command may be attached to reports.
    pub resume_enabled: bool,
}

/// Detect the Herdr environment. Returns `None` (silently, not as an error)
/// when any condition is missing.
pub fn detect() -> Option<HerdrEnv> {
    detect_with(|name| std::env::var(name).ok(), |path| path.is_file())
}

/// Whether this process drives an interactive TUI, as opposed to a headless run
/// (`-p`, `--mode json`, `--mode rpc`, `--server`, `--export`).
///
/// Only an interactive run may claim a Herdr pane. The host's own argv is the one
/// signal that survives every mode, including `/reload`:
///
/// - `BeforeTuiStart` is dispatched once, and a library loaded *later* by
///   `/reload` never sees it, so gating on that event leaves a reloaded plugin
///   permanently silent.
/// - `SessionStart` is useless for the same reason, and it also fires for `rpc`.
/// - stdout being a tty does not separate the two: `rpi -p` started from a shell
///   inherits the very same pty the TUI would use (measured).
/// - `GetCliFlag` would answer this directly, but the host panics on that action
///   (`{"error":"runtime_action panicked"}`), so it is not usable.
pub fn is_interactive_process() -> bool {
    interactive_from_args(std::env::args().skip(1))
}

/// Testable core of [`is_interactive_process`].
///
/// `rpi dev` / `dev-local` run the agent *in the same process* and forward the
/// real options after `--`, so for them only that tail matters; otherwise only
/// the flags before the first `--` count, because everything after it is a
/// message that may legitimately contain the text `-p`.
pub(crate) fn interactive_from_args(args: impl Iterator<Item = String>) -> bool {
    let args: Vec<String> = args.collect();
    let separator = args.iter().position(|a| a == "--");
    let dev_wrapper = match separator {
        Some(at) => args[..at].iter().any(|a| a == "dev" || a == "dev-local"),
        None => args.iter().any(|a| a == "dev" || a == "dev-local"),
    };
    let flags: &[String] = match (separator, dev_wrapper) {
        (Some(at), true) => &args[at + 1..],
        (Some(at), false) => &args[..at],
        (None, _) => &args,
    };

    let mut expects_mode_value = false;
    for flag in flags {
        if std::mem::take(&mut expects_mode_value) {
            if !is_interactive_mode(flag) {
                return false;
            }
            continue;
        }
        match flag.as_str() {
            "-p" | "--print" | "--server" | "--export" => return false,
            "--mode" => expects_mode_value = true,
            _ => {
                if let Some(mode) = flag.strip_prefix("--mode=") {
                    if !is_interactive_mode(mode) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Only the default `text` mode runs the TUI; `json` and `rpc` are headless.
fn is_interactive_mode(mode: &str) -> bool {
    !matches!(mode.trim(), "json" | "rpc")
}

/// Testable core of [`detect`]: `get` reads an environment variable and
/// `is_file` checks that the Herdr binary still exists.
pub fn detect_with(
    get: impl Fn(&str) -> Option<String>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<HerdrEnv> {
    if get("HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    if is_disabled(get("RPI_HERDR").as_deref()) {
        return None;
    }

    let pane_id = non_empty(get("HERDR_PANE_ID"))?;
    let bin_path = PathBuf::from(non_empty(get("HERDR_BIN_PATH"))?);
    // A stale path (Herdr upgraded mid-run) would otherwise make every turn
    // spawn a process just to fail; require the file up front. This only proves
    // the path is a regular file — an existing but non-executable binary still
    // passes and fails at spawn time, which the reporter treats as a dropped
    // report. Probing the exec bit is not portable (Windows has none), so the
    // check stays deliberately shallow.
    if !is_file(&bin_path) {
        return None;
    }

    let agent_label =
        non_empty(get("RPI_HERDR_AGENT")).unwrap_or_else(|| DEFAULT_AGENT_LABEL.to_string());

    Some(HerdrEnv {
        pane_id,
        bin_path,
        agent_label,
        resume_enabled: !is_off(get("RPI_HERDR_RESUME").as_deref()),
    })
}

/// Canonicalise an environment value: surrounding whitespace is dropped, and a
/// value that was only whitespace counts as unset. Forwarding the raw value
/// would leak padding into `--agent` / `--pane` and mismatch Herdr's own ids.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `RPI_HERDR=0|false|off|no|disabled` (case-insensitive) disables the
/// integration.
fn is_disabled(value: Option<&str>) -> bool {
    is_off(value)
}

/// Treat `0`, `false`, `off`, `no` and `disabled` as "off"; everything else
/// (including absence) means "enabled".
fn is_off(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no") | Some("disabled")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned environment reader: no borrow of the test's temporary slice.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn present(_: &Path) -> bool {
        true
    }

    fn absent(_: &Path) -> bool {
        false
    }

    #[test]
    fn detects_a_complete_environment() {
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w18:p1"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
        ]);
        let env = detect_with(get, present).expect("complete environment");
        assert_eq!(env.pane_id, "w18:p1");
        assert_eq!(env.bin_path, PathBuf::from("/usr/bin/herdr"));
        assert_eq!(env.agent_label, DEFAULT_AGENT_LABEL);
        assert!(env.resume_enabled);
    }

    #[test]
    fn outside_herdr_every_missing_variable_is_silent() {
        for missing in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH"] {
            let pairs: Vec<(&str, &str)> = [
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w18:p1"),
                ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ]
            .into_iter()
            .filter(|(key, _)| *key != missing)
            .collect();
            assert!(
                detect_with(env_of(&pairs), present).is_none(),
                "missing {missing} must disable the integration"
            );
        }
    }

    #[test]
    fn herdr_env_must_be_exactly_one() {
        for value in ["0", "", "true", "yes", " 1"] {
            let get = env_of(&[
                ("HERDR_ENV", value),
                ("HERDR_PANE_ID", "w18:p1"),
                ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ]);
            assert!(detect_with(get, present).is_none(), "HERDR_ENV={value:?}");
        }
    }

    /// Shorthand: `interactive(&["-p"])`.
    fn interactive(args: &[&str]) -> bool {
        interactive_from_args(args.iter().map(|a| (*a).to_string()))
    }

    #[test]
    fn a_plain_invocation_is_interactive() {
        assert!(interactive(&[]));
        assert!(interactive(&["--no-session", "--no-tools"]));
        assert!(interactive(&["hello there"]));
        assert!(interactive(&["--mode", "text"]));
        assert!(interactive(&["--mode=text"]));
    }

    #[test]
    fn every_headless_mode_is_detected() {
        // These run without a TUI, so claiming a Herdr pane would be a lie — and
        // releasing on exit is what used to blank a *working* parent agent out
        // of the sidebar.
        assert!(!interactive(&["-p", "hi"]));
        assert!(!interactive(&["--print", "hi"]));
        assert!(!interactive(&["--mode", "json", "-p", "hi"]));
        assert!(!interactive(&["--mode", "rpc"]));
        assert!(!interactive(&["--mode=json"]));
        assert!(!interactive(&["--mode=rpc"]));
        assert!(!interactive(&["--server"]));
        assert!(!interactive(&["--export", "out.html"]));
        // A headless flag anywhere still wins.
        assert!(!interactive(&["--no-tools", "-p", "hi"]));
        assert!(!interactive(&["--no-tools", "--mode", "json"]));
    }

    #[test]
    fn a_message_that_mentions_a_headless_flag_is_not_one() {
        // Only the options before `--` count; a prompt may contain anything.
        assert!(interactive(&["--", "-p", "explain this flag"]));
        assert!(interactive(&["--mode", "text", "--", "--mode", "json"]));
    }

    #[test]
    fn the_dev_wrapper_is_transparent() {
        // `rpi dev` runs the agent in-process and forwards real options after
        // `--`, so the wrapper's own flags must not decide anything.
        assert!(interactive(&["dev", "--no-watch", "--", "--no-session"]));
        assert!(!interactive(&["dev", "--no-watch", "--", "-p", "hi"]));
        assert!(!interactive(&["dev-local", "--", "--mode", "json"]));
        assert!(interactive(&["dev", "--release"]));
        // A dev wrapper plus dev-only flags but no separator is still a TUI.
        assert!(interactive(&["dev-local", "--no-watch"]));
    }

    #[test]
    fn empty_pane_id_is_not_an_environment() {
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "   "),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
        ]);
        assert!(detect_with(get, present).is_none());
    }

    #[test]
    fn missing_binary_on_disk_disables_reporting() {
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w18:p1"),
            ("HERDR_BIN_PATH", "/opt/removed/herdr"),
        ]);
        assert!(detect_with(get, absent).is_none());
    }

    #[test]
    fn explicit_opt_out_disables_the_integration() {
        for value in ["0", "false", "OFF", "no", "Disabled"] {
            let get = env_of(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w18:p1"),
                ("HERDR_BIN_PATH", "/usr/bin/herdr"),
                ("RPI_HERDR", value),
            ]);
            assert!(detect_with(get, present).is_none(), "RPI_HERDR={value:?}");
        }
        // Anything else (including an empty value) keeps it enabled.
        for value in ["1", "true", "on", ""] {
            let get = env_of(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w18:p1"),
                ("HERDR_BIN_PATH", "/usr/bin/herdr"),
                ("RPI_HERDR", value),
            ]);
            assert!(detect_with(get, present).is_some(), "RPI_HERDR={value:?}");
        }
    }

    #[test]
    fn agent_label_override_and_resume_switch() {
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w18:p1"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ("RPI_HERDR_AGENT", "rpi-nightly"),
            ("RPI_HERDR_RESUME", "0"),
        ]);
        let env = detect_with(get, present).expect("complete environment");
        assert_eq!(env.agent_label, "rpi-nightly");
        assert!(!env.resume_enabled, "RPI_HERDR_RESUME=0 keeps state only");
    }

    #[test]
    fn padding_is_stripped_from_every_exported_value() {
        // Herdr's pane id and agent label are matched verbatim, so a padded
        // variable must not reach the CLI.
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "  w18:p1 "),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ("RPI_HERDR_AGENT", " rpi \t"),
        ]);
        let env = detect_with(get, present).expect("complete environment");
        assert_eq!(env.pane_id, "w18:p1");
        assert_eq!(env.agent_label, "rpi");
    }

    #[test]
    fn blank_agent_label_falls_back_to_the_default() {
        let get = env_of(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w18:p1"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ("RPI_HERDR_AGENT", "  "),
        ]);
        let env = detect_with(get, present).expect("complete environment");
        assert_eq!(env.agent_label, DEFAULT_AGENT_LABEL);
    }
}
