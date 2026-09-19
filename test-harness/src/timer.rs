//! What the splitter did to the timer, recorded rather than acted on.

/// Mirrors `asr`'s timer state values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TimerState {
    #[default]
    NotRunning,
    Running,
    Paused,
    Ended,
}

impl TimerState {
    pub(crate) fn as_u32(self) -> u32 {
        match self {
            TimerState::NotRunning => 0,
            TimerState::Running => 1,
            TimerState::Paused => 2,
            TimerState::Ended => 3,
        }
    }
}

/// A call the splitter made, in the order it made it.
#[derive(Clone, Debug, PartialEq)]
pub enum TimerEvent {
    Start,
    Split,
    SkipSplit,
    UndoSplit,
    Reset,
    PauseGameTime,
    ResumeGameTime,
    SetGameTime { secs: i64, nanos: i32 },
    SetVariable { key: String, value: String },
}

/// The timer the splitter sees, and the record of what it asked for.
///
/// Deliberately not a real timer. It models only the transitions the splitter
/// reads back through `timer::state()`: start makes it running, reset stops it.
/// It does **not** end the run after the last split, because it has no notion of
/// a split count -- if a test comes to depend on `Ended`, that is the moment to
/// give it a segment list rather than to guess here.
#[derive(Default)]
pub struct Timer {
    pub state: TimerState,
    pub events: Vec<TimerEvent>,
}

impl Timer {
    pub(crate) fn record(&mut self, event: TimerEvent) {
        match event {
            TimerEvent::Start => self.state = TimerState::Running,
            TimerEvent::Reset => self.state = TimerState::NotRunning,
            _ => {}
        }
        self.events.push(event);
    }

    /// Events that control the run: starts, splits, skips, undos and resets.
    ///
    /// Everything else is excluded, and for the same reason in both cases --
    /// it happens constantly and would drown any assertion about what the
    /// splitter *did*. Writes to the status variable are the splitter talking
    /// to the runner, and the game-time calls are a second clock running
    /// alongside the run rather than anything that moves it on: `set_game_time`
    /// fires every tick of every run. Tests about game time read
    /// [`game_time`](Self::game_time) instead.
    pub fn run_control(&self) -> impl Iterator<Item = &TimerEvent> {
        self.events.iter().filter(|e| {
            !matches!(
                e,
                TimerEvent::SetVariable { .. }
                    | TimerEvent::SetGameTime { .. }
                    | TimerEvent::PauseGameTime
                    | TimerEvent::ResumeGameTime
            )
        })
    }

    /// Every game time the splitter set, in order, as seconds.
    pub fn game_time(&self) -> Vec<f64> {
        self.events
            .iter()
            .filter_map(|e| match e {
                TimerEvent::SetGameTime { secs, nanos } => {
                    Some(*secs as f64 + f64::from(*nanos) / 1e9)
                }
                _ => None,
            })
            .collect()
    }

    /// How many times the splitter asked the host to stop its own flow of game
    /// time. Sticky host state, so more than one is a bug.
    pub fn game_time_pauses(&self) -> usize {
        self.events
            .iter()
            .filter(|e| **e == TimerEvent::PauseGameTime)
            .count()
    }

    /// How many splits were taken, ignoring skips and undos.
    pub fn splits(&self) -> usize {
        self.events
            .iter()
            .filter(|e| **e == TimerEvent::Split)
            .count()
    }
}
