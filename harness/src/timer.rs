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

    /// Events that control the run, which is everything except writes to the
    /// status variable. The splitter uses that variable to talk to the runner,
    /// so it is chatter rather than timer control and drowns assertions.
    pub fn run_control(&self) -> impl Iterator<Item = &TimerEvent> {
        self.events
            .iter()
            .filter(|e| !matches!(e, TimerEvent::SetVariable { .. }))
    }

    /// How many splits were taken, ignoring skips and undos.
    pub fn splits(&self) -> usize {
        self.events
            .iter()
            .filter(|e| **e == TimerEvent::Split)
            .count()
    }
}
