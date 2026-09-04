//! A whole run, replayed: the splitter watched doing the thing it exists to do.
//!
//! Every other test here works from a single capture, which is one instant and
//! so has no changes in it. It can say *the wonder was already unlocked*, never
//! *the split fired when it became unlocked*. This replays a recording of a real
//! run -- the states it passed through, in order -- and watches the splits
//! actually fire.
//!
//! Recorded with `tb-record`; see snapshots/README.md. Characterization, like
//! the rest of the snapshot suite: a game update can turn it red with nothing
//! wrong in the code.

use test_harness::{scenario::Scenario, timer::TimerEvent, World};

/// Ticks to give a step before moving on regardless.
///
/// Generous because the first steps include full heap sweeps, budgeted at
/// 32 MiB a tick over some 4.5 GiB. A step the splitter has nothing to say
/// about costs nothing to skip, so this only sets the worst case.
const PER_STEP: usize = 4000;

struct Replay {
    /// `(step index, what the splitter did there)`.
    fired: Vec<(usize, TimerEvent)>,
    log: Vec<String>,
    /// The step labels as recorded, so the test can say what *should* have
    /// happened where.
    recorded: Vec<String>,
}

fn replay() -> Replay {
    let scenario = Scenario::find("wonder-run").unwrap_or_else(|e| panic!("{e}"));
    let recorded = scenario.events();
    let (process, playhead) = scenario.into_process();

    let mut fired = Vec::new();
    let mut seen = 0usize;
    let mut budget = 0usize;

    let world = test_harness::drive_with(
        World::new().with_process(process),
        timberborn_autosplitter::main(),
        200_000,
        |_, world| {
            budget += 1;
            let events: Vec<TimerEvent> = world.timer.run_control().cloned().collect();
            let acted = events.len() > seen;
            if acted {
                for event in &events[seen..] {
                    fired.push((playhead.position(), event.clone()));
                }
                seen = events.len();
            }
            // Move on as soon as the splitter has reacted, or once the step has
            // had long enough that it plainly is not going to.
            if acted || budget >= PER_STEP {
                budget = 0;
                return playhead.advance();
            }
            true
        },
    );

    Replay {
        fired,
        log: world.log,
        recorded,
    }
}

fn replayed() -> &'static Replay {
    static ONCE: std::sync::OnceLock<Replay> = std::sync::OnceLock::new();
    ONCE.get_or_init(replay)
}

/// The whole category, offline: the timer starts and all seven splits fire.
#[test]
fn fires_the_whole_category() {
    let events: Vec<&TimerEvent> = replayed().fired.iter().map(|(_, e)| e).collect();
    let expected = [
        TimerEvent::Start,
        TimerEvent::Split,
        TimerEvent::Split,
        TimerEvent::Split,
        TimerEvent::Split,
        TimerEvent::Split,
        TimerEvent::Split,
        TimerEvent::Split,
    ];
    assert_eq!(
        events.len(),
        expected.len(),
        "expected a start and seven splits, got {events:?}"
    );
    for (got, want) in events.iter().zip(expected.iter()) {
        assert_eq!(*got, want);
    }
}

/// Not just that eight things happened, but that each happened at the moment it
/// happened during the run. A splitter that fired all seven splits at once, or
/// one step late, would satisfy the count and be badly wrong.
#[test]
fn fires_them_where_they_were_recorded() {
    let expected: Vec<usize> = replayed()
        .recorded
        .iter()
        .enumerate()
        .filter(|(_, label)| *label == "start" || *label == "split")
        .map(|(index, _)| index)
        .collect();
    let actual: Vec<usize> = replayed().fired.iter().map(|(step, _)| *step).collect();

    assert_eq!(
        actual,
        expected,
        "the splitter acted at different steps than the recording did.\n\
         recorded steps: {:?}",
        replayed().recorded
    );
}

/// The run start is only bound while the scene is still loading; the window is
/// about one scan wide. A recording that skipped it made the splitter see a
/// game already in progress and refuse to start a timer -- correctly, and
/// uselessly. This is that window.
#[test]
fn binds_the_run_start_during_the_load() {
    assert!(
        replayed()
            .log
            .iter()
            .any(|line| line.contains("run start bound during the load")),
        "log was {:#?}",
        replayed().log
    );
    assert!(
        !replayed()
            .log
            .iter()
            .any(|line| line.contains("Not starting the timer")),
        "the splitter declined to start; log was {:#?}",
        replayed().log
    );
}

/// The splits in order, by what they were for. The count and the timing could
/// both be right with the wrong buildings behind them.
#[test]
fn splits_for_the_right_things_in_the_right_order() {
    let reasons: Vec<&str> = replayed()
        .log
        .iter()
        .filter_map(|line| {
            line.strip_prefix("Split: ")
                .or_else(|| line.strip_prefix("Run end: "))
        })
        .collect();
    assert_eq!(
        reasons,
        [
            "Forester finished.",
            "Gear Workshop finished.",
            "Tapper's Shack finished.",
            "Observatory / Numbercruncher finished.",
            "Smelter + Wood Workshop finished.",
            "wonder unlocked.",
            "Congratulations screen. Splitting.",
        ]
    );
}

/// Activating the wonder is not the end of the run; the Congratulations screen
/// is, about half an in-game hour later. Splitting at activation would be the
/// easy mistake, and would cost every runner half an hour of game time.
#[test]
fn does_not_split_when_the_wonder_is_activated() {
    let activated = replayed()
        .recorded
        .iter()
        .position(|label| label.starts_with("wonder-activated"))
        .expect("the recording should hold the wonder's activation");
    assert!(
        !replayed().fired.iter().any(|(step, _)| *step == activated),
        "the splitter split at the wonder's activation, which is not the run end"
    );
}
