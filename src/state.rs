//! The agent-state machine reported to Herdr.
//!
//! Pure functions only: the reporter owns all side effects, so this machine can
//! be unit tested exhaustively without a host.
//!
//! Herdr displays four states in total, but only three are reportable by us:
//! `idle`, `working` and `blocked`. The fourth, `unknown`, is what Herdr falls
//! back to after `pane release-agent` — it is produced by releasing, not by a
//! transition, so it is deliberately absent from [`AgentState`].

/// The states we ever report. `unknown` is reachable only by releasing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    /// Wire value for `herdr pane report-agent --state`.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
        }
    }

    /// Every reportable state, so tests can enumerate the table without a
    /// hand-maintained list that silently falls behind `AgentState`.
    #[cfg(test)]
    pub const ALL: [AgentState; 3] = [AgentState::Idle, AgentState::Working, AgentState::Blocked];
}

/// Everything that can move the state machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// `SessionStart` — only TUI, REPL and `--server`/`--mode rpc` sessions.
    SessionReady,
    /// `BeforeAgentStart` — once per user prompt, in every mode.
    PromptStarted,
    /// `AgentStart`.
    RunStarted,
    /// `AgentEnd` (completed, aborted or failed).
    RunEnded,
    /// `UiPromptStart` — an interactive dialog is waiting on the user.
    PromptShown,
    /// `UiPromptEnd`.
    PromptDismissed,
}

impl Input {
    /// Every input, so tests can prove per-cell coverage. Adding a variant
    /// breaks the build here until it is listed.
    #[cfg(test)]
    pub const ALL: [Input; 6] = [
        Input::SessionReady,
        Input::PromptStarted,
        Input::RunStarted,
        Input::RunEnded,
        Input::PromptShown,
        Input::PromptDismissed,
    ];
}

/// The reported state plus whether a run is currently in flight.
///
/// The extra bit is what makes `PromptDismissed` decidable: a dialog opened
/// *inside* a run returns to `Working`, while a dialog observed outside any run
/// (extension/startup prompt) must go back to `Idle` rather than leaving the
/// pane stuck on `working`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Machine {
    state: AgentState,
    run_active: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            state: AgentState::Idle,
            run_active: false,
        }
    }
}

impl Machine {
    /// Current reported state.
    pub fn state(self) -> AgentState {
        self.state
    }

