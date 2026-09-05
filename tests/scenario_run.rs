//! Whole runs, replayed: the splitter watched doing the thing it exists to do.
//!
//! Every other test here works from a single capture, which is one instant and
//! so has no changes in it. It can say *the wonder was already unlocked*, never
//! *the split fired when it became unlocked*. This replays recordings of real
//! runs -- the states they passed through, in order -- and watches the splits
//! actually fire.
//!
//! Recorded with `tb-record`; see snapshots/README.md. Characterization, like
//! the rest of the snapshot suite: a game update can turn one red with nothing
//! wrong in the code.

use test_harness::{scenario::Scenario, timer::TimerEvent, World};

/// Ticks to give a step before moving on regardless.
///
/// Generous because the early steps include full heap sweeps, budgeted at
/// 32 MiB a tick over some 4.5 GiB. A step the splitter has nothing to say
/// about costs nothing to skip, so this only sets the worst case.
const PER_STEP: usize = 4000;

struct Replay {
    /// Which recording this was, so a failure says which run.
    name: String,
    /// `(step index, what the splitter did there)`.
    fired: Vec<(usize, TimerEvent)>,
    log: Vec<String>,
    /// The step labels as recorded, so a test can say what *should* have
    /// happened where.
    recorded: Vec<String>,
}

fn replay(name: &str, dirs: &[std::path::PathBuf]) -> Replay {
    let scenario = Scenario::open(dirs).unwrap_or_else(|e| panic!("{e}"));
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
        name: name.to_owned(),
        fired,
        log: world.log,
        recorded,
    }
}

/// Every recorded run, replayed once and shared.
///
/// A pass costs about a minute, nearly all of it sweeping the heap, so paying
/// it per test would make the suite unusable. Running every recording is the
/// point of having more than one: the same category played as Folktails and as
/// Iron Teeth goes down different code, since the splitter matches
/// faction-suffixed template names, and every assertion here should hold for
/// both.
fn replayed() -> &'static [Replay] {
    static ONCE: std::sync::OnceLock<Vec<Replay>> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        Scenario::all("wonder-run")
            .unwrap_or_else(|e| panic!("{e}"))
            .iter()
            .map(|(name, dirs)| replay(name, dirs))
            .collect()
    })
}

/// The whole category, offline: the timer starts and all seven splits fire.
#[test]
fn fires_the_whole_category() {
    for run in replayed() {
        let events: Vec<&TimerEvent> = run.fired.iter().map(|(_, e)| e).collect();
        assert_eq!(
            events.len(),
            8,
            "{}: expected a start and seven splits, got {events:?}",
            run.name
        );
        assert_eq!(events[0], &TimerEvent::Start, "{}", run.name);
        for event in &events[1..] {
            assert_eq!(**event, TimerEvent::Split, "{}", run.name);
        }
    }
}

/// Not just that eight things happened, but that each happened at the moment it
/// happened during the run. A splitter that fired all seven splits at once, or
/// one step late, would satisfy a count and be badly wrong.
#[test]
fn fires_them_where_they_were_recorded() {
    for run in replayed() {
        let expected: Vec<usize> = run
            .recorded
            .iter()
            .enumerate()
            .filter(|(_, label)| *label == "start" || *label == "split")
            .map(|(index, _)| index)
            .collect();
        let actual: Vec<usize> = run.fired.iter().map(|(step, _)| *step).collect();

        assert_eq!(
            actual, expected,
            "{}: the splitter acted at different steps than the recording did.\n\
             recorded steps: {:?}",
            run.name, run.recorded
        );
    }
}

/// The run start is only bound while the scene is still loading, a window about
/// one scan wide. A recording that skipped it made the splitter see a game
/// already in progress and refuse to start a timer -- correctly, and uselessly.
/// This is that window.
#[test]
fn binds_the_run_start_during_the_load() {
    for run in replayed() {
        assert!(
            run.log
                .iter()
                .any(|line| line.contains("run start bound during the load")),
            "{}: log was {:#?}",
            run.name,
            run.log
        );
        assert!(
            !run.log
                .iter()
                .any(|line| line.contains("Not starting the timer")),
            "{}: the splitter declined to start; log was {:#?}",
            run.name,
            run.log
        );
    }
}

