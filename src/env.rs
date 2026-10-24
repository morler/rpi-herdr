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

use std::path::PathBuf;

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

/// Testable core of [`detect`]: `get` reads an environment variable and
/// `is_file` checks that the Herdr binary still exists.
pub fn detect_with(
    get: impl Fn(&str) -> Option<String>,
    is_file: impl Fn(&PathBuf) -> bool,
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
    // spawn a process just to fail; require the file up front.
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

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// `RPI_HERDR=0|false|off` (case-insensitive) disables the integration.
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

    fn present(_: &PathBuf) -> bool {
        true
    }

    fn absent(_: &PathBuf) -> bool {
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
