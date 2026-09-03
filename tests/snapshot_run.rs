//! The splitter driven against a capture of a *finished* run.
//!
//! Characterization, not specification: these say what the splitter did against
//! build 25096761 as it actually sat in memory. A game update can turn one red
//! with nothing wrong in the code, which is why they are behind
//! `--features snapshot-tests` — `cargo snapshot-tests`.
//!
//! The capture is one instant of a save where everything has already happened:
//! every tracked building finished, the wonder unlocked and launched, the
//! Congratulations screen already shown. So what it exercises is not a run but
//! the **attach-mid-run** path — the splitter reconstructing what it missed and
//! declining to start a timer for a run that is already over.

use std::sync::OnceLock;

use test_harness::{snapshot::Snapshot, timer::TimerEvent, World};

/// The state these need, by description rather than by filename.
const RUN_FINISHED: &str = "run-finished";

/// Ticks to run. The three full heap sweeps are budget-limited to 32 MiB each,
/// and the capture holds 4678 MiB of scannable range, so this is a few hundred
/// ticks of scanning before anything interesting is reachable.
const TICKS: usize = 2000;

/// Driven once and shared: a pass costs ~25s, nearly all of it sweeping the
/// heap, and paying that per test would make the suite unusable.
struct Outcome {
    log: Vec<String>,
    timer: Vec<TimerEvent>,
}

fn finished_run() -> &'static Outcome {
    static OUTCOME: OnceLock<Outcome> = OnceLock::new();
    OUTCOME.get_or_init(|| {
        let dir =
            test_harness::snapshot::find(RUN_FINISHED, None).unwrap_or_else(|e| panic!("{e}"));
        let snapshot = Snapshot::open(&dir).expect("opening the snapshot");
        let world = World::new().with_process(snapshot.process());
        let world = test_harness::drive(world, timberborn_autosplitter::main(), TICKS);
        Outcome {
            log: world.log,
            timer: world.timer.events,
        }
    })
}

fn logged(needle: &str) -> bool {
    finished_run().log.iter().any(|line| line.contains(needle))
}

fn require(needle: &str) {
    assert!(
        logged(needle),
        "expected a log line containing {needle:?}; log was {:#?}",
        finished_run().log
    );
}

/// The runtime half of the version check, which until now had only ever been
/// run by hand with the game open. `metadata.py check` answers the same
/// question offline; this answers it against real memory, including the vtables
/// that only exist once a class has been constructed.
#[test]
fn every_name_the_design_depends_on_resolves() {
    require("probe: ALL RESOLVED");
    assert!(!logged("MISSING"), "log was {:#?}", finished_run().log);
}

/// The whole path: scan, container, services. Reaching the probe at all means
/// each of these worked.
#[test]
fn finds_the_games_singleton_container_and_services() {
    require("The game's singleton container is at");
    require("Found DayNightCycle at");
    require("Watching wonder completion at");
    require("Watching building unlocks at");
}

/// Attaching to a run already in progress must not start a timer. The runner
/// would see a run that silently began at the wrong time, which is the exact
/// failure this splitter exists to remove.
#[test]
fn refuses_to_start_a_timer_for_a_run_already_over() {
    require("Not starting the timer");
    require("Game already in progress");

    let controlling: Vec<_> = finished_run()
        .timer
        .iter()
        .filter(|e| !matches!(e, TimerEvent::SetVariable { .. }))
        .collect();
    assert!(
        controlling.is_empty(),
        "nothing should reach the timer; got {controlling:#?}"
    );
}

/// Refusing is only half of it: the runner has to be *told*, in the status
/// variable rather than only in a log nobody reads.
#[test]
fn tells_the_runner_why_through_the_status_variable() {
    let said = finished_run().timer.iter().any(|e| {
        matches!(e, TimerEvent::SetVariable { key, value }
            if key == "Timberborn Autosplitter" && value.contains("already in progress"))
    });
    assert!(said, "timer events were {:#?}", finished_run().timer);
}

/// State from before the splitter attached has to be reconstructed, or the
/// first poll would read every already-true condition as having just happened
/// and fire every split at once.
#[test]
fn reconstructs_what_it_missed() {
    require("Wonder already unlocked in this save: true");
    require("Already finished in this save: true");

    // Against the one line that lists them, not against the log as a whole:
    // "Forester" appears in several lines, so a bare substring search over
    // everything would pass without the reconstruction having happened.
    let listed = finished_run()
        .log
        .iter()
        .find(|line| line.contains("Already finished in this save: Forester"))
        .unwrap_or_else(|| panic!("log was {:#?}", finished_run().log));
    for building in [
        "Forester",
        "Gear Workshop",
        "Tapper's Shack",
        "Observatory / Numbercruncher",
        "Smelter",
        "Wood Workshop",
    ] {
        assert!(
            listed.contains(building),
            "{building:?} missing from {listed:?}"
        );
    }
}

/// Buildings are found through the global entity registry rather than the
/// districts' finished-building registries, which do not list every building.
/// That bug was silent and cost real debugging; a walk that finds nothing would
/// look like a game with nothing built.
#[test]
fn walks_the_global_entity_registry() {
    require("[entities] opening walk starting over");
    require("entities for 6 tracked buildings");
}

/// The map is what makes later scans cheap, and it is only worth anything if it
/// narrows the search. Asserting it found *something* rather than a particular
/// figure: the fraction is a property of the save, not of the code.
#[test]
fn maps_the_heap_to_narrow_later_scans() {
    require("[scan] heap mapped:");
    require("Later scans read only those.");
}

/// Reads either succeed or are reported. A capture with holes in it would show
/// up here rather than as a mysterious failure to find an object.
#[test]
fn the_capture_has_no_holes() {
    // Every scan reports its own read failures, so an incomplete capture shows
    // up here rather than as a mysterious failure to find an object.
    let sweeps: Vec<&String> = finished_run()
        .log
        .iter()
        .filter(|line| line.contains("chunk read failures"))
        .collect();
    assert!(
        !sweeps.is_empty(),
        "no sweep reported; log was {:#?}",
        finished_run().log
    );
    for sweep in sweeps {
        assert!(
            sweep.contains("0 chunk read failures, 0 KiB unreadable"),
            "the capture has holes in it: {sweep:?}"
        );
    }
}

/// The BCL collection readers resolve `ImmutableArray`, `Dictionary` and
/// `HashSet` field names rather than falling back to a hardcoded layout. The
/// fallback is insurance, and a silent one: it was only made to log at all
/// after a session could not tell whether it was load-bearing.
#[test]
fn reads_collections_by_name_rather_than_by_the_known_layout() {
    assert!(
        !logged("[collections]"),
        "the fallback was taken, so a field name did not resolve; log was {:#?}",
        finished_run().log
    );
}