/// The splits in order, by what they were for. The count and the timing can
/// both be right with the wrong buildings behind them.
#[test]
fn splits_for_the_right_things_in_the_right_order() {
    for run in replayed() {
        let reasons: Vec<&str> = run
            .log
            .iter()
            .filter_map(|line| {
                line.strip_prefix("Split: ")
                    .or_else(|| line.strip_prefix("Run end: "))
            })
            .collect();
        // One label covers both factions' advanced science building, which is
        // why this reads identically for Folktails and Iron Teeth.
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
            ],
            "{}",
            run.name
        );
    }
}

/// Activating the wonder is not the end of the run; the Congratulations screen
/// is, about half an in-game hour later. Splitting at activation is the easy
/// mistake, and would cost every runner that half hour.
#[test]
fn does_not_split_when_the_wonder_is_activated() {
    for run in replayed() {
        let activated = run
            .recorded
            .iter()
            .position(|label| label.starts_with("wonder-activated"))
            .unwrap_or_else(|| {
                panic!(
                    "{}: the recording should hold the wonder's activation",
                    run.name
                )
            });
        assert!(
            !run.fired.iter().any(|(step, _)| *step == activated),
            "{}: the splitter split at the wonder's activation, which is not the run end",
            run.name
        );
    }
}

/// Says which recordings are in play. A suite covering one run when two are
/// recorded would look identical from the outside.
#[test]
fn reports_which_runs_are_being_replayed() {
    let names: Vec<&str> = replayed().iter().map(|r| r.name.as_str()).collect();
    println!("wonder-run recordings in use: {names:?}");
    assert!(!names.is_empty());
}

/// Both sweeps happen before a game exists, which is the point of doing them
/// in that order.
///
/// A recording begins at the main menu, so a sweep that appears before the
/// first game scene loaded is one a runner paid for while the process was at
/// its smallest. Getting this wrong is invisible from the outside and was:
/// resolving the clock first blocked on a vtable Mono does not fill in until a
/// game scene is built, which pushed both sweeps into the first load. Only a
/// live run showed it, and this is what stops it coming back.
#[test]
fn both_sweeps_land_before_the_first_game() {
    for run in replayed() {
        let found_table = run
            .log
            .iter()
            .position(|line| line.contains("[table] found at"))
            .unwrap_or_else(|| panic!("{}: no table was found; log was {:#?}", run.name, run.log));
        let first_game = run
            .log
            .iter()
            .position(|line| line.contains("A game scene loaded"))
            .unwrap_or_else(|| panic!("{}: no game ever loaded", run.name));
        assert!(
            found_table < first_game,
            "{}: the table was not found until after a game had loaded; log was {:#?}",
            run.name,
            run.log
        );
        // Tighter than "before the game loaded": before anything
        // timing-critical. The run start is bound inside the load window, and
        // the table has to be there already or that bind pays for a sweep.
        let bound_run_start = run
            .log
            .iter()
            .position(|line| line.contains("Watching run start at"))
            .unwrap_or(usize::MAX);
        assert!(
            found_table < bound_run_start,
            "{}: the table was not found until the run start had been bound; log was {:#?}",
            run.name,
            run.log
        );
        // And the anchor's is the first sweep there is. The run start's may
        // follow it inside the load -- see `the_first_game_sweeps_only_for_...`
        // -- but nothing may precede it.
        let first_sweep = run
            .log
            .iter()
            .find(|line| line.contains("starting (full sweep"))
            .unwrap_or_else(|| panic!("{}: nothing swept at all", run.name));
        assert!(
            first_sweep.contains("SceneLoader"),
            "{}: the first sweep was not the anchor's: {first_sweep:?}",
            run.name
        );
    }
}

