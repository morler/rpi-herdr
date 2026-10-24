//! The agent-state machine reported to Herdr.
//!
//! Pure functions only: the reporter owns all side effects, so this table can
//! be unit tested exhaustively without a host.

/// The three states Herdr displays. `unknown` is reachable only by releasing.
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

    /// Next state after observing `input`.
    ///
    /// - `RunEnded` always returns to `Idle`: `AgentEnd` fires on the completed,
    ///   aborted and failed paths alike.
    /// - `PromptDismissed` returns from `Blocked` to `Working`, not `Idle`: every
    ///   rpi dialog (tool approval, `ask_user`, extension input) opens *inside* a
    ///   run that has not finished yet.
    pub fn next(self, input: Input) -> Self {
        match input {
            Input::SessionReady => self,
            Input::PromptStarted | Input::RunStarted => AgentState::Working,
            Input::RunEnded => AgentState::Idle,
            Input::PromptShown => AgentState::Blocked,
            Input::PromptDismissed => match self {
                AgentState::Blocked => AgentState::Working,
                other => other,
            },
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{AgentState, Input};
    use AgentState::{Blocked, Idle, Working};

    const ALL_INPUTS: [Input; 6] = [
        Input::SessionReady,
        Input::PromptStarted,
        Input::RunStarted,
        Input::RunEnded,
        Input::PromptShown,
        Input::PromptDismissed,
    ];

    #[test]
    fn transition_table_matches_the_design_document() {
        // (current, input, expected) — one row per cell of the 3x6 table.
        let table = [
            (Idle, Input::SessionReady, Idle),
            (Idle, Input::PromptStarted, Working),
            (Idle, Input::RunStarted, Working),
            (Idle, Input::RunEnded, Idle),
            (Idle, Input::PromptShown, Blocked),
            (Idle, Input::PromptDismissed, Idle),
            (Working, Input::SessionReady, Working),
            (Working, Input::PromptStarted, Working),
            (Working, Input::RunStarted, Working),
            (Working, Input::RunEnded, Idle),
            (Working, Input::PromptShown, Blocked),
            (Working, Input::PromptDismissed, Working),
            (Blocked, Input::SessionReady, Blocked),
            (Blocked, Input::PromptStarted, Working),
            (Blocked, Input::RunStarted, Working),
            (Blocked, Input::RunEnded, Idle),
            (Blocked, Input::PromptShown, Blocked),
            (Blocked, Input::PromptDismissed, Working),
        ];
        assert_eq!(table.len(), 3 * ALL_INPUTS.len(), "cover every cell");
        for (from, input, expected) in table {
            assert_eq!(from.next(input), expected, "{from:?} + {input:?}");
        }
    }

    #[test]
    fn dismissing_a_dialog_returns_to_working_not_idle() {
        assert_eq!(Blocked.next(Input::PromptDismissed), Working);
    }

    #[test]
    fn a_failed_or_aborted_run_still_lands_on_idle() {
        for from in [Idle, Working, Blocked] {
            assert_eq!(from.next(Input::RunEnded), Idle);
        }
    }

    #[test]
    fn wire_values_match_the_herdr_cli() {
        assert_eq!(Idle.as_str(), "idle");
        assert_eq!(Working.as_str(), "working");
        assert_eq!(Blocked.as_str(), "blocked");
    }
}