    /// Next machine after observing `input`.
    ///
    /// - `RunEnded` always returns to `Idle` and clears the run flag: `AgentEnd`
    ///   fires on the completed, aborted and failed paths alike.
    /// - `PromptDismissed` returns from `Blocked` to `Working` only while a run
    ///   is still in flight; otherwise it returns to `Idle`.
    pub fn next(self, input: Input) -> Self {
        match input {
            Input::SessionReady => self,
            Input::PromptStarted | Input::RunStarted => Self {
                state: AgentState::Working,
                run_active: true,
            },
            Input::RunEnded => Self {
                state: AgentState::Idle,
                run_active: false,
            },
            Input::PromptShown => Self {
                state: AgentState::Blocked,
                ..self
            },
            Input::PromptDismissed => Self {
                state: if self.run_active {
                    AgentState::Working
                } else {
                    AgentState::Idle
                },
                ..self
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentState, Input, Machine};
    use AgentState::{Blocked, Idle, Working};

    fn machine(state: AgentState, run_active: bool) -> Machine {
        Machine { state, run_active }
    }

    #[test]
    fn transition_table_matches_the_design_document() {
        // (current, run_active, input, expected) — one row per cell of the
        // 3 x 6 x 2 table. `run_active` matters only for `PromptDismissed`, but
        // every combination is spelled out so a new input cannot slip through.
        let table = [
            (Idle, false, Input::SessionReady, Idle),
            (Idle, false, Input::PromptStarted, Working),
            (Idle, false, Input::RunStarted, Working),
            (Idle, false, Input::RunEnded, Idle),
            (Idle, false, Input::PromptShown, Blocked),
            (Idle, false, Input::PromptDismissed, Idle),
            (Idle, true, Input::SessionReady, Idle),
            (Idle, true, Input::PromptStarted, Working),
            (Idle, true, Input::RunStarted, Working),
            (Idle, true, Input::RunEnded, Idle),
            (Idle, true, Input::PromptShown, Blocked),
            (Idle, true, Input::PromptDismissed, Working),
            (Working, false, Input::SessionReady, Working),
            (Working, false, Input::PromptStarted, Working),
            (Working, false, Input::RunStarted, Working),
            (Working, false, Input::RunEnded, Idle),
            (Working, false, Input::PromptShown, Blocked),
            (Working, false, Input::PromptDismissed, Idle),
            (Working, true, Input::SessionReady, Working),
            (Working, true, Input::PromptStarted, Working),
            (Working, true, Input::RunStarted, Working),
            (Working, true, Input::RunEnded, Idle),
            (Working, true, Input::PromptShown, Blocked),
            (Working, true, Input::PromptDismissed, Working),
            (Blocked, false, Input::SessionReady, Blocked),
            (Blocked, false, Input::PromptStarted, Working),
            (Blocked, false, Input::RunStarted, Working),
            (Blocked, false, Input::RunEnded, Idle),
            (Blocked, false, Input::PromptShown, Blocked),
            (Blocked, false, Input::PromptDismissed, Idle),
            (Blocked, true, Input::SessionReady, Blocked),
            (Blocked, true, Input::PromptStarted, Working),
            (Blocked, true, Input::RunStarted, Working),
            (Blocked, true, Input::RunEnded, Idle),
            (Blocked, true, Input::PromptShown, Blocked),
            (Blocked, true, Input::PromptDismissed, Working),
        ];
        for (from, run_active, input, expected) in table {
            assert_eq!(
                machine(from, run_active).next(input).state(),
                expected,
                "{from:?} (run_active={run_active}) + {input:?}"
            );
        }

        // Per-cell coverage: each (state, run_active, input) appears exactly
        // once, so a duplicated row cannot mask a missing one.
        for from in AgentState::ALL {
            for run_active in [false, true] {
                for input in Input::ALL {
                    let rows = table
                        .iter()
                        .filter(|(f, r, i, _)| *f == from && *r == run_active && *i == input)
                        .count();
                    assert_eq!(rows, 1, "{from:?}/{run_active}/{input:?} exactly once");
                }
            }
        }
        assert_eq!(table.len(), AgentState::ALL.len() * Input::ALL.len() * 2);
    }

    #[test]
    fn dismissing_a_dialog_returns_to_working_not_idle() {
        let blocked = Machine {
            state: Blocked,
            run_active: true,
        };
        assert_eq!(blocked.next(Input::PromptDismissed).state(), Working);
    }

    #[test]
    fn dismissing_a_dialog_outside_a_run_returns_to_idle() {
        // A startup/extension prompt is not part of a run; reporting `working`
        // there would strand the pane until the next run.
        let blocked = Machine {
            state: Blocked,
            run_active: false,
        };
        assert_eq!(blocked.next(Input::PromptDismissed).state(), Idle);
    }

    #[test]
    fn a_pending_dialog_survives_the_end_of_its_run() {
        // `AgentEnd` must not leave the pane blocked forever.
        let blocked = Machine {
            state: Blocked,
            run_active: true,
        };
        let ended = blocked.next(Input::RunEnded);
        assert_eq!(ended.state(), Idle);
        assert!(!ended.run_active);
        assert_eq!(ended.next(Input::PromptDismissed).state(), Idle);
    }

    #[test]
    fn a_failed_or_aborted_run_still_lands_on_idle() {
        for from in AgentState::ALL {
            for run_active in [false, true] {
                let next = machine(from, run_active).next(Input::RunEnded);
                assert_eq!(next.state(), Idle, "{from:?}/{run_active}");
                assert!(!next.run_active, "{from:?}/{run_active}");
            }
        }
    }

    #[test]
    fn starting_a_run_tracks_the_active_flag() {
        let started = Machine::default();
        assert!(!started.run_active);
        for input in [Input::PromptStarted, Input::RunStarted] {
            let next = started.next(input);
            assert_eq!(next.state(), Working, "{input:?}");
            assert!(next.run_active, "{input:?}");
        }
    }

    #[test]
    fn wire_values_match_the_herdr_cli() {
        assert_eq!(Idle.as_str(), "idle");
        assert_eq!(Working.as_str(), "working");
        assert_eq!(Blocked.as_str(), "blocked");
        assert_eq!(AgentState::ALL.len(), 3, "no `unknown` state to report");
    }
}