/// Two games in one process, which is what a runner resetting for another
/// attempt actually does.
///
/// Replayed separately from the wonder runs because it is a different
/// recording of a different thing: no buildings, no wonder, just two scene
/// loads with a trip through the main menu between them.
fn two_games() -> &'static Replay {
    static ONCE: std::sync::OnceLock<Replay> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let all = Scenario::all("two-games").unwrap_or_else(|e| panic!("{e}"));
        let (name, dirs) = all.first().expect("a two-games recording");
        replay(name, dirs)
    })
}

/// The second game's container is found through the reference table, not by
/// sweeping for it.
///
/// This is the case the table was added for, and the only one no other
/// recording covers. The sweep it replaces is at its worst here: the process
/// grew from 3916 MiB to 5195 MiB between the two games, so the scan a runner
/// pays for gets slower with every attempt of the session.
#[test]
fn the_second_game_costs_no_sweep() {
    let run = two_games();
    let containers: Vec<&String> = run
        .log
        .iter()
        .filter(|line| line.contains("The game's singleton container is at"))
        .collect();
    assert_eq!(
        containers.len(),
        2,
        "{}: expected a container per game, got {containers:?}",
        run.name
    );
    assert!(
        containers[0] != containers[1],
        "{}: both games resolved the same container: {containers:?}",
        run.name
    );
    assert!(
        !run.log
            .iter()
            .any(|line| line.contains("[scan] SingletonRepository starting")),
        "{}: swept for a container despite a table being found; log was {:#?}",
        run.name,
        run.log
    );
}

/// The second game is free: not one sweep after the first scene load ends.
///
/// The first game is not quite, and the reason is worth recording. Sweeping
/// for the anchor is by design -- there is nothing to find the table with until
/// something has been found the hard way. The other sweep is the run start's,
/// and it happens because during the *first* load the incoming
/// `GameInitializer` is on the heap before the runtime has a reference to it:
/// the table reported "0 live instances" while a sweep found it. By the second
/// load there are two of them in the table and no sweep happens at all.
///
/// So the fallback is not decoration. It covers a real window, once per
/// session, at main-menu prices -- 894 MiB rather than the 5195 MiB the same
/// sweep would cost by the second game.
#[test]
fn the_second_game_costs_nothing_at_all() {
    let run = two_games();
    let second_load = run
        .log
        .iter()
        .rposition(|line| line.contains("A scene is loading"))
        .unwrap_or_else(|| panic!("{}: no second load in the log", run.name));
    let after: Vec<&String> = run.log[second_load..]
        .iter()
        .filter(|line| line.contains("starting (full sweep"))
        .collect();
    assert!(
        after.is_empty(),
        "{}: the second game still swept: {after:?}",
        run.name
    );
}

/// The sweeps the first game does pay for are the two named above, and no
/// others. A third means something stopped being findable through the table.
#[test]
fn the_first_game_sweeps_only_for_the_anchor_and_the_run_start() {
    let run = two_games();
    let swept: Vec<&str> = run
        .log
        .iter()
        .filter_map(|line| line.strip_prefix("[scan] "))
        .filter(|line| line.contains("starting (full sweep"))
        .filter_map(|line| line.split_once(' ').map(|(name, _)| name))
        .collect();
    assert_eq!(
        swept,
        ["SceneLoader", "GameInitializer"],
        "{}: unexpected sweeps; log was {:#?}",
        run.name,
        run.log
    );
}

/// Both games start their timer, which is what the table has to not break.
#[test]
fn both_games_bind_a_run_start() {
    let run = two_games();
    let bound = run
        .log
        .iter()
        .filter(|line| line.contains("run start bound during the load"))
        .count();
    assert_eq!(
        bound, 2,
        "{}: expected a run start bound in each game; log was {:#?}",
        run.name, run.log
    );
}
